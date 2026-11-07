//! `stdfil.h`: natives in Rust, and what the support library builds on them.

mod support;

use std::sync::OnceLock;

use fil_rust_rt::Library;
use support::{call_in, failure_of, open};

fn natives() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| open("natives.c"))
}

fn call_long(name: &str) -> i64 {
    call_in(natives(), name, &[])
}

#[test]
fn reallocation() {
    assert_eq!(call_long("realloc_keeps_contents"), 7);
}

#[test]
fn alignment() {
    assert_eq!(call_long("aligned_allocation"), 1);
}

#[test]
fn inspecting_capabilities() {
    assert_eq!(call_long("capabilities"), 4);
}

#[test]
fn strings() {
    assert_eq!(call_long("string_length"), 5);
}

#[test]
fn support_library_natives() {
    assert_eq!(call_long("collection_request"), 1);
    assert_eq!(call_long("formatting"), 4);
}

#[test]
#[ignore = "run by failure_of in a child process"]
fn failing_child() {
    match support::failing_case().unwrap().as_str() {
        "user error" => call_long("user_error"),
        "unterminated" => call_long("unterminated"),
        case => panic!("unknown case {case}"),
    };
}

#[test]
fn user_errors() {
    let report = failure_of("user error");
    assert!(report.contains("filc user error: gave up"), "{report}");
    assert!(report.contains("filc panic: user thwarted themselves."), "{report}");
}

#[test]
fn unterminated_strings_are_stopped() {
    let report = failure_of("unterminated");
    assert!(report.contains("filc safety error"), "{report}");
}
