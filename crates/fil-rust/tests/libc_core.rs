//! The glibc flavor's libc core against the host's: the same fixture, built
//! for each, must write the same transcript of string, memory, ctype, parsing
//! and allocation results. Misuse fails as compiled code would, and other
//! threads moving a string's terminator change answers, never safety.
#![cfg(feature = "glibc")]

mod common;

use std::ffi::{CString, c_int, c_void};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture_with, flavor};
use fil_rust::{Config, Flavor, Library, Runtime};

const FIXTURE: &str = "libc_core.c";

fn runtime() -> Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    *RUNTIME.get_or_init(|| Runtime::start(Config::from_env_for(flavor()).unwrap()).unwrap())
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture_with(FIXTURE, &["-fno-builtin"])).unwrap())
}

fn glibc() -> bool {
    flavor() == Flavor::Glibc
}

/// The fixture built for the host, loaded.
fn host_library() -> *mut c_void {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(FIXTURE);
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("libc_core_host.{}.so", std::process::id()));
    let cc = ["FILC_TARGET_CC", "CC"].into_iter().find_map(|v| std::env::var(v).ok()).unwrap_or_else(|| "cc".into());
    let status = Command::new(cc)
        .args(["-shared", "-fPIC", "-O1", "-fno-builtin", "-w", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "compiling {FIXTURE} for the host failed");
    let path = CString::new(out.to_str().unwrap()).unwrap();
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "loading the host build of {FIXTURE}");
    std::fs::remove_file(&out).unwrap();
    handle
}

fn host_transcript() -> String {
    let handle = host_library();
    let symbol = |name: &str| {
        let name = CString::new(name).unwrap();
        let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
        assert!(!address.is_null());
        address
    };
    let length: extern "C" fn() -> usize = unsafe { std::mem::transmute(symbol("transcript_length")) };
    let byte: extern "C" fn(usize) -> c_int = unsafe { std::mem::transmute(symbol("transcript_byte")) };
    let bytes: Vec<u8> = (0..length()).map(|index| byte(index) as u8).collect();
    String::from_utf8(bytes).unwrap()
}

fn fil_transcript() -> String {
    let library = library();
    let bytes: Vec<u8> = runtime().run(|fil| {
        let length = library.function::<fn() -> usize>(fil, "transcript_length").unwrap().call(fil, ()).unwrap();
        let byte = library.function::<fn(usize) -> i32>(fil, "transcript_byte").unwrap();
        (0..length).map(|index| byte.call(fil, (index,)).unwrap() as u8).collect()
    });
    String::from_utf8(bytes).unwrap()
}

#[test]
fn matches_the_host_libc() {
    if !glibc() {
        return;
    }
    let (host, fil) = (host_transcript(), fil_transcript());
    let lines = host.lines().count();
    assert!(lines > 500, "a short transcript: {lines} lines");
    for (number, (host, fil)) in host.lines().zip(fil.lines()).enumerate() {
        assert_eq!(fil, host, "line {number} differs from the host's");
    }
    assert_eq!(fil.lines().count(), lines);
}

#[test]
fn numbers_can_end_before_an_unterminated_buffer() {
    if !glibc() {
        return;
    }
    let library = library();
    let parsed = runtime().run(|fil| library.function::<fn() -> i64>(fil, "parse_in_a_buffer").unwrap().call(fil, ()).unwrap());
    assert_eq!(parsed, 122);
}

const CHILD: &str = "FIL_RUST_LIBC_CORE_CHILD";

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    let library = library();
    let name = std::env::var(CHILD).unwrap();
    runtime().run(|fil| {
        let _ = library.function::<fn() -> i64>(fil, &name).unwrap().call(fil, ());
    });
}

/// Runs the fixture's `case` in a process of its own, which must fail saying
/// `message`.
fn fails(case: &str, message: &str) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    let output = common::without_core_dumps(&mut command).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{case} succeeded");
    assert!(stderr.contains(message), "{case} failed some other way:\n{stderr}");
}

#[test]
fn misuse_fails_as_compiled_code_would() {
    if !glibc() {
        return;
    }
    fails("length_of_unterminated", "string is not null-terminated");
    fails("copy_past_the_end", "cannot write 29 bytes");
    fails("move_past_the_end", "cannot write destination 100 bytes");
    fails("parse_unterminated", "cannot read");
}

#[test]
fn moving_terminators_change_answers_not_safety() {
    if !glibc() {
        return;
    }
    let library = library();
    const ROUNDS: i32 = 2000;
    let mover = std::thread::spawn(move || {
        runtime().run(|fil| library.function::<fn(i32)>(fil, "move_terminators").unwrap().call(fil, (ROUNDS,)).unwrap())
    });
    let total = runtime().run(|fil| library.function::<fn(i32) -> i64>(fil, "read_moving").unwrap().call(fil, (ROUNDS * 10,)).unwrap());
    mover.join().unwrap();
    assert!(total >= 0, "a length past the buffer");
}
