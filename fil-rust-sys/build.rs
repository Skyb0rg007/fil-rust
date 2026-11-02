//! Builds the per-flavor C support libraries that get embedded into the crate:
//!
//! * the **compat** stand-ins, which take the place of Fil-C's yolo libc
//!   (`libyoloc.so` for musl; `libyolocimpl.so`, `libyolomimpl.so` and
//!   `ld-fil1-x86_64.so` for glibc) on top of the host glibc, and
//! * the **runtime shim**, compiled against the flavor's libc headers and the
//!   Fil-C runtime headers so that it agrees with libpizlo on struct layouts.
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

struct ShimInputs {
    clang: String,
    resource_dir: PathBuf,
    runtime_src: PathBuf,
}

/// Compiles the runtime shim against the given libc headers.
fn build_shim(inputs: &ShimInputs, libc_includes: &[PathBuf], out: &Path) {
    let mut cmd = Command::new(&inputs.clang);
    cmd.args([
        "-O2",
        "-g",
        "-fPIC",
        "-shared",
        "-march=x86-64-v2",
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
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.starts_with("x86_64-") || !target.contains("-linux-") {
        panic!("fil-rust-sys: only x86-64 Linux targets are supported (building for {target})");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let cc = env_or("CC", "cc");
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
        let musl_include = env_path("FILC_MUSL_INCLUDE");
        build_shim(
            &inputs,
            &[musl_include, linux_include.clone()],
            &out_dir.join("shim_musl.so"),
        );
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
