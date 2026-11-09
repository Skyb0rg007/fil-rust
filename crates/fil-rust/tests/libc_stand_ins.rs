//! The glibc flavor's libc stand-ins: errno, which the runtime keeps per
//! thread, and the symbols with nothing behind them, which fail by name only
//! once they're used.
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
    LIBRARY.get_or_init(|| runtime().load(fixture("libc_stand_ins.c")).unwrap())
}

fn glibc() -> bool {
    flavor() == Flavor::Glibc
}

const CHILD: &str = "FIL_RUST_LIBC_STAND_INS_CHILD";

/// Runs `case` of `child` in a process of its own, which is expected to fail.
fn failure_of(case: &str) -> String {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    let output = common::without_core_dumps(&mut command).output().unwrap();
    assert!(!output.status.success(), "{case} succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    let library = library();
    runtime().run(|fil| match std::env::var(CHILD).unwrap().as_str() {
        "call" => {
            let _ = library.function::<fn() -> i32>(fil, "call_revoke").unwrap().call(fil, ());
        }
        "read" => {
            let _ = library.function::<fn() -> i64>(fil, "read_re_syntax_options").unwrap().call(fil, ());
        }
        other => panic!("no case {other}"),
    });
}

#[test]
fn errno_is_the_threads_own() {
    if !glibc() {
        return;
    }
    let library = library();
    let mine = runtime().run(|fil| {
        let set = library.function::<fn(i32) -> i32>(fil, "set_errno_to").unwrap();
        let address = library.function::<fn() -> usize>(fil, "errno_address").unwrap();
        assert_eq!(set.call(fil, (17,)).unwrap(), 17);
        // `__errno_location` is const: the address mustn't move.
        let first = address.call(fil, ()).unwrap();
        assert_eq!(address.call(fil, ()).unwrap(), first);
        first
    });
    let theirs = std::thread::spawn(move || {
        runtime().run(|fil| {
            let get = library.function::<fn() -> i32>(fil, "get_errno").unwrap();
            let set = library.function::<fn(i32) -> i32>(fil, "set_errno_to").unwrap();
            assert_eq!(get.call(fil, ()).unwrap(), 0, "another thread's errno");
            set.call(fil, (5,)).unwrap();
            library.function::<fn() -> usize>(fil, "errno_address").unwrap().call(fil, ()).unwrap()
        })
    })
    .join()
    .unwrap();
    assert_ne!(mine, theirs);
    runtime().run(|fil| {
        let get = library.function::<fn() -> i32>(fil, "get_errno").unwrap();
        assert_eq!(get.call(fil, ()).unwrap(), 17);
    });
}

#[test]
fn errno_the_runtime_sets_is_the_one_c_reads() {
    if !glibc() {
        return;
    }
    let library = library();
    runtime().run(|fil| {
        let zset_errno = fil.runtime_library().function::<fn(i32)>(fil, "zset_errno").unwrap();
        zset_errno.call(fil, (libc::EBADF,)).unwrap();
        let get = library.function::<fn() -> i32>(fil, "get_errno").unwrap();
        assert_eq!(get.call(fil, ()).unwrap(), libc::EBADF);
    });
}

#[test]
fn a_missing_function_fails_when_called_not_before() {
    if !glibc() {
        return;
    }
    let library = library();
    runtime().run(|fil| {
        let there = library.function::<fn() -> i32>(fil, "revoke_is_there").unwrap();
        assert_eq!(there.call(fil, ()).unwrap(), 1);
    });
    let stderr = failure_of("call");
    assert!(stderr.contains("revoke is not implemented"), "{stderr}");
}

#[test]
fn a_missing_variable_fails_when_its_getter_runs() {
    if !glibc() {
        return;
    }
    let stderr = failure_of("read");
    assert!(stderr.contains("re_syntax_options is not implemented"), "{stderr}");
}
