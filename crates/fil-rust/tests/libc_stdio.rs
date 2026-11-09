//! The glibc flavor's stdio against the host's: the same fixture, built for
//! each, must write the same transcript of formatting, stream, line, position
//! and directory results. Output stays in order with Rust's, a `FILE` is
//! known by identity whatever's written over it, closed streams trap, `%s` and
//! `%n` are checked, and buffered streams survive a fork.
#![cfg(feature = "glibc")]

mod common;

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture_with, flavor};
use fil_rust::{Config, Flavor, Library, Runtime};

const FIXTURE: &str = "libc_stdio.c";

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

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("libc_stdio.{name}.{}", std::process::id()))
}

fn host_transcript() -> String {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(FIXTURE);
    let out = scratch("host.so");
    let cc = ["FILC_TARGET_CC", "CC"].into_iter().find_map(|v| std::env::var(v).ok()).unwrap_or_else(|| "cc".into());
    let status = Command::new(cc).args(["-shared", "-fPIC", "-O1", "-fno-builtin", "-w", "-o"]).arg(&out).arg(&src).status().unwrap();
    assert!(status.success(), "compiling {FIXTURE} for the host failed");
    let path = CString::new(out.to_str().unwrap()).unwrap();
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "loading the host build of {FIXTURE}");
    std::fs::remove_file(&out).unwrap();
    let symbol = |name: &str| {
        let name = CString::new(name).unwrap();
        let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
        assert!(!address.is_null());
        address
    };
    let write: extern "C" fn(*const c_char) = unsafe { std::mem::transmute::<*mut c_void, _>(symbol("write_transcript")) };
    let directory = CString::new(scratch("host-dir").to_str().unwrap()).unwrap();
    write(directory.as_ptr());
    let length: extern "C" fn() -> usize = unsafe { std::mem::transmute::<*mut c_void, _>(symbol("transcript_length")) };
    let byte: extern "C" fn(usize) -> c_int = unsafe { std::mem::transmute::<*mut c_void, _>(symbol("transcript_byte")) };
    String::from_utf8((0..length()).map(|index| byte(index) as u8).collect()).unwrap()
}

fn fil_transcript() -> String {
    let library = library();
    let bytes: Vec<u8> = runtime().run(|fil| {
        let directory = fil.c_str(scratch("fil-dir").to_str().unwrap()).unwrap();
        library.function::<fn(fil_rust::Ptr<c_char>)>(fil, "write_transcript").unwrap().call(fil, (directory,)).unwrap();
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
    assert!(lines > 60, "a short transcript: {lines} lines");
    for (number, (host, fil)) in host.lines().zip(fil.lines()).enumerate() {
        assert_eq!(fil, host, "line {number} differs from the host's");
    }
    assert_eq!(fil.lines().count(), lines);
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

#[test]
fn a_scribbled_over_file_is_still_the_same_stream() {
    if glibc() {
        assert_eq!(call("scribbled_file_still_works"), 0);
    }
}

#[test]
fn string_precision_stops_at_the_object() {
    if glibc() {
        assert_eq!(call("capped_string"), 7);
    }
}

const CHILD: &str = "FIL_RUST_LIBC_STDIO_CHILD";

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    let library = library();
    let case = std::env::var(CHILD).unwrap();
    match case.as_str() {
        "ordering" => {
            let print = |which: i32| runtime().run(|fil| library.function::<fn(i32)>(fil, "print_some").unwrap().call(fil, (which,)).unwrap());
            println!("rust 1");
            print(0);
            println!("rust 2");
            print(1);
            print!("rust 3 ");
            print(1);
        }
        "fork" => {
            let path = std::env::var("FIL_RUST_LIBC_STDIO_PATH").unwrap();
            let result = runtime().run(|fil| {
                let path = fil.c_str(&path).unwrap();
                library.function::<fn(fil_rust::Ptr<c_char>) -> i32>(fil, "fork_with_buffered_streams").unwrap().call(fil, (path,)).unwrap()
            });
            println!("fork result {result}");
        }
        name => {
            runtime().run(|fil| {
                let _ = library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ());
            });
        }
    }
}

fn run_child(case: &str, extra: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    command.envs(extra.iter().copied());
    common::without_core_dumps(&mut command).output().unwrap()
}

/// Runs the fixture's `case` in a process of its own, which must fail saying
/// `message`.
fn fails(case: &str, message: &str) {
    let output = run_child(case, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{case} succeeded");
    assert!(stderr.contains(message), "{case} failed some other way:\n{stderr}");
}

#[test]
fn output_stays_in_order_with_rust() {
    if !glibc() {
        return;
    }
    let output = run_child("ordering", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = "rust 1\nc 1\nc partial then puts\nrust 2\nc two\nrust 3 c two\n";
    assert!(stdout.contains(expected), "{stdout}");
}

#[test]
fn misuse_traps() {
    if !glibc() {
        return;
    }
    fails("fake_file", "not a FILE this libc opened");
    fails("use_after_fclose", "free");
    fails("read_after_fclose", "free");
    fails("entry_after_closedir", "free");
    fails("unterminated_string", "string is not null-terminated");
    fails("count_too_wide", "cannot write 8 bytes");
    fails("count_through_an_integer", "cannot write");
    fails("sprintf_overflows", "cannot write");
    fails("too_few_arguments", "variadic");
}

#[test]
fn buffered_streams_survive_a_fork() {
    if !glibc() {
        return;
    }
    let path = scratch("fork.txt");
    let output = run_child("fork", &[("FIL_RUST_LIBC_STDIO_PATH", path.to_str().unwrap())]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success() && stdout.contains("fork result 0"), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("child printed\n"), "{stdout}");
    assert_eq!(stdout.matches("parent before fork\n").count(), 1, "{stdout}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before child\nbefore parent\n");
    std::fs::remove_file(&path).unwrap();
}
