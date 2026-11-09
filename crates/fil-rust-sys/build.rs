//! Builds the per-flavor C support libraries that get embedded into the crate:
//!
//! * the **compat** stand-ins, which take the place of Fil-C's yolo libc
//!   (`libyoloc.so` for musl; `libyolocimpl.so`, `libyolomimpl.so` and
//!   `ld-fil1-x86_64.so` for glibc) on top of the host glibc, and
//! * the **runtime stub**, a `libpizlo.so` of jump slots that the Rust runtime
//!   fills in, generated from `abi/runtime_symbols_*.txt`,
//! * stand-ins for the **Fil-C libc**: for glibc, one per soname, with a jump
//!   slot under every symbol and version the real one has, which the Rust
//!   runtime fills in; for musl, a library with the soname and nothing else,
//!   for libraries that link against the libc but import nothing from it, and
//! * the **support** library, the Fil-C-compiled part of the runtime: for
//!   musl, linked from the distribution's `libpizlo.a`; for glibc, compiled
//!   from the vendored `csrc/filc`.
//!
//! Inputs come from environment variables that `shell.nix` sets.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// One export of a glibc-flavor libc soname: a getter, under every version the
/// distribution's library has it.
struct Export<'a> {
    name: &'a str,
    /// A variable's getter, which fails as it runs if there's nothing behind
    /// it; a function's hands back a function that fails when it's called.
    data: bool,
    /// Each version, and whether it's the default one (`@@`).
    versions: Vec<(&'a str, bool)>,
}

/// Writes the assembly and version script for one libc stand-in, from its
/// `abi/glibc_<soname>.txt`.
///
/// Every export is one jump through `fr_stub_table`, as in the runtime stub,
/// under global aliases that `.symver` gives each version, since a library
/// asks for a symbol at a version and the loader won't bind it otherwise. The
/// version script has every node of the real library, in order, and lists the
/// default names under theirs. It makes only the aliases local: a plain
/// `local: *` would hide the non-default versions too.
fn stand_in_sources(list: &str) -> (String, String) {
    use std::fmt::Write;

    let mut nodes: Vec<&str> = Vec::new();
    let mut exports: Vec<Export<'_>> = Vec::new();
    for line in list.lines() {
        let (kind, rest) = line.split_once(' ').expect("`<kind> <rest>` lines");
        if kind == "V" {
            nodes.push(rest);
            continue;
        }
        let (name, default, version) = match rest.split_once("@@") {
            Some((name, version)) => (name, true, version),
            None => {
                let (name, version) = rest.split_once('@').expect("`name@version`");
                (name, false, version)
            }
        };
        let data = match kind {
            "F" => false,
            "D" => true,
            _ => panic!("fil-rust-sys: unknown kind {kind} in a libc symbol list"),
        };
        match exports.last_mut() {
            Some(last) if last.name == name => last.versions.push((version, default)),
            _ => exports.push(Export { name, data, versions: vec![(version, default)] }),
        }
    }

    let mut s = String::new();
    let w = &mut s;
    writeln!(w, "\t.section .note.GNU-stack,\"\",@progbits\n\t.text").unwrap();
    let mut defaults: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (index, export) in exports.iter().enumerate() {
        writeln!(w, "\t.globl fr_i{index}\n\t.type fr_i{index},@function\n\t.p2align 4").unwrap();
        writeln!(w, "fr_i{index}:\n\tjmpq *.Ltable+{}(%rip)\n\t.size fr_i{index}, .-fr_i{index}", index * 8).unwrap();
        for &(version, default) in &export.versions {
            let alias = if default {
                defaults.entry(version).or_default().push(export.name);
                format!("fr_d{index}")
            } else {
                format!("fr_v{index}_{}", version.replace('.', "_"))
            };
            let at = if default { "@@" } else { "@" };
            writeln!(w, "\t.globl {alias}\n\t.set {alias}, fr_i{index}").unwrap();
            writeln!(w, "\t.symver {alias}, {}{at}{version}", export.name).unwrap();
        }
        let slot = if export.data { ".Lmissing_data" } else { ".Lmissing_native" };
        writeln!(w, ".Lmissing_{index}:\n\tleaq .Lname_{index}(%rip), %rdx\n\tjmpq *{slot}(%rip)").unwrap();
    }
    writeln!(w, "\t.section .rodata").unwrap();
    for (index, export) in exports.iter().enumerate() {
        writeln!(w, ".Lname_{index}:\n\t.asciz \"{}\"", export.name).unwrap();
    }
    writeln!(w, "\t.data\n\t.p2align 3").unwrap();
    writeln!(w, "\t.globl fr_stub_table\n\t.type fr_stub_table,@object\nfr_stub_table:\n.Ltable:").unwrap();
    for index in 0..exports.len() {
        writeln!(w, "\t.quad .Lmissing_{index}").unwrap();
    }
    writeln!(w, "\t.size fr_stub_table, .-fr_stub_table").unwrap();
    writeln!(w, "\t.globl fr_stub_names\n\t.type fr_stub_names,@object\nfr_stub_names:").unwrap();
    for index in 0..exports.len() {
        writeln!(w, "\t.quad .Lname_{index}").unwrap();
    }
    writeln!(w, "\t.size fr_stub_names, .-fr_stub_names").unwrap();
    writeln!(w, "\t.globl fr_stub_count\n\t.type fr_stub_count,@object").unwrap();
    writeln!(w, "fr_stub_count:\n\t.quad {}\n\t.size fr_stub_count, 8", exports.len()).unwrap();
    for (symbol, label) in [("fr_stub_missing_native", ".Lmissing_native"), ("fr_stub_missing_data", ".Lmissing_data")] {
        writeln!(w, "\t.globl {symbol}\n\t.type {symbol},@object").unwrap();
        writeln!(w, "{symbol}:\n{label}:\n\t.quad 0\n\t.size {symbol}, 8").unwrap();
    }

    let mut script = String::new();
    let mut previous: Option<&str> = None;
    for &node in &nodes {
        let mut globals: Vec<&str> = defaults.get(node).cloned().unwrap_or_default();
        if previous.is_none() {
            globals.extend([
                "fr_stub_table",
                "fr_stub_names",
                "fr_stub_count",
                "fr_stub_missing_native",
                "fr_stub_missing_data",
            ]);
        }
        write!(script, "{node} {{").unwrap();
        if !globals.is_empty() {
            write!(script, " global:").unwrap();
            for global in globals {
                write!(script, " {global};").unwrap();
            }
        }
        if previous.is_none() {
            write!(script, " local: fr_i*; fr_d*; fr_v*;").unwrap();
        }
        match previous {
            Some(previous) => writeln!(script, " }} {previous};").unwrap(),
            None => writeln!(script, " }};").unwrap(),
        }
        previous = Some(node);
    }
    (s, script)
}

/// Builds a glibc-flavor libc stand-in under the real one's soname.
fn build_stand_in(cc: &str, soname: &str, out_dir: &Path, out: &str) {
    let list = format!("abi/glibc_{soname}.txt");
    let text = std::fs::read_to_string(&list).unwrap_or_else(|e| panic!("fil-rust-sys: reading {list}: {e}"));
    let (assembly, script) = stand_in_sources(&text);
    let stem = out.trim_end_matches(".so");
    let source = out_dir.join(format!("{stem}.S"));
    let map = out_dir.join(format!("{stem}.map"));
    std::fs::write(&source, assembly).unwrap();
    std::fs::write(&map, script).unwrap();
    let mut cmd = Command::new(cc);
    cmd.args(["-shared", "-nostdlib", "-fPIC", "-Wl,-z,noexecstack"])
        .arg(format!("-Wl,-soname,{soname}"))
        .arg(format!("-Wl,--version-script={}", map.display()))
        .arg("-o")
        .arg(out_dir.join(out))
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

/// Builds the support library from the vendored `csrc/filc`, with a Fil-C
/// compiler (the glibc distribution has no `libpizlo.a` to take it from).
fn build_support_from_source(filcc: &str, out_dir: &Path, stem: &str) {
    const SOURCES: [&str; 5] = ["gcc_personality", "pizlonated_eh_landing_pad", "runtime", "snprintf", "unwind"];
    let directory = out_dir.join(stem);
    std::fs::create_dir_all(&directory).unwrap();
    let mut link = Command::new(filcc);
    link.args(["-shared", "-nostdlib", "-Wl,-soname,libfilrust_support.so", "-Wl,-z,noexecstack", "-o"])
        .arg(out_dir.join(format!("{stem}.so")));
    for source in SOURCES {
        let object = directory.join(format!("{source}.o"));
        run(Command::new(filcc)
            .args(["-O3", "-g", "-fPIC", "-w", "-c"])
            .arg(Path::new("csrc/filc").join(format!("{source}.c")))
            .arg("-o")
            .arg(&object));
        link.arg(object);
    }
    run(&mut link);
}

/// compiler-rt's processor detection, linked into the runtime for
/// `__builtin_cpu_supports` and `__builtin_cpu_is`.
fn build_cpu_model(cc: &str, out_dir: &Path) {
    let object = out_dir.join("cpu_model.o");
    run(Command::new(cc).args(["-O2", "-fPIC", "-c", "csrc/cpu_model.c", "-o"]).arg(&object));
    let archive = out_dir.join("libfr_cpu_model.a");
    let _ = std::fs::remove_file(&archive);
    println!("cargo:rerun-if-env-changed=AR");
    run(Command::new(env_or("AR", "ar")).arg("crs").arg(&archive).arg(&object));
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=fr_cpu_model");
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
    let musl = env::var_os("CARGO_FEATURE_MUSL").is_some();
    let glibc = env::var_os("CARGO_FEATURE_GLIBC").is_some();
    if !musl && !glibc {
        panic!("fil-rust-sys: enable at least one of the `musl` and `glibc` features");
    }

    if arch == "x86_64" {
        build_cpu_model(&cc, &out_dir);
    }

    if musl {
        build_name_only(&cc, &out_dir.join("compat_musl_libyoloc.so"), "libyoloc.so");
        build_stub(&cc, &arch, Path::new("abi/runtime_symbols_musl.txt"), &out_dir, "stub_musl");
        build_name_only(&cc, &out_dir.join("name_only_libc.so"), "libc.so");
        println!("cargo:rerun-if-env-changed=FILC_ROOT");
        let archive = env::var_os("FILC_ROOT").map(|root| Path::new(&root).join("pizfix/lib/libpizlo.a"));
        build_support(&cc, archive, &out_dir, "support_musl");
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
        build_stub(&cc, &arch, Path::new("abi/runtime_symbols_glibc.txt"), &out_dir, "stub_glibc");
        for (soname, out) in [
            ("libc.so.6666", "stand_in_libc_6666.so"),
            ("libm.so.6666", "stand_in_libm_6666.so"),
            ("libdl.so.6662", "stand_in_libdl_6662.so"),
            ("libpthread.so.6660", "stand_in_libpthread_6660.so"),
        ] {
            build_stand_in(&cc, soname, &out_dir, out);
        }
        let filcc = env::var("FILC_GLIBC_CC").expect("fil-rust-sys: FILC_GLIBC_CC must be set for the glibc flavor");
        println!("cargo:rerun-if-env-changed=FILC_GLIBC_CC");
        build_support_from_source(&filcc, &out_dir, "support_glibc");
    }
}
