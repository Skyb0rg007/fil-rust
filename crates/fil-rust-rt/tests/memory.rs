//! The object model under compiled code: aux arrays, atomic boxes, relocations,
//! and everything that moves capabilities around.

mod support;

use std::sync::OnceLock;

use fil_rust_rt::abi::Ptr;
use fil_rust_rt::Library;
use support::{call_in, failure_of, int, open};

fn memory() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| open("memory.c"))
}

fn call_long(name: &str, args: &[Ptr]) -> i64 {
    call_in(memory(), name, args)
}

#[test]
fn relocations_in_constants() {
    assert_eq!(call_long("relocated", &[]), 10 + 30 + 40 + 30);
}

#[test]
fn globals_in_a_cycle() {
    assert_eq!(call_long("cycle_sum", &[]), 3);
}

#[test]
fn memcpy_carries_capabilities() {
    assert_eq!(call_long("copy_pointers", &[int(1)]), 0);
    assert_eq!(call_long("copy_pointers", &[int(5)]), 10);
    assert_eq!(call_long("copy_pointers", &[int(1000)]), 499500);
}

#[test]
fn overlapping_moves() {
    assert_eq!(call_long("overlap", &[int(1)]), 123);
    assert_eq!(call_long("overlap", &[int(0)]), 23);
}

#[test]
fn structs_by_value() {
    assert_eq!(call_long("by_value", &[]), 28);
}

#[test]
fn pointers_in_varargs() {
    assert_eq!(call_long("vararg_pointers", &[]), 12);
}

#[test]
fn pointer_atomics() {
    assert_eq!(call_long("atomics", &[]), 11202233);
}

#[test]
fn allocation_sizes() {
    assert_eq!(call_long("sizes", &[]), (0..18).sum());
}

#[test]
fn aligned_globals() {
    assert_eq!(call_long("aligned", &[]), 9);
}

#[test]
fn thread_locals() {
    let on_this_thread = call_long("thread_local_variables", &[int(1)]);
    let on_another = std::thread::spawn(|| call_long("thread_local_variables", &[int(41)])).join().unwrap();
    assert_eq!((on_this_thread, on_another), (2, 42));
}

#[test]
fn constructors_run_once() {
    assert_eq!(call_long("ctors", &[]), 1);
}

#[test]
fn outline_arguments() {
    let args: Vec<Ptr> = (0..40).map(int).collect();
    assert_eq!(call_long("many_args", &args), 10 + 20 + 30 + 39);
}

#[test]
fn large_structs_returned() {
    assert_eq!(call_long("huge_call", &[]), 60);
}

#[test]
#[ignore = "run by failure_of in a child process"]
fn failing_child() {
    match support::failing_case().unwrap().as_str() {
        "torn" => call_long("torn", &[]),
        "phase" => call_long("out_of_phase", &[]),
        case => panic!("unknown case {case}"),
    };
}

#[test]
fn partly_overwritten_pointers_lose_their_capability() {
    let report = failure_of("torn");
    assert!(report.contains("cannot read pointer with null object."), "{report}");
}

#[test]
fn pointers_copied_out_of_phase_lose_their_capability() {
    let report = failure_of("phase");
    assert!(report.contains("cannot read pointer with null object."), "{report}");
}
