//! Sockets, seen from C through the unmodified libc. This is backend-neutral,
//! so what passes here means the same on both.

mod common;

use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{fixture, runtime};
use fil_rust::{Library, Ptr};

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("sockets.c")).unwrap())
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

fn check(name: &str) {
    let line = call(name);
    assert_eq!(line, 0, "{name} failed at line {line} of sockets.c");
}

/// A new directory. A socket's path has to be short, so it isn't under the
/// target directory.
fn directory() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!("fil-rust-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let path = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn check_in_directory(name: &str) {
    let path: PathBuf = directory();
    let library = library();
    let line = runtime().run(|fil| {
        let function = library.function::<fn(Ptr<c_char>) -> i32>(fil, name).unwrap();
        function.call(fil, (fil.c_str(Path::new(&path).to_str().unwrap()).unwrap(),)).unwrap()
    });
    std::fs::remove_dir_all(path).unwrap();
    assert_eq!(line, 0, "{name} failed at line {line} of sockets.c");
}

#[test]
fn streams_between_a_pair() {
    check("stream_pairs");
}

#[test]
fn listening_and_connecting() {
    check_in_directory("listening");
}

#[test]
fn datagrams_with_addresses() {
    check_in_directory("datagrams");
}

#[test]
fn options_are_set_and_reported() {
    check("options");
}

#[test]
fn messages_in_pieces_with_a_descriptor() {
    check("messages");
}

#[test]
fn messages_with_names() {
    check_in_directory("message_names");
}

#[test]
fn several_messages_at_once() {
    check("many_messages");
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    call(&case);
}

#[test]
fn misuse_is_stopped() {
    for case in [
        "receive_past_the_end",
        "receive_through_a_message",
        "send_past_the_end",
        "message_with_too_many_pieces",
        "message_with_a_small_control_buffer",
        "address_length_past_the_end",
        "received_address_past_the_end",
        "forged_pointer_in_a_message",
        "too_many_messages",
    ] {
        let output = common::without_core_dumps(&mut Command::new(std::env::current_exe().unwrap()))
            .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env("FIL_RUST_CHILD", case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: {stderr}");
        assert!(stderr.contains("thwarted a futile attempt to violate memory safety"), "{case}: {stderr}");
    }
}
