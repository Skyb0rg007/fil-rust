//! Processes, System V IPC, timers and the remaining system calls, seen from C
//! through the unmodified libc. This is backend-neutral, so what passes here
//! means the same on both.

mod common;

use std::ffi::c_char;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, runtime};
use fil_rust::{Library, Ptr};

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("system.c")).unwrap())
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

fn check(name: &str) {
    let line = call(name);
    assert_eq!(line, 0, "{name} failed at line {line} of system.c");
}

fn check_with_path(name: &str, path: &str) {
    let library = library();
    let line = runtime().run(|fil| {
        let function = library.function::<fn(Ptr<c_char>) -> i32>(fil, name).unwrap();
        function.call(fil, (fil.c_str(path).unwrap(),)).unwrap()
    });
    assert_eq!(line, 0, "{name} failed at line {line} of system.c");
}

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("fil-rust-system-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn a_fork_leaves_the_child_with_a_working_runtime() {
    let library = library();
    let line = runtime().run(|fil| {
        library.function::<fn(i32) -> i32>(fil, "fork_from_many_threads").unwrap().call(fil, (50,)).unwrap()
    });
    assert_eq!(line, 0, "failed at line {line} of system.c");
}

#[test]
fn waiting_for_children() {
    check("waiting_for_children");
}

#[test]
fn starting_programs() {
    check_with_path("starting_programs", std::env::current_exe().unwrap().to_str().unwrap());
}

#[test]
fn message_queues() {
    check("shared_message_queues");
}

#[test]
fn semaphore_sets() {
    check("semaphores");
}

#[test]
fn timers() {
    check("timers");
}

#[test]
fn files_and_directories() {
    let directory = scratch("files");
    check_with_path("files", directory.to_str().unwrap());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_state_of_the_process() {
    check("the_process");
}

#[test]
fn privileged_calls_get_their_arguments() {
    check("calls_that_need_privilege");
}

#[test]
fn quota_commands_are_recognized() {
    if runtime().config().backend() == fil_rust::Backend::Rust {
        check("asking_for_a_quota");
    }
}

#[test]
fn splicing_memory_into_pipes() {
    check("vector_splicing");
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    if case == "exec-reached" {
        std::process::exit(42);
    }
    call(&case);
}

#[test]
fn misuse_is_stopped() {
    for (case, message) in [
        ("unterminated_arguments", "thwarted a futile attempt"),
        ("bad_string_in_arguments", "thwarted a futile attempt"),
        ("short_message_buffer", "thwarted a futile attempt"),
        ("short_wait_status", "thwarted a futile attempt"),
        ("short_semaphore_operations", "thwarted a futile attempt"),
        ("mapping_tls_away", "ARCH_SET_FS"),
        ("seccomp_in_strict_mode_is_not_for_fil_c", "SECCOMP_MODE_STRICT"),
    ] {
        let output = common::without_core_dumps(&mut Command::new(std::env::current_exe().unwrap()))
            .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env("FIL_RUST_CHILD", case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: {stderr}");
        assert!(stderr.contains(message), "{case}: {stderr}");
    }
}
