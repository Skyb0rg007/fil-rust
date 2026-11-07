//! What the runtime's tests share.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use fil_rust_rt::abi::Ptr;
use fil_rust_rt::{Library, call, install, run};
use fil_rust_sys::Flavor;

/// Compiles a fixture the way a libc-free library is built: without the default
/// libraries, so it keeps `NEEDED libpizlo.so` but not `libc.so`.
fn fixture(file: &str) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(file);
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rt-fixtures");
    std::fs::create_dir_all(&out).unwrap();
    let library = out.join(file).with_extension("so");
    let modified = |path: &Path| std::fs::metadata(path).and_then(|metadata| metadata.modified());
    if let (Ok(source_time), Ok(library_time)) = (modified(&source), modified(&library))
        && library_time >= source_time
    {
        return library;
    }
    // Every child process of the failure tests gets here too, and may be
    // loading the library, so it has to appear whole.
    let building = library.with_extension(format!("so.{}", std::process::id()));
    let compiler = std::env::var("FILC_CC").expect("FILC_CC must be set (run the tests in nix-shell)");
    let status = Command::new(compiler)
        .args(["-shared", "-fPIC", "-O1", "-g", "-nodefaultlibs", "-o"])
        .arg(&building)
        .arg(&source)
        .status()
        .expect("running the Fil-C compiler");
    assert!(status.success(), "compiling {file} failed");
    std::fs::rename(&building, &library).unwrap();
    library
}

/// Compiles and loads a fixture.
pub fn open(file: &str) -> Library {
    install(Flavor::Musl).unwrap();
    unsafe { Library::open(&fixture(file)).unwrap() }
}

pub fn int(value: i64) -> Ptr {
    Ptr { ptr: value as *mut u8, lower: std::ptr::null_mut() }
}

/// Calls a function that returns a `long`.
pub fn call_in(library: &Library, name: &str, args: &[Ptr]) -> i64 {
    run(|thread| unsafe {
        let function = library.symbol(thread, name).unwrap();
        let mut returned = [Ptr::NULL];
        call(thread, function, args, &mut returned);
        returned[0].ptr as i64
    })
}

/// In the child, the case to run; otherwise `None`.
pub fn failing_case() -> Option<String> {
    let case = std::env::var("FIL_RUST_RT_FAILING_CASE").ok()?;
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &libc::rlimit { rlim_cur: 0, rlim_max: 0 }) };
    Some(case)
}

/// Runs one of the fixture's failing functions in a child process, and returns
/// what it reported.
pub fn failure_of(case: &str) -> String {
    let test = std::env::current_exe().unwrap();
    let output = Command::new(test)
        .args(["--exact", "failing_child", "--nocapture", "--ignored"])
        .env("FIL_RUST_RT_FAILING_CASE", case)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{case} didn't fail");
    assert_eq!(std::os::unix::process::ExitStatusExt::signal(&output.status), Some(libc::SIGABRT), "{case}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

