//! pthreads on the glibc flavor: threads and their results, every lock kind
//! and its errors, condition variables with deadlines, once, key destructors
//! however a thread ends, barriers, semaphores, spin locks and fork handlers;
//! Rust threads sharing a Fil-C mutex with Fil-C threads; and misuse that
//! traps.
#![cfg(feature = "glibc")]

mod common;

use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, flavor};
use fil_rust::{Config, Flavor, Library, Runtime};

fn runtime() -> Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    *RUNTIME.get_or_init(|| Runtime::start(Config::from_env_for(flavor()).unwrap()).unwrap())
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("libc_threads.c")).unwrap())
}

fn glibc() -> bool {
    flavor() == Flavor::Glibc
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

/// Each case's checks all hold: it returns which failed otherwise.
fn holds(name: &str) {
    if glibc() {
        let failed = call(name);
        assert_eq!(failed, 0, "{name}: check {failed} failed");
    }
}

#[test]
fn threads_hand_back_results() {
    holds("threads_and_results");
    holds("exit_hands_back_a_result");
}

#[test]
fn mutexes_of_every_kind() {
    holds("mutex_kinds");
    holds("contended_mutex");
}

#[test]
fn condition_variables_wait_and_time_out() {
    holds("condition_variables");
}

#[test]
fn reader_writer_locks() {
    holds("reader_writer_locks");
}

#[test]
fn once_runs_once_across_threads() {
    holds("once_runs_once");
}

#[test]
fn key_destructors_run_as_threads_end() {
    holds("key_destructors");
}

#[test]
fn barriers_semaphores_and_spin_locks() {
    holds("barriers_semaphores_spinlocks");
}

#[test]
fn fork_handlers_run_on_each_side() {
    holds("fork_handlers");
}

#[test]
fn rust_threads_share_a_fil_c_mutex() {
    if !glibc() {
        return;
    }
    let library = library();
    let before = runtime().run(|fil| library.function::<fn() -> i64>(fil, "counted_so_far").unwrap().call(fil, ()).unwrap());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| runtime().run(|fil| library.function::<fn(i32)>(fil, "count_up").unwrap().call(fil, (150,)).unwrap()));
        }
        // And Fil-C threads at the same time.
        scope.spawn(|| runtime().run(|fil| library.function::<fn()>(fil, "count_in_threads").unwrap().call(fil, ()).unwrap()));
    });
    let after = runtime().run(|fil| library.function::<fn() -> i64>(fil, "counted_so_far").unwrap().call(fil, ()).unwrap());
    assert_eq!(after - before, 4 * 150 + 3 * 200);
}

const CHILD: &str = "FIL_RUST_LIBC_THREADS_CHILD";

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    let library = library();
    let case = std::env::var(CHILD).unwrap();
    runtime().run(|fil| {
        let _ = library.function::<fn() -> i32>(fil, &case).unwrap().call(fil, ());
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
fn misuse_traps() {
    if !glibc() {
        return;
    }
    fails("exit_from_rust", "pthread_exit on a thread Fil-C code didn't create");
    fails("lock_freed_mutex", "free");
}
