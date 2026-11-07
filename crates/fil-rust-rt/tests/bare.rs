//! Fil-C libraries running on the Rust runtime, with no libpizlo, no libc and
//! no thread but the caller's.

mod support;

use std::sync::OnceLock;

use fil_rust_rt::Library;
use fil_rust_rt::abi::Ptr;
use fil_rust_sys::loaded_objects;
use support::{call_in, failure_of, int, open};

fn bare() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| open("bare.c"))
}

fn call_long(name: &str, args: &[Ptr]) -> i64 {
    call_in(bare(), name, args)
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

#[test]
#[ignore = "run by failure_of in a child process"]
fn failing_child() {
    match support::failing_case().unwrap().as_str() {
        "oob" => call_long("oob_read", &[int(4)]),
        "uaf" => call_long("use_after_free", &[]),
        "double free" => call_long("double_free", &[]),
        "wild" => call_long("wild_pointer", &[int(0x1000)]),
        "signature" => call_long("wrong_signature", &[]),
        "overflow" => call_long("recurse", &[int(1 << 40)]),
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

/// The report runs on the failure stack: this thread's is used up.
#[test]
fn stack_overflow_is_stopped() {
    let report = failure_of("overflow");
    assert!(report.contains("filc safety error: stack overflow."), "{report}");
}
