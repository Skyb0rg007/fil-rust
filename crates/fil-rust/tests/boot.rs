//! How the runtime sits in the process with a libc: for the glibc flavor,
//! a library that uses the libc maps nothing of the distribution but the
//! library itself, starts no thread, and runs on the caller's thread, the
//! libc being the runtime's own over the host's; the musl flavor, which has
//! no libc, turns such a library down by name.

mod common;

use std::process::Command;

use common::{config, fixture, flavor, runtime};
use fil_rust::{Error, Flavor};

const CHILD: &str = "FIL_RUST_BOOT_CHILD";

fn thread_count() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|line| line.starts_with("Threads:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Files mapped from the distribution, but for the libraries under test.
fn distribution_mappings() -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let root = config().root().to_str().unwrap().to_string();
    maps.lines().filter(|line| line.contains(&root)).map(str::to_string).collect()
}

/// What's mapped from memory: the runtime's stand-ins.
fn memory_files() -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mut names: Vec<String> =
        maps.lines().filter_map(|line| line.split_whitespace().nth(5)).filter_map(|path| path.strip_prefix("/memfd:")).map(|name| name.trim_end_matches(" (deleted)").to_string()).collect();
    names.sort();
    names.dedup();
    names
}

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let before = thread_count();
    let rt = runtime();
    let library = rt.load(fixture("basic.c")).unwrap();
    let (caller, tid, text) = rt.run(|fil| {
        let gettid = fil.libc().function::<fn() -> i32>(fil, "gettid").unwrap();
        let tid = gettid.call(fil, ()).unwrap();
        let buffer = fil.alloc_bytes(32).unwrap();
        let format = library.function::<fn(fil_rust::Ptr<u8>, usize, i32, fil_rust::Ptr<std::ffi::c_char>) -> i32>(fil, "format_into").unwrap();
        format.call(fil, (buffer, 32, 7, fil.c_str("seven").unwrap())).unwrap();
        (unsafe { libc::gettid() }, tid, buffer.cast::<std::ffi::c_char>().read_string(fil).unwrap())
    });
    assert_eq!(tid, caller, "the libc ran on another thread");
    assert_eq!(text, "7:seven");
    assert_eq!(thread_count(), before, "something started a thread");
    let mappings = distribution_mappings();
    assert!(mappings.is_empty(), "{mappings:#?}");
    let memory = memory_files();
    for name in ["libpizlo.so", "libc.so.6666", "libm.so.6666", "libpthread.so.6660", "libdl.so.6662"] {
        assert!(memory.iter().any(|file| file == name), "no stand-in for {name}: {memory:?}");
    }
}

#[test]
fn a_library_using_the_libc_brings_in_nothing_else() {
    if flavor() != Flavor::Glibc {
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, "1");
    let output = common::without_core_dumps(&mut command).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn musl_turns_down_a_library_that_needs_a_libc() {
    if flavor() != Flavor::Musl {
        return;
    }
    match runtime().load(fixture("basic.c")) {
        Err(Error::Unsupported(message)) => assert!(message.contains("libc") && message.contains("glibc"), "{message}"),
        Err(other) => panic!("another error: {other}"),
        Ok(_) => panic!("loaded a library that needs a libc"),
    }
}
