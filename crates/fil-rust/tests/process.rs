//! Behavior that ends the process: Fil-C safety panics, misuse of the
//! calling convention, panicking callbacks, `exit`, and stdio flushing at
//! exit. Each case runs in a child process (this test binary re-executed with
//! `FIL_RUST_CHILD` naming the case).

mod common;

use std::ffi::c_char;
use std::process::{Command, Output};

use common::basic::IntFn;
use common::{basic, runtime};
use fil_rust::{Callback, Ptr};

fn run_child(case: &str) -> Output {
    let exe = std::env::current_exe().unwrap();
    common::without_core_dumps(&mut Command::new(exe))
        .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env("FIL_RUST_CHILD", case)
        .output()
        .expect("running the child process")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The child side: does what the case says.
#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    let rt = runtime();
    let b = basic();
    match case.as_str() {
        "c-out-of-bounds" => {
            rt.run(|fil| {
                let a = fil.alloc_array::<i32>(&[1, 2, 3]).unwrap();
                // The C code indexes past the end: Fil-C stops it.
                let _ = b.library().function::<fn(Ptr<i32>, i64) -> i32>(fil, "read_at").unwrap().call(fil, (a, 1000));
            });
        }
        "c-use-after-free" => {
            rt.run(|fil| {
                let s = fil.c_str("freed").unwrap();
                b.free_it(fil, s.cast()).unwrap();
                let _ = b.my_strlen(fil, s);
            });
        }
        "too-few-arguments" => {
            rt.run(|fil| {
                // Claims add takes one argument.
                let wrong = b.library().function::<fn(i32) -> i32>(fil, "add").unwrap();
                let _ = wrong.call(fil, (1,));
            });
        }
        "pointer-as-integer" => {
            rt.run(|fil| {
                // Claims my_strlen takes an integer: the callee sees a pointer
                // without a capability.
                let wrong = b.library().function::<fn(u64) -> usize>(fil, "my_strlen").unwrap();
                let s = fil.c_str("abc").unwrap();
                let _ = wrong.call(fil, (s.addr() as u64,));
            });
        }
        "callback-panics" => {
            rt.run(|fil| {
                let cb = Callback::<IntFn>::new(fil, |_, (x,)| {
                    if x == 3 {
                        panic!("callback refused {x}");
                    }
                    x
                })
                .unwrap();
                let apply = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap();
                let _ = apply.call(fil, (cb.get(fil), 3));
            });
        }
        "stale-callback" => {
            rt.run(|fil| {
                let cb = Callback::<IntFn>::new(fil, |_, (x,)| x).unwrap();
                let f = cb.get(fil);
                drop(cb);
                let apply = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap();
                let _ = apply.call(fil, (f, 1));
            });
        }
        "callback-too-few-arguments" => {
            rt.run(|fil| {
                // The callback claims two arguments; C passes one.
                let cb = Callback::<fn(i32, i32) -> i32>::new(fil, |_, (a, b)| a + b).unwrap();
                let apply = b.library().function::<fn(fn(i32, i32) -> i32, i32) -> i32>(fil, "apply").unwrap();
                let _ = apply.call(fil, (cb.get(fil), 1));
            });
        }
        "exit-code" => {
            rt.run(|fil| {
                b.say(fil, fil.c_str("before exit").unwrap()).unwrap();
                let exit = fil.libc().function::<fn(i32)>(fil, "exit").unwrap();
                let _ = exit.call(fil, (3,));
            });
        }
        "flush-at-exit" => {
            rt.run(|fil| {
                let printf = fil.libc().function::<fn(Ptr<c_char>) -> i32>(fil, "printf").unwrap();
                // No newline and no flush: only the exit hook writes it out.
                printf.call(fil, (fil.c_str("buffered output").unwrap(),)).unwrap();
            });
        }
        other => panic!("unknown child case {other}"),
    }
}

fn assert_safety_panic(o: &Output, needle: &str) {
    assert!(!o.status.success(), "child should have died: {}", stderr(o));
    let err = stderr(o);
    assert!(err.contains("filc panic"), "no Fil-C panic in:\n{err}");
    assert!(err.contains(needle), "expected {needle:?} in:\n{err}");
}

#[test]
fn out_of_bounds_in_c_is_stopped() {
    let o = run_child("c-out-of-bounds");
    assert_safety_panic(&o, "read_at");
}

#[test]
fn use_after_free_in_c_is_stopped() {
    let o = run_child("c-use-after-free");
    assert_safety_panic(&o, "free");
}

#[test]
fn wrong_signature_with_too_few_arguments_is_stopped() {
    let o = run_child("too-few-arguments");
    assert_safety_panic(&o, "argument size mismatch");
}

#[test]
fn integers_never_become_pointers() {
    let o = run_child("pointer-as-integer");
    assert_safety_panic(&o, "null object");
}

#[test]
fn panicking_callback_ends_the_process() {
    let o = run_child("callback-panics");
    assert_safety_panic(&o, "a Rust callback panicked: callback refused 3");
}

#[test]
fn stale_callback_is_stopped() {
    let o = run_child("stale-callback");
    assert_safety_panic(&o, "after it was dropped");
}

#[test]
fn callback_with_missing_arguments_is_stopped() {
    let o = run_child("callback-too-few-arguments");
    assert_safety_panic(&o, "argument size mismatch");
}

#[test]
fn exit_from_fil_c() {
    let o = run_child("exit-code");
    assert_eq!(o.status.code(), Some(3), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("before exit"));
}

#[test]
fn stdio_is_flushed_at_exit() {
    let o = run_child("flush-at-exit");
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("buffered output"), "stdout: {}", stdout(&o));
}
