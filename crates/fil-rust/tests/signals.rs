//! Signal handling with two runtimes in one process: Rust std's stack
//! overflow handler (SIGSEGV/SIGBUS on guard pages) and the Fil-C runtime's
//! own signal handling. Process-ending cases run in a child process.

mod common;

use std::process::{Command, Output};

use common::{fixture, runtime};

fil_rust::foreign! {
    struct Signals {
        fn raise_usr1(n: i32) -> i32;
        fn usr1_count_now() -> i32;
        fn wait_usr1(count: i32, ms: i32) -> i32;
        fn interval_timer(n: i32) -> i32;
        fn recurse_forever(depth: i32) -> i32;
    }
}

/// Serializes the tests that use the fixture's signal counters. Taken
/// outside `Runtime::run`.
static COUNTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn signals() -> &'static Signals {
    static S: std::sync::OnceLock<Signals> = std::sync::OnceLock::new();
    S.get_or_init(|| runtime().run(|fil| Signals::load(fil, fixture("signals.c")).unwrap()))
}

fn run_child(case: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env("FIL_RUST_CHILD", case)
        .output()
        .expect("running the child process")
}

#[inline(never)]
#[allow(unconditional_recursion)]
fn rust_recurse(n: u64) -> u64 {
    let pad = std::hint::black_box([n; 64]);
    rust_recurse(n + 1) + pad[3]
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    let rt = runtime();
    let s = signals();
    // The runtime (and its signal handlers) are fully up.
    assert_eq!(rt.run(|fil| s.raise_usr1(fil, 1).unwrap()), 1);
    match case.as_str() {
        "rust-overflow-std-thread" => {
            std::thread::Builder::new().stack_size(256 * 1024).spawn(|| rust_recurse(0)).unwrap().join().unwrap();
        }
        "rust-overflow-main-thread" => {
            rust_recurse(0);
        }
        "rust-overflow-in-job" => {
            rt.run(|_fil| rust_recurse(0));
        }
        "fil-c-overflow" => {
            rt.run(|fil| s.recurse_forever(fil, 0).unwrap());
        }
        other => panic!("unknown child case {other}"),
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn rust_stack_overflow_is_still_detected_on_rust_threads() {
    for case in ["rust-overflow-std-thread", "rust-overflow-main-thread"] {
        let o = run_child(case);
        let err = stderr(&o);
        assert!(err.contains("has overflowed its stack"), "{case}: Rust's handler didn't report it:\n{err}");
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&o.status), Some(libc::SIGABRT), "{case}");
    }
}

#[test]
fn rust_stack_overflow_on_an_executor_thread_ends_the_process() {
    // Executors are Fil-C threads whose stack guard Rust doesn't know about;
    // the overflow still ends the process with a fatal signal, not a hang
    // or silent corruption.
    let o = run_child("rust-overflow-in-job");
    let sig = std::os::unix::process::ExitStatusExt::signal(&o.status);
    assert!(matches!(sig, Some(libc::SIGSEGV | libc::SIGBUS | libc::SIGABRT)), "status {:?}:\n{}", o.status, stderr(&o));
}

#[test]
fn fil_c_stack_overflow_is_a_fil_c_panic() {
    let o = run_child("fil-c-overflow");
    let err = stderr(&o);
    assert!(!o.status.success());
    assert!(err.contains("filc panic") || err.contains("filc safety error"), "{err}");
    assert!(err.to_lowercase().contains("stack"), "{err}");
}

#[test]
fn fil_c_signal_handlers_run() {
    let _guard = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let rt = runtime();
    let s = signals();
    assert_eq!(rt.run(|fil| s.raise_usr1(fil, 5).unwrap()), 5);
    assert_eq!(rt.run(|fil| s.interval_timer(fil, 10).unwrap()), 10);
}

#[test]
fn signals_from_outside_reach_fil_c_handlers() {
    let _guard = COUNTERS.lock().unwrap_or_else(|e| e.into_inner());
    let rt = runtime();
    let s = signals();
    let before = rt.run(|fil| {
        s.raise_usr1(fil, 0).unwrap();
        s.usr1_count_now(fil).unwrap()
    });
    assert_eq!(before, 0);
    // Sent to the process, so any thread that doesn't block SIGUSR1 may
    // take it: Rust threads, the executors, or the Fil-C main thread.
    for _ in 0..3 {
        unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
    }
    let seen = rt.run(|fil| s.wait_usr1(fil, 3, 5000).unwrap());
    // Standard signals coalesce while pending, so at least one arrives.
    assert!((1..=3).contains(&seen), "{seen}");

    // Aimed at this (Rust) thread: the runtime's handler can't run here, so
    // the signal is handed to the Fil-C main thread.
    unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGUSR1) };
    let after = rt.run(|fil| s.wait_usr1(fil, seen + 1, 5000).unwrap());
    assert_eq!(after, seen + 1);

    // And from a Rust thread spawned after the handler was installed.
    std::thread::spawn(|| unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGUSR1) }).join().unwrap();
    assert_eq!(rt.run(|fil| s.wait_usr1(fil, seen + 2, 5000).unwrap()), seen + 2);
}
