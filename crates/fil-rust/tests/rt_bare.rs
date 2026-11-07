//! Libraries with no libc imports, hosted by the Rust runtime on Rust's own
//! threads. `FIL_RUST_RUNTIME=pizlo` runs what isn't specific to that hosting
//! on libpizlo instead, as a check that the two behave alike.

mod common;

use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, flavor};
use fil_rust::{Backend, Config, Error, FilHandle, FilPtr, Libc, Library, Ptr, Runtime};

fn backend() -> Backend {
    match std::env::var("FIL_RUST_RUNTIME").as_deref() {
        Ok("pizlo") => Backend::Pizlo,
        _ => Backend::Rust,
    }
}

fn config() -> Config {
    Config::from_env_for(flavor()).unwrap().runtime(backend())
}

fn runtime() -> Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    *RUNTIME.get_or_init(|| Runtime::start(config()).unwrap())
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("rt_bare.c")).unwrap())
}

/// Whether this process is the child some test started, and which part to run.
const CHILD: &str = "FIL_RUST_RT_BARE_CHILD";

fn run_child(case: &str) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1"]).env(CHILD, case);
    common::without_core_dumps(&mut command).output().unwrap()
}

/// Lines of the memory map that came from a file of the Fil-C distribution.
fn distribution_mappings() -> Vec<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let root = std::env::var("FILC_ROOT").unwrap();
    maps.lines().filter(|line| line.contains(&root)).map(str::to_string).collect()
}

fn thread_count() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|line| line.starts_with("Threads:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    match std::env::var(CHILD).unwrap().as_str() {
        "alone" => {
            let before = thread_count();
            let rt = runtime();
            let this = std::thread::current().id();
            let on_this_thread = rt.run(|fil| {
                let square = fil.callback::<fn(i32) -> i32, _>(|_, (value,)| value * value).unwrap();
                let apply = library().function::<fn(fn(i32) -> i32, i32) -> i32>(fil, "apply").unwrap();
                assert_eq!(apply.call(fil, (square.get(fil), 9)).unwrap(), 81);
                std::thread::current().id() == this
            });
            assert!(on_this_thread);
            assert_eq!(thread_count(), before);
            let mappings = distribution_mappings();
            assert!(mappings.is_empty(), "{mappings:#?}");
        }
        "unsupported_libc" => {
            let started = Runtime::start(config().libc(Libc::Always));
            assert!(matches!(started, Err(Error::Unsupported(_))), "{started:?}");
        }
        "past_the_end" => runtime().run(|fil| {
            let read = library().function::<fn() -> i32>(fil, "read_past_end").unwrap();
            let _ = read.call(fil, ());
        }),
        other => panic!("no case {other}"),
    }
}

#[test]
fn starts_nothing_and_maps_nothing_of_the_distribution() {
    if backend() == Backend::Rust {
        let output = run_child("alone");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}

#[test]
fn asking_for_a_libc_is_refused_until_there_is_one() {
    if backend() == Backend::Rust {
        let output = run_child("unsupported_libc");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}

#[test]
fn safety_failures_stop_the_process() {
    let output = run_child("past_the_end");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("filc panic"), "{stderr}");
    assert!(stderr.contains("read_past_end"), "{stderr}");
}

#[test]
fn a_library_that_needs_libc_is_rejected_by_name() {
    if backend() != Backend::Rust {
        return;
    }
    let path = fixture("rt_needs_libc.c");
    match runtime().load(path) {
        Err(Error::Load(message)) => assert!(message.contains("pizlonated_getpid"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn libraries_run_their_constructors_once() {
    let again = runtime().load(fixture("rt_bare.c")).unwrap();
    runtime().run(|fil| {
        let value = library().function::<fn() -> i32>(fil, "constructed_value").unwrap();
        assert_eq!(value.call(fil, ()).unwrap(), 42);
        assert!(again.has_symbol("constructed_value"));
    });
}

#[test]
fn calls_and_strings() {
    runtime().run(|fil| {
        let add = library().function::<fn(i32, i32) -> i32>(fil, "add").unwrap();
        assert_eq!(add.call(fil, (40, 2)).unwrap(), 42);
        let greeting = library().function::<fn() -> Ptr<c_char>>(fil, "greeting").unwrap();
        assert_eq!(greeting.call(fil, ()).unwrap().read_string(fil).unwrap(), "hello from Fil-C");
    });
}

#[test]
fn callbacks_get_and_return_pointers() {
    runtime().run(|fil| {
        let pass = library()
            .function::<fn(fn(Ptr<c_char>) -> Ptr<c_char>, Ptr<c_char>) -> Ptr<c_char>>(fil, "pass_through")
            .unwrap();
        let upper = fil
            .callback::<fn(Ptr<c_char>) -> Ptr<c_char>, _>(|fil, (text,)| {
                let text = text.read_string(fil).unwrap();
                fil.c_str(&text.to_uppercase()).unwrap()
            })
            .unwrap();
        let text = fil.c_str("quiet").unwrap();
        assert_eq!(pass.call(fil, (upper.get(fil), text)).unwrap().read_string(fil).unwrap(), "QUIET");
    });
}

#[test]
fn closures_carry_and_replace_their_data() {
    runtime().run(|fil| {
        let call = library().function::<fn(Ptr<i32>, Ptr<i32>, i32) -> i32>(fil, "call_closure").unwrap();
        let (first, second) = (fil.alloc::<i32>(&5).unwrap(), fil.alloc::<i32>(&6).unwrap());
        assert_eq!(call.call(fil, (first, second, 1)).unwrap(), 6007);
    });
}

#[test]
fn memory_errors_come_back_to_rust() {
    runtime().run(|fil| {
        let block = fil.alloc_zeroed::<i32>(4).unwrap();
        let literal = library().function::<fn() -> Ptr<u8>>(fil, "greeting").unwrap().call(fil, ()).unwrap();
        let function = library().symbol(fil, "add").unwrap();
        let push = library().function::<fn(Ptr<u8>, i64) -> Ptr<u8>>(fil, "push").unwrap();
        let release = library().function::<fn(Ptr<u8>)>(fil, "release").unwrap();
        let freed = push.call(fil, (FilPtr::null(), 1)).unwrap();
        release.call(fil, (freed,)).unwrap();

        assert_eq!(FilPtr::<i32>::null().read(fil), Err(Error::NullCapability));
        assert_eq!(block.add(4).read(fil), Err(Error::OutOfBounds));
        assert_eq!(freed.cast::<i32>().read(fil), Err(Error::Freed));
        assert_eq!(literal.write(fil, &0), Err(Error::ReadOnly));
        assert_eq!(function.cast::<u8>().read(fil), Err(Error::SpecialObject));
        assert_eq!(block.byte_add(1).cast::<Ptr<u8>>().read(fil).map(|_| ()), Err(Error::Misaligned));
        assert_eq!(function.cast::<u8>().byte_add(8).cast::<u8>().as_fn::<fn()>().map(|_| ()), Err(Error::NotAFunction));
    });
}

#[test]
fn pointers_pass_between_threads_in_handles() {
    let rt = runtime();
    let handle: FilHandle<i64> = rt.run(|fil| fil.root(fil.alloc::<i64>(&17).unwrap()).unwrap());
    let read = std::thread::spawn(move || rt.run(|fil| handle.get(fil).read(fil).unwrap()));
    assert_eq!(read.join().unwrap(), 17);
}

#[test]
fn several_threads_run_at_once() {
    let rt = runtime();
    let library = library();
    std::thread::scope(|scope| {
        for offset in 0..8i64 {
            scope.spawn(move || {
                for _ in 0..50 {
                    rt.run(|fil| {
                        let push = library.function::<fn(Ptr<u8>, i64) -> Ptr<u8>>(fil, "push").unwrap();
                        let sum = library.function::<fn(Ptr<u8>) -> i64>(fil, "sum").unwrap();
                        let release = library.function::<fn(Ptr<u8>)>(fil, "release").unwrap();
                        let mut head = FilPtr::null();
                        for value in 0..100 {
                            head = push.call(fil, (head, value + offset)).unwrap();
                        }
                        assert_eq!(sum.call(fil, (head,)).unwrap(), 4950 + 100 * offset);
                        release.call(fil, (head,)).unwrap();
                    });
                }
            });
        }
    });
}

#[test]
fn thread_locals_belong_to_the_thread() {
    // On libpizlo, `run` picks an executor.
    if backend() != Backend::Rust {
        return;
    }
    let rt = runtime();
    let library = library();
    let add = move |value| rt.run(|fil| library.function::<fn(i32) -> i32>(fil, "add_to_thread").unwrap().call(fil, (value,)).unwrap());
    assert_eq!(add(2), 2);
    assert_eq!(add(3), 5);
    assert_eq!(std::thread::spawn(move || add(10)).join().unwrap(), 10);
    assert_eq!(add(1), 6);
}

#[test]
fn spawned_jobs_come_and_go() {
    let rt = runtime();
    let library = library();
    for value in 0..200 {
        let job = rt.spawn(move |fil| {
            let add = library.function::<fn(i32, i32) -> i32>(fil, "add").unwrap();
            add.call(fil, (value, 1)).unwrap()
        });
        assert_eq!(job.join(), value + 1);
    }
}

#[test]
fn a_panic_leaves_the_thread_usable() {
    let rt = runtime();
    let panicked = catch_unwind(AssertUnwindSafe(|| rt.run(|_| panic!("on purpose"))));
    assert!(panicked.is_err());
    let nested = rt.run(|fil| {
        fil.exited(|| {
            let panicked = catch_unwind(AssertUnwindSafe(|| rt.run(|_| panic!("on purpose"))));
            assert!(panicked.is_err());
            rt.run(|fil| fil.alloc::<i32>(&1).unwrap().read(fil).unwrap())
        })
    });
    assert_eq!(nested, 1);
}

#[test]
fn the_runtime_library_answers_without_a_libc() {
    let rt = runtime();
    rt.run(|fil| {
        assert!(fil.version().unwrap() > 0);
        fil.collect_garbage().unwrap();
        fil.flush().unwrap();
    });
    assert_eq!(rt.libc().has_symbol("strlen"), backend() == Backend::Pizlo);
}

