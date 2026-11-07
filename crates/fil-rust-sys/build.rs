//! Builds the per-flavor C support libraries that get embedded into the crate:
//!
//! * the **compat** stand-ins, which take the place of Fil-C's yolo libc
//!   (`libyoloc.so` for musl; `libyolocimpl.so`, `libyolomimpl.so` and
//!   `ld-fil1-x86_64.so` for glibc) on top of the host glibc, and
//! * the **runtime shim**, compiled against the flavor's libc headers and the
//!   Fil-C runtime headers so that it agrees with libpizlo on struct layouts, and
//! * the **runtime stub**, a `libpizlo.so` of jump slots that the Rust runtime
//!   fills in, generated from `abi/runtime_symbols_*.txt`, and
//! * **name-only** libraries with the sonames of the Fil-C libc, for running
//!   libraries that link against it but import nothing from it.
//!
//! Inputs come from environment variables that `shell.nix` sets.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn env_path(name: &str) -> PathBuf {
    println!("cargo:rerun-if-env-changed={name}");
    match env::var_os(name) {
        Some(value) => PathBuf::from(value),
        None => panic!(
            "fil-rust-sys: environment variable {name} is not set. Build inside the \
             repository's nix-shell, or set it by hand (see shell.nix)."
        ),
    }
}

fn env_or(name: &str, default: &str) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("fil-rust-sys: failed to spawn {cmd:?}: {e}"));
    if !status.success() {
        panic!("fil-rust-sys: command failed ({status}): {cmd:?}");
    }
}

fn clang_resource_dir(clang: &str) -> PathBuf {
    let out = Command::new(clang)
        .arg("-print-resource-dir")
        .output()
        .unwrap_or_else(|e| panic!("fil-rust-sys: cannot run {clang}: {e}"));
    assert!(out.status.success(), "fil-rust-sys: {clang} -print-resource-dir failed");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

/// Compiles a compat stand-in against the host glibc.
fn build_compat(cc: &str, src: &Path, out: &Path, soname: &str, version_script: Option<&Path>) {
    let mut cmd = Command::new(cc);
    cmd.args(["-O2", "-g", "-fPIC", "-shared", "-Wall", "-Wextra"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg("-o")
        .arg(out)
        .arg(src);
    if let Some(script) = version_script {
        cmd.arg(format!("-Wl,--version-script={}", script.display()));
    }
    cmd.arg("-lm");
    run(&mut cmd);
}

/// Builds a library with a soname and nothing else. Compiled libraries list
/// the Fil-C libc as a dependency even when they import nothing from it, and a
/// loaded library with its soname keeps the loader from finding the real one.
fn build_name_only(cc: &str, out: &Path, soname: &str) {
    let mut cmd = Command::new(cc);
    cmd.args(["-shared", "-nostdlib", "-x", "c", "/dev/null"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg("-o")
        .arg(out);
    run(&mut cmd);
}

/// One line of a runtime symbol list.
struct StubSymbol<'a> {
    kind: &'a str,
    name: &'a str,
}

/// The stack `filc_stack_overflow_failure` reports from. Compiled code jumps
/// there with a stack pointer that may be unusable, and reporting formats text
/// in Rust, so this is well above the 8 KiB the C runtime gets by with.
const FAILURE_STACK_SIZE: usize = 1 << 18;

/// Writes the assembly for one flavor's stub.
///
/// Every function is an indirect jump through `fr_stub_table`, which starts out
/// pointing at per-symbol failure paths, so a symbol the runtime doesn't
/// implement yet fails with its name when it's used rather than failing the
/// whole `dlopen`. A jump leaves every argument register alone, and x16/x17 are
/// free to clobber at a call on aarch64.
fn stub_assembly(arch: &str, symbols: &[StubSymbol<'_>]) -> String {
    use std::fmt::Write;

    let mut s = String::new();
    let x86 = arch == "x86_64";
    let w = &mut s;
    writeln!(w, "\t.section .note.GNU-stack,\"\",@progbits").unwrap();
    writeln!(w, "\t.text").unwrap();
    let functions: Vec<&StubSymbol<'_>> = symbols.iter().filter(|s| s.kind != "D").collect();
    for (index, symbol) in functions.iter().enumerate() {
        let name = symbol.name;
        writeln!(w, "\t.globl {name}\n\t.type {name},@function").unwrap();
        writeln!(w, "\t.p2align 4\n{name}:").unwrap();
        if name == "filc_stack_overflow_failure" {
            // Don't use the stack: it may be what overflowed.
            if x86 {
                writeln!(w, "\tmovb $1, %cl\n1:\txorl %eax, %eax").unwrap();
                writeln!(w, "\tlock cmpxchgb %cl, .Lfailure_lock(%rip)\n\tjne 1b").unwrap();
                writeln!(w, "\tleaq .Lfailure_stack+{FAILURE_STACK_SIZE}(%rip), %rsp").unwrap();
                writeln!(w, "\tcallq *.Ltable+{}(%rip)\n\tud2", index * 8).unwrap();
            } else {
                writeln!(w, "\tadrp x16, .Lfailure_lock\n\tadd x16, x16, :lo12:.Lfailure_lock").unwrap();
                writeln!(w, "\tmov w1, #1\n1:\tldaxrb w17, [x16]\n\tcbnz w17, 1b").unwrap();
                writeln!(w, "\tstlxrb w17, w1, [x16]\n\tcbnz w17, 1b").unwrap();
                writeln!(w, "\tadrp x0, .Lfailure_stack+{FAILURE_STACK_SIZE}").unwrap();
                writeln!(w, "\tadd x0, x0, :lo12:.Lfailure_stack+{FAILURE_STACK_SIZE}\n\tmov sp, x0").unwrap();
                writeln!(w, "\tadrp x16, .Ltable\n\tadd x16, x16, :lo12:.Ltable").unwrap();
                writeln!(w, "\tldr x16, [x16, #{}]\n\tblr x16\n\tbrk #1", index * 8).unwrap();
            }
        } else if x86 {
            writeln!(w, "\tjmpq *.Ltable+{}(%rip)", index * 8).unwrap();
        } else {
            writeln!(w, "\tadrp x16, .Ltable\n\tadd x16, x16, :lo12:.Ltable").unwrap();
            writeln!(w, "\tldr x16, [x16, #{}]\n\tbr x16", index * 8).unwrap();
        }
        writeln!(w, "\t.size {name}, .-{name}").unwrap();

        // The failure paths: a helper reports `name` and dies; a getter hands
        // back a function that does so when it's called, since looking a
        // symbol up doesn't mean it will be used.
        writeln!(w, ".Lmissing_{index}:").unwrap();
        let (register, slot) = match (symbol.kind, x86) {
            ("H", true) => ("%rdi", ".Lmissing_helper"),
            ("H", false) => ("x0", ".Lmissing_helper"),
            (_, true) => ("%rdx", ".Lmissing_native"),
            (_, false) => ("x2", ".Lmissing_native"),
        };
        if x86 {
            writeln!(w, "\tleaq .Lname_{index}(%rip), {register}\n\tjmpq *{slot}(%rip)").unwrap();
        } else {
            writeln!(w, "\tadrp {register}, .Lname_{index}\n\tadd {register}, {register}, :lo12:.Lname_{index}").unwrap();
            writeln!(w, "\tadrp x16, {slot}\n\tldr x16, [x16, :lo12:{slot}]\n\tbr x16").unwrap();
        }
    }

    writeln!(w, "\t.section .rodata").unwrap();
    for (index, symbol) in functions.iter().enumerate() {
        writeln!(w, ".Lname_{index}:\n\t.asciz \"{}\"", symbol.name).unwrap();
    }

    writeln!(w, "\t.data\n\t.p2align 3").unwrap();
    writeln!(w, "\t.globl fr_stub_table\n\t.type fr_stub_table,@object").unwrap();
    writeln!(w, "fr_stub_table:\n.Ltable:").unwrap();
    for index in 0..functions.len() {
        writeln!(w, "\t.quad .Lmissing_{index}").unwrap();
    }
    writeln!(w, "\t.size fr_stub_table, .-fr_stub_table").unwrap();
    writeln!(w, "\t.globl fr_stub_names\n\t.type fr_stub_names,@object\nfr_stub_names:").unwrap();
    for index in 0..functions.len() {
        writeln!(w, "\t.quad .Lname_{index}").unwrap();
    }
    writeln!(w, "\t.size fr_stub_names, .-fr_stub_names").unwrap();
    writeln!(w, "\t.globl fr_stub_count\n\t.type fr_stub_count,@object\nfr_stub_count:").unwrap();
    writeln!(w, "\t.quad {}\n\t.size fr_stub_count, 8", functions.len()).unwrap();
    writeln!(w, "\t.globl fr_stub_missing_helper\n\t.type fr_stub_missing_helper,@object").unwrap();
    writeln!(w, "fr_stub_missing_helper:\n.Lmissing_helper:\n\t.quad 0\n\t.size fr_stub_missing_helper, 8").unwrap();
    writeln!(w, "\t.globl fr_stub_missing_native\n\t.type fr_stub_missing_native,@object").unwrap();
    writeln!(w, "fr_stub_missing_native:\n.Lmissing_native:\n\t.quad 0\n\t.size fr_stub_missing_native, 8").unwrap();
    for symbol in symbols.iter().filter(|s| s.kind == "D") {
        let name = symbol.name;
        writeln!(w, "\t.globl {name}\n\t.type {name},@object\n\t.p2align 3").unwrap();
        writeln!(w, "{name}:\n\t.zero 8\n\t.size {name}, 8").unwrap();
    }
    writeln!(w, ".Lfailure_lock:\n\t.byte 0").unwrap();
    writeln!(w, "\t.bss\n\t.p2align 4\n.Lfailure_stack:\n\t.zero {FAILURE_STACK_SIZE}").unwrap();
    s
}

/// Builds a flavor's `libpizlo.so` stub. It links against nothing: the symbols
/// are all its own, and `cc -nostdlib` keeps it from picking up a libc.
fn build_stub(cc: &str, arch: &str, list: &Path, out_dir: &Path, stem: &str) {
    let text = std::fs::read_to_string(list)
        .unwrap_or_else(|e| panic!("fil-rust-sys: reading {}: {e}", list.display()));
    let symbols: Vec<StubSymbol<'_>> = text
        .lines()
        .map(|line| {
            let (kind, name) = line.split_once(' ').expect("`<kind> <symbol>` lines");
            StubSymbol { kind, name }
        })
        .collect();
    let source = out_dir.join(format!("{stem}.S"));
    std::fs::write(&source, stub_assembly(arch, &symbols)).unwrap();
    let mut cmd = Command::new(cc);
    cmd.args(["-shared", "-nostdlib", "-fPIC", "-Wl,-soname,libpizlo.so", "-Wl,-z,noexecstack"])
        .arg("-o")
        .arg(out_dir.join(format!("{stem}.so")))
        .arg(&source);
    run(&mut cmd);
}

/// Builds the part of the runtime that's Fil-C code itself, from the objects
/// `libpizlo.a` has for it. Nothing here comes from libpas, and it has no
/// constructors, so loading it costs nothing. Without an archive, as the glibc
/// flavor is, this writes an empty file and the runtime goes without.
fn build_support(cc: &str, archive: Option<PathBuf>, out_dir: &Path, stem: &str) {
    let output = out_dir.join(format!("{stem}.so"));
    let Some(archive) = archive.filter(|archive| archive.exists()) else {
        std::fs::write(&output, []).unwrap();
        return;
    };
    println!("cargo:rerun-if-changed={}", archive.display());
    let ar = env_or("AR", "ar");
    let members = Command::new(&ar).arg("t").arg(&archive).output().expect("running ar");
    let members = String::from_utf8(members.stdout).unwrap();
    let objects: Vec<&str> = members.lines().filter(|member| member.starts_with("fil-pizlo-")).collect();
    let directory = out_dir.join(stem);
    std::fs::create_dir_all(&directory).unwrap();
    run(Command::new(&ar).arg("x").arg(&archive).args(&objects).current_dir(&directory));
    let mut cmd = Command::new(cc);
    cmd.args(["-shared", "-nostdlib", "-Wl,-soname,libfilrust_support.so", "-Wl,-z,noexecstack", "-o"]).arg(&output);
    for object in &objects {
        cmd.arg(directory.join(object));
    }
    run(&mut cmd);
}

struct ShimInputs {
    clang: String,
    resource_dir: PathBuf,
    runtime_src: PathBuf,
    /// `--target` for clang.
    triple: String,
    arch: String,
    /// A linker for the target, if clang's default won't do (cross builds).
    linker: Option<String>,
}

/// Compiles the runtime shim against the given libc headers.
fn build_shim(inputs: &ShimInputs, libc_includes: &[PathBuf], out: &Path) {
    let mut cmd = Command::new(&inputs.clang);
    cmd.arg(format!("--target={}", inputs.triple));
    if inputs.arch == "x86_64" {
        cmd.arg("-march=x86-64-v2");
    }
    if let Some(linker) = &inputs.linker {
        cmd.arg(format!("--ld-path={linker}"));
    }
    cmd.args([
        "-O2",
        "-g",
        "-fPIC",
        "-shared",
        "-pthread",
        "-nostdinc",
        "-nostdlib",
        "-fno-strict-aliasing",
        "-Wall",
        "-Wno-unused-function",
        "-Wno-unused-variable",
        "-DPAS_FILC=1",
        "-Wl,-soname,libfilrust_shim.so",
    ]);
    for dir in libc_includes {
        cmd.arg("-isystem").arg(dir);
    }
    cmd.arg("-isystem")
        .arg(inputs.resource_dir.join("include"))
        .arg("-I")
        .arg(&inputs.runtime_src)
        .arg("-o")
        .arg(out)
        .arg("csrc/shim.c");
    run(&mut cmd);
}

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=abi");
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !(arch == "x86_64" || arch == "aarch64") || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        panic!("fil-rust-sys: only x86-64 and aarch64 Linux targets are supported (building for {target})");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // FILC_TARGET_CC: a C compiler for the target, when cross-compiling.
    let cc = env::var("FILC_TARGET_CC").unwrap_or_else(|_| env_or("CC", "cc"));
    println!("cargo:rerun-if-env-changed=FILC_TARGET_CC");
    println!("cargo:rerun-if-env-changed=FILC_SHIM_LINKER");
    let musl = env::var_os("CARGO_FEATURE_MUSL").is_some();
    let glibc = env::var_os("CARGO_FEATURE_GLIBC").is_some();
    if !musl && !glibc {
        panic!("fil-rust-sys: enable at least one of the `musl` and `glibc` features");
    }

    let clang = env_or("FILC_HOST_CLANG", "clang");
    let inputs = ShimInputs {
        resource_dir: clang_resource_dir(&clang),
        clang,
        runtime_src: env_path("FILC_RUNTIME_SRC"),
        triple: format!("{arch}-unknown-linux-gnu"),
        arch: arch.clone(),
        linker: env::var("FILC_SHIM_LINKER").ok(),
    };
    let linux_include = env_path("FILC_LINUX_INCLUDE");

    if musl {
        build_compat(
            &cc,
            Path::new("csrc/compat_musl.c"),
            &out_dir.join("compat_musl_libyoloc.so"),
            "libyoloc.so",
            None,
        );
        build_stub(&cc, &arch, Path::new("abi/runtime_symbols_musl.txt"), &out_dir, "stub_musl");
        build_name_only(&cc, &out_dir.join("name_only_libc.so"), "libc.so");
        println!("cargo:rerun-if-env-changed=FILC_ROOT");
        let archive = env::var_os("FILC_ROOT").map(|root| Path::new(&root).join("pizfix/lib/libpizlo.a"));
        build_support(&cc, archive, &out_dir, "support_musl");
        let musl_include = env_path("FILC_MUSL_INCLUDE");
        build_shim(
            &inputs,
            &[musl_include, linux_include.clone()],
            &out_dir.join("shim_musl.so"),
        );
    }

    if glibc && arch != "x86_64" {
        panic!("fil-rust-sys: the glibc flavor's distribution exists only for x86-64");
    }
    if glibc {
        for (src, out, soname, map) in [
            ("compat_glibc_c.c", "compat_glibc_libyolocimpl.so", "libyolocimpl.so", "compat_glibc_c.map"),
            ("compat_glibc_m.c", "compat_glibc_libyolomimpl.so", "libyolomimpl.so", "compat_glibc_m.map"),
            ("compat_glibc_ld.c", "compat_glibc_ld.so", "ld-fil1-x86_64.so", "compat_glibc_ld.map"),
        ] {
            build_compat(
                &cc,
                &Path::new("csrc").join(src),
                &out_dir.join(out),
                soname,
                Some(&Path::new("csrc").join(map)),
            );
        }
        build_compat(
            &cc,
            Path::new("csrc/compat_glibc_pizlo.c"),
            &out_dir.join("compat_glibc_pizlo.so"),
            "libpizlo.so",
            None,
        );
        build_stub(&cc, &arch, Path::new("abi/runtime_symbols_glibc.txt"), &out_dir, "stub_glibc");
        for (out, soname) in [
            ("name_only_libc_6666.so", "libc.so.6666"),
            ("name_only_libm_6666.so", "libm.so.6666"),
            ("name_only_libdl_6662.so", "libdl.so.6662"),
            ("name_only_libpthread_6660.so", "libpthread.so.6660"),
        ] {
            build_name_only(&cc, &out_dir.join(out), soname);
        }
        // The glibc flavor of the runtime is built against (yolo) glibc headers,
        // which the host glibc's headers match. Ask the host compiler for them.
        let glibc_include = env_path("FILC_GLIBC_INCLUDE");
        build_shim(
            &inputs,
            &[glibc_include, linux_include],
            &out_dir.join("shim_glibc.so"),
        );
    }
}
