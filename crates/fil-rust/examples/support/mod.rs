//! Shared by the examples: compiles an example's C/C++ source with the Fil-C
//! compiler of the configured flavor and starts the runtime.
//!
//! Run the examples from `nix-shell`, which provides the compilers and the
//! distributions. `FILC_FLAVOR=glibc` selects the glibc flavor (build with
//! `--features glibc`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use fil_rust::{Config, Flavor, Libc, Runtime};

/// Starts the runtime from `FILC_FLAVOR` and the distribution root variables,
/// with the Fil-C libc, since the examples load libraries inside
/// `Runtime::run`.
pub fn runtime() -> Runtime {
    let config = Config::from_env().expect("set FILC_ROOT / FILC_GLIBC_ROOT (run in nix-shell)");
    Runtime::start(config.libc(Libc::Always)).expect("starting the Fil-C runtime")
}

/// Compiles `examples/c/<file>` into a shared library and returns its path.
pub fn build(file: &str) -> PathBuf {
    let flavor = Config::from_env().map(|c| c.flavor()).unwrap_or(Flavor::Musl);
    let cxx = file.ends_with(".cpp");
    let var = match (flavor, cxx) {
        (Flavor::Musl, false) => "FILC_CC",
        (Flavor::Musl, true) => "FILC_CXX",
        (Flavor::Glibc, false) => "FILC_GLIBC_CC",
        (Flavor::Glibc, true) => "FILC_GLIBC_CXX",
    };
    let compiler = std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set (run in nix-shell)"));

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("examples/c").join(file);
    // In the workspace's target directory.
    let out_dir = root.join("../../target/example-libs").join(format!("{flavor:?}").to_lowercase());
    std::fs::create_dir_all(&out_dir).unwrap();
    let stem = Path::new(file).file_stem().unwrap().to_str().unwrap();
    let out = out_dir.join(format!("lib{stem}.so"));
    let status = Command::new(&compiler)
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("running {compiler}: {e}"));
    assert!(status.success(), "compiling {file} failed");
    out
}
