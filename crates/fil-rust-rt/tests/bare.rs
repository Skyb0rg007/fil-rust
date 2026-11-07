//! Fil-C libraries running on the Rust runtime, with no libpizlo, no libc and
//! no thread but the caller's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use fil_rust_rt::abi::Ptr;
use fil_rust_rt::{Library, call, install, run};
use fil_rust_sys::{Flavor, loaded_objects};

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

fn bare() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        install(Flavor::Musl).unwrap();
        unsafe { Library::open(&fixture("bare.c")).unwrap() }
    })
}

fn int(value: i64) -> Ptr {
    Ptr { ptr: value as *mut u8, lower: std::ptr::null_mut() }
}

fn call_long(name: &str, args: &[Ptr]) -> i64 {
    run(|thread| unsafe {
        let function = bare().symbol(thread, name).unwrap();
        let mut returned = [Ptr::NULL];
        call(thread, function, args, &mut returned);
        returned[0].ptr as i64
    })
}

#[test]
fn calls_with_globals() {
    assert_eq!(call_long("add", &[int(2), int(3)]), 12);
}

#[test]
fn heap_lists() {
    for value in 1..=100 {
        call_long("push", &[int(value)]);
    }
    assert_eq!(call_long("total", &[]), 5050);
}

#[test]
fn function_pointers() {
    assert_eq!(call_long("fnptr", &[int(21)]), 42);
}

#[test]
fn stack_memory() {
    assert_eq!(call_long("escape", &[]), 3);
}

#[test]
fn varargs() {
    assert_eq!(call_long("vcall", &[]), 6);
}

/// The stub is embedded, so it shows up as a memfd: nothing here may come from
/// the Fil-C distribution.
#[test]
fn nothing_else_is_loaded() {
    bare();
    let objects = loaded_objects();
    let stray: Vec<_> = objects.iter().filter(|path| path.contains("filc") || path.contains("pizlo")).collect();
    assert!(stray.is_empty(), "{stray:?}");
    let libc_loaded = objects.iter().any(|path| path.ends_with("/libc.so") || path.ends_with("libc.so.6666"));
    assert!(!libc_loaded, "{objects:?}");
}

/// Runs one of the fixture's failing functions in a child process, and returns
/// what it reported.
fn failure_of(case: &str) -> String {
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

#[test]
#[ignore = "run by failure_of in a child process"]
fn failing_child() {
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &libc::rlimit { rlim_cur: 0, rlim_max: 0 }) };
    match std::env::var("FIL_RUST_RT_FAILING_CASE").unwrap().as_str() {
        "oob" => call_long("oob_read", &[int(4)]),
        "uaf" => call_long("use_after_free", &[]),
        "double free" => call_long("double_free", &[]),
        "wild" => call_long("wild_pointer", &[int(0x1000)]),
        "signature" => call_long("wrong_signature", &[]),
        case => panic!("unknown case {case}"),
    };
}

#[test]
fn out_of_bounds_is_stopped() {
    let report = failure_of("oob");
    assert!(report.contains("filc safety error: cannot read pointer with ptr >= upper."), "{report}");
    assert!(report.contains("bare.c:"), "the report should have the stack: {report}");
    assert!(report.contains("filc panic: thwarted a futile attempt to violate memory safety."), "{report}");
}

#[test]
fn use_after_free_is_stopped() {
    let report = failure_of("uaf");
    assert!(report.contains("cannot read pointer to free object."), "{report}");
}

#[test]
fn double_free_is_stopped() {
    let report = failure_of("double free");
    assert!(report.contains("cannot free already free object"), "{report}");
}

#[test]
fn wild_pointer_is_stopped() {
    let report = failure_of("wild");
    assert!(report.contains("cannot read pointer with null object."), "{report}");
}

#[test]
fn wrong_signature_is_stopped() {
    let report = failure_of("signature");
    assert!(report.contains("argument size mismatch"), "{report}");
}
