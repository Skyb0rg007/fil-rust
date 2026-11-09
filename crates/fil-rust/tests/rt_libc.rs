//! The Fil-C libc on the Rust runtime: when it comes, what's different once it
//! has, and what's the same. The runtime can start once per process and these
//! are about how it starts, so each case gets a process of its own.

mod common;

use std::ffi::c_char;
use std::process::{Command, Output};
use std::sync::Barrier;
use std::time::Duration;

use common::{fixture, flavor};
use fil_rust::{Collection, Config, Error, Libc, Ptr, Runtime};

const CHILD: &str = "FIL_RUST_RT_LIBC_CHILD";

fn run_child(case: &str) -> Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    common::without_core_dumps(&mut command).output().unwrap()
}

fn passes(case: &str) {
    if flavor() != fil_rust::Flavor::Musl {
        return;
    }
    let output = run_child(case);
    assert!(
        output.status.success(),
        "{case}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn config() -> Config {
    Config::from_env_for(flavor()).unwrap()
}

/// Whether the Fil-C libc is in the process.
fn libc_is_mapped() -> bool {
    let root = std::env::var("FILC_ROOT").unwrap();
    std::fs::read_to_string("/proc/self/maps")
        .unwrap()
        .lines()
        .any(|line| line.contains(&root) && line.ends_with("/libc.so"))
}

fn needs_libc() -> std::path::PathBuf {
    fixture("rt_libc.c")
}

fn collect(fil: &fil_rust::Fil<'_>) -> fil_rust::Callback<fn()> {
    fil.callback::<fn(), _>(|fil, ()| fil.collect_garbage().unwrap()).unwrap()
}

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    match std::env::var(CHILD).unwrap().as_str() {
        "auto" => {
            let rt = Runtime::start(config()).unwrap();
            let bare = rt.load(fixture("rt_gc.c")).unwrap();
            assert!(!libc_is_mapped());
            let caller = std::thread::current().id();
            assert_eq!(rt.run(|_| std::thread::current().id()), caller);

            let library = rt.load(needs_libc()).unwrap();
            assert!(libc_is_mapped());
            assert_ne!(rt.run(|_| std::thread::current().id()), caller);
            let (process, kept) = rt.run(|fil| {
                let current = library.function::<fn() -> i32>(fil, "current_process").unwrap();
                let stack = bare.function::<fn(fn(), i64) -> i64>(fil, "frame_slot").unwrap();
                (current.call(fil, ()).unwrap(), stack.call(fil, (collect(fil).get(fil), 1234)).unwrap())
            });
            assert_eq!(process, std::process::id() as i32);
            assert_eq!(kept, 1234);
        }
        "asking-for-the-libc" => {
            let rt = Runtime::start(config()).unwrap();
            assert!(!libc_is_mapped());
            let libc = rt.libc();
            assert!(libc_is_mapped());
            let length = rt.run(|fil| {
                let strlen = libc.function::<fn(Ptr<c_char>) -> usize>(fil, "strlen").unwrap();
                strlen.call(fil, (fil.c_str("hello").unwrap(),)).unwrap()
            });
            assert_eq!(length, 5);
        }
        "waits-for-runs" => {
            let rt = Runtime::start(config()).unwrap();
            let started = Barrier::new(2);
            std::thread::scope(|scope| {
                let running = scope.spawn(|| {
                    rt.run(|fil| {
                        started.wait();
                        fil.exited(|| std::thread::sleep(Duration::from_millis(200)));
                        libc_is_mapped()
                    })
                });
                started.wait();
                rt.load(needs_libc()).unwrap();
                assert!(!running.join().unwrap(), "the libc showed up under a running thread");
            });
        }
        "never" => {
            let rt = Runtime::start(config().libc(Libc::Never)).unwrap();
            match rt.load(needs_libc()) {
                Err(Error::Unsupported(message)) => {
                    assert!(message.contains("pizlonated_"), "{message}");
                }
                other => panic!("{other:?}"),
            }
            assert!(!libc_is_mapped());
        }
        "always" => {
            let rt = Runtime::start(config().libc(Libc::Always)).unwrap();
            assert!(libc_is_mapped());
            let library = rt.run(|fil| fil.load(needs_libc())).unwrap();
            rt.run(|fil| {
                assert_eq!(library.function::<fn() -> i32>(fil, "constructed").unwrap().call(fil, ()).unwrap(), 1);
            });
        }
        "inside-run" => {
            let rt = Runtime::start(config()).unwrap();
            rt.run(|fil| {
                assert!(matches!(fil.load(needs_libc()), Err(Error::Unsupported(_))));
                assert!(matches!(rt.load(needs_libc()), Err(Error::Unsupported(_))));
            });
            assert!(!libc_is_mapped());
            rt.load(needs_libc()).unwrap();
        }
        "constructors-see-the-arguments" => {
            let rt = Runtime::start(config().args(["custom-name"])).unwrap();
            let library = rt.load(needs_libc()).unwrap();
            let seen = rt.run(|fil| {
                assert_eq!(library.function::<fn() -> i32>(fil, "constructed").unwrap().call(fil, ()).unwrap(), 1);
                let seen = library.function::<fn() -> Ptr<c_char>>(fil, "argument_seen").unwrap();
                seen.call(fil, ()).unwrap().read_string(fil).unwrap()
            });
            assert_eq!(seen, "custom-name");
        }
        "late-constructors-see-the-environment" => {
            let rt = Runtime::start(config().collection(Collection::Manual)).unwrap();
            let library = rt.load(needs_libc()).unwrap();
            rt.run(|fil| library.function::<fn()>(fil, "replace_environment").unwrap().call(fil, ()).unwrap());
            rt.collect().unwrap();
            let late = rt.load(fixture("rt_libc_late.c")).unwrap();
            let seen = rt.run(|fil| {
                let seen = late.function::<fn() -> Ptr<c_char>>(fil, "variable_seen").unwrap();
                seen.call(fil, ()).unwrap().read_string(fil).unwrap()
            });
            assert!(seen.starts_with("FIL_RUST_RT_LIBC_CHILD="), "{seen}");
        }
        "pthreads-and-collections" => {
            let rt = Runtime::start(config().collection(Collection::Manual)).unwrap();
            let library = rt.load(needs_libc()).unwrap();
            rt.run(|fil| {
                let run = library.function::<fn(fn(), i64) -> i64>(fil, "in_a_thread").unwrap();
                assert_eq!(run.call(fil, (collect(fil).get(fil), 21)).unwrap(), 42);
            });
            let churn = |count: i32| {
                rt.run(|fil| library.function::<fn(i32)>(fil, "churn_threads").unwrap().call(fil, (count,)).unwrap());
                rt.collect().unwrap();
                rt.heap_stats().unwrap().live_bytes
            };
            let baseline = churn(1);
            let after = churn(100);
            assert!(after <= baseline + (1 << 12), "{baseline} became {after}");
        }
        other => panic!("no case {other}"),
    }
}

#[test]
fn the_libc_comes_with_the_first_library_that_needs_it() {
    passes("auto");
}

#[test]
fn the_libc_comes_when_asked_for() {
    passes("asking-for-the-libc");
}

#[test]
fn the_libc_waits_for_threads_that_went_without() {
    passes("waits-for-runs");
}

#[test]
fn the_libc_can_be_ruled_out() {
    passes("never");
}

#[test]
fn the_libc_can_be_there_from_the_start() {
    passes("always");
}

#[test]
fn the_libc_cannot_arrive_under_a_running_thread() {
    passes("inside-run");
}

#[test]
fn constructors_see_the_configured_arguments() {
    passes("constructors-see-the-arguments");
}

#[test]
fn late_constructors_get_the_environment_the_program_started_with() {
    passes("late-constructors-see-the-environment");
}

#[test]
fn pthreads_work_across_collections_and_go_away() {
    passes("pthreads-and-collections");
}
