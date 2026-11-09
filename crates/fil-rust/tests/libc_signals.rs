//! The glibc flavor's signals, with no libc main thread: a signal that lands
//! on a thread that isn't Fil-C's goes to the thread that installed the
//! handler, or queues for the next to attach. Also timers, `dlsym`, exit
//! handlers and what constructors are given.
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
    LIBRARY.get_or_init(|| runtime().load(fixture("libc_signals.c")).unwrap())
}

fn glibc() -> bool {
    flavor() == Flavor::Glibc
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

/// Raises SIGUSR1 on a thread that has never run Fil-C code.
fn raise_elsewhere() {
    std::thread::spawn(|| unsafe { libc::raise(libc::SIGUSR1) }).join().unwrap();
}

fn wait_for_usr1(count: i32) -> i32 {
    for _ in 0..2000 {
        let seen = call("usr1_seen");
        if seen >= count {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    call("usr1_seen")
}

const CHILD: &str = "FIL_RUST_LIBC_SIGNALS_CHILD";

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    let library = library();
    match std::env::var(CHILD).unwrap().as_str() {
        "routing" => {
            // Installed from a thread that stays: the signal goes to it.
            let (ready, wait) = std::sync::mpsc::channel();
            let (done, finish) = std::sync::mpsc::channel::<()>();
            let installer = std::thread::spawn(move || {
                assert_eq!(call("install_usr1"), 0);
                ready.send(()).unwrap();
                finish.recv().unwrap();
            });
            wait.recv().unwrap();
            raise_elsewhere();
            assert_eq!(wait_for_usr1(1), 1, "the installer didn't get it");
            done.send(()).unwrap();
            installer.join().unwrap();
            // The installer is gone: it queues, and this thread, attaching
            // anew, takes it.
            raise_elsewhere();
            assert_eq!(std::thread::spawn(|| wait_for_usr1(2)).join().unwrap(), 2, "nobody took the queued one");
            println!("routing ok");
        }
        "exit" => runtime().run(|fil| library.function::<fn()>(fil, "exit_with_handlers").unwrap().call(fil, ()).unwrap()),
        "quick_exit" => runtime().run(|fil| library.function::<fn()>(fil, "quick_exit_with_handlers").unwrap().call(fil, ()).unwrap()),
        other => panic!("no case {other}"),
    }
}

fn run_child(case: &str) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    common::without_core_dumps(&mut command).output().unwrap()
}

#[test]
fn signals_reach_a_fil_c_thread_with_no_libc_main() {
    if !glibc() {
        return;
    }
    let output = run_child("routing");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("routing ok"), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn signal_hands_back_the_old_handler() {
    if glibc() {
        assert_eq!(call("signal_returns_the_old_handler"), 1);
    }
}

#[test]
fn timers_fire() {
    if glibc() {
        assert_eq!(call("timer_fires"), 1);
    }
}

#[test]
fn dlsym_finds_libc_functions() {
    if glibc() {
        assert_eq!(call("dlsym_finds_qsort"), 1);
    }
}

#[test]
fn exit_runs_handlers_newest_first() {
    if !glibc() {
        return;
    }
    let output = run_child("exit");
    assert_eq!(output.status.code(), Some(7));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("on_exit status 7\natexit\n"), "{stdout}");
    assert!(!stdout.contains("quick"), "{stdout}");
}

#[test]
fn quick_exit_runs_only_its_handlers() {
    if !glibc() {
        return;
    }
    let output = run_child("quick_exit");
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("quick\n") && !stdout.contains("atexit"), "{stdout}");
}

#[test]
fn constructors_get_arguments_and_the_environment() {
    if !glibc() {
        return;
    }
    assert_eq!(call("constructor_argc"), 1);
    assert_eq!(call("constructor_argv0_is"), 1);
    assert_eq!(call("constructor_saw_path"), 1);
}
