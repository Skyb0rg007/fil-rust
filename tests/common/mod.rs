//! Shared test support: builds C fixtures with the Fil-C compiler of the
//! configured flavor and starts the runtime.

#![allow(dead_code)]

pub mod basic;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use fil_rust::{Config, Flavor, Runtime};

static BUILD_LOCK: Mutex<()> = Mutex::new(());

pub fn flavor() -> Flavor {
    match std::env::var("FILC_FLAVOR").as_deref() {
        Ok("glibc") => Flavor::Glibc,
        _ => Flavor::Musl,
    }
}

fn compiler(cxx: bool) -> String {
    let var = match (flavor(), cxx) {
        (Flavor::Musl, false) => "FILC_CC",
        (Flavor::Musl, true) => "FILC_CXX",
        (Flavor::Glibc, false) => "FILC_GLIBC_CC",
        (Flavor::Glibc, true) => "FILC_GLIBC_CXX",
    };
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set (run the tests in nix-shell)"))
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixtures")
        .join(format!("{:?}", flavor()).to_lowercase());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn newer(src: &Path, out: &Path) -> bool {
    let (Ok(s), Ok(o)) = (std::fs::metadata(src), std::fs::metadata(out)) else { return true };
    s.modified().unwrap() > o.modified().unwrap()
}

/// Compiles `tests/fixtures/<file>` into a shared library and returns its
/// path. Extra arguments go to the compiler.
pub fn fixture_with(file: &str, extra: &[&str]) -> PathBuf {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = fixtures_dir().join(file);
    let stem = Path::new(file).file_stem().unwrap().to_str().unwrap();
    let out = out_dir().join(format!("lib{stem}.so"));
    if newer(&src, &out) {
        let cxx = file.ends_with(".cpp");
        let tmp = out.with_extension(format!("so.{}", std::process::id()));
        let status = Command::new(compiler(cxx))
            .args(["-shared", "-fPIC", "-O1", "-g", "-o"])
            .arg(&tmp)
            .arg(&src)
            .args(extra)
            .status()
            .expect("running the Fil-C compiler");
        assert!(status.success(), "compiling {file} failed");
        std::fs::rename(&tmp, &out).unwrap();
    }
    out
}

pub fn fixture(file: &str) -> PathBuf {
    fixture_with(file, &[])
}

/// Compiles a Fil-C executable (for subprocess tests).
pub fn fixture_exe(file: &str) -> PathBuf {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = fixtures_dir().join(file);
    let stem = Path::new(file).file_stem().unwrap().to_str().unwrap();
    let out = out_dir().join(stem);
    if newer(&src, &out) {
        let status = Command::new(compiler(file.ends_with(".cpp")))
            .args(["-O1", "-g", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("running the Fil-C compiler");
        assert!(status.success(), "compiling {file} failed");
    }
    out
}

pub fn config() -> Config {
    Config::from_env_for(flavor()).expect("Fil-C distribution configured in the environment")
}

/// The runtime, started on first use.
pub fn runtime() -> Runtime {
    // Debugging aid: let a debugger attach to a hung test process.
    if std::env::var_os("FIL_RUST_ALLOW_PTRACE").is_some() {
        unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY) };
    }
    Runtime::start(config()).expect("starting the Fil-C runtime")
}

/// The `basic.c` fixture, loaded once per test process.
///
/// Call this *outside* `Runtime::run`: blocking on the `OnceLock` inside a
/// job would block a thread that is inside the Fil-C runtime, which stalls
/// the garbage collector (see "Threads and blocking" in the crate docs).
pub fn basic() -> &'static basic::Basic {
    static BASIC: std::sync::OnceLock<basic::Basic> = std::sync::OnceLock::new();
    BASIC.get_or_init(|| runtime().run(basic::load))
}
