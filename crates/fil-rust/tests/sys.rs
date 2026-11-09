//! The system calls Fil-C code makes, seen from C, through the unmodified libc.

mod common;

use std::ffi::c_char;
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, runtime};
use fil_rust::{Library, Ptr};

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("sys.c")).unwrap())
}

fn check(name: &str) {
    let library = library();
    let line = runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap());
    assert_eq!(line, 0, "{name} failed at line {line} of sys.c");
}

fn check_with_path(name: &str, path: &str) {
    let library = library();
    let line = runtime().run(|fil| {
        let function = library.function::<fn(Ptr<c_char>) -> i32>(fil, name).unwrap();
        function.call(fil, (fil.c_str(path).unwrap(),)).unwrap()
    });
    assert_eq!(line, 0, "{name} failed at line {line} of sys.c");
}

#[test]
fn files_and_directories() {
    let directory = std::env::temp_dir().join(format!("fil-rust-sys-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let directory = directory.canonicalize().unwrap();
    check_with_path("files", directory.to_str().unwrap());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn pipes_and_waiting() {
    check("streams");
}

#[test]
fn epoll() {
    check("epoll_events");
}

#[test]
fn the_state_of_the_process() {
    check("process_state");
}

#[test]
fn loading_libraries_from_c() {
    check_with_path("dynamic_loading", fixture("basic.c").to_str().unwrap());
}

#[test]
fn stdio() {
    check("printed");
}

#[test]
#[ignore = "run by the other test, in a process of its own"]
fn overflowing_a_buffer_in_a_system_call() {
    let library = library();
    runtime().run(|fil| {
        library.function::<fn(i32) -> i64>(fil, "read_into_small_buffer").unwrap().call(fil, (0,)).unwrap();
    });
}

#[test]
fn system_calls_check_their_buffers() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "overflowing_a_buffer_in_a_system_call", "--ignored", "--nocapture"]);
    let output = common::without_core_dumps(&mut command).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("filc panic"), "{stderr}");
    assert!(stderr.contains("thwarted a futile attempt to violate memory safety"), "{stderr}");
}
