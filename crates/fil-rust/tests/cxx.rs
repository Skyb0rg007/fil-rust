//! C++ libraries behind a C ABI: containers, threads, and exceptions.

mod common;

use std::ffi::c_char;

use common::runtime;
use fil_rust::{Error, Opaque, Ptr};

fil_rust::foreign! {
    struct Cxx {
        fn checked_div(a: i32, b: i32) -> i32;
        fn catch_inside(x: i32) -> i32;
        fn call_and_catch(f: fn(i32) -> i32, x: i32) -> i32;
        fn counter_new() -> Ptr<Opaque>;
        fn counter_push(c: Ptr<Opaque>, v: i32);
        fn counter_sum(c: Ptr<Opaque>) -> i64;
        fn counter_name(c: Ptr<Opaque>, name: Ptr<c_char>, v: i32);
        fn counter_lookup(c: Ptr<Opaque>, name: Ptr<c_char>) -> i32;
        fn counter_free(c: Ptr<Opaque>);
        fn join_sorted(words: Ptr<Ptr<c_char>>, n: i32) -> Ptr<c_char>;
        fn parallel_sum(nthreads: i32, per: i32) -> i64;
        fn throw_with_guards(depth: i32) -> i32;
        fn get_destructor_runs() -> i32;
        fn throw_int(v: i32) -> i32;
        fn guarded_call(f: fn(i32) -> i32, x: i32) -> i32;
        fn call_on_thread(f: fn(i32) -> i32, x: i32) -> i32;
    }
}

/// Held by the tests that count destructor runs (a global in the fixture).
/// Taken outside `run`: a thread must not block while it is in Fil-C.
static DESTRUCTORS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cxx() -> &'static Cxx {
    static CXX: std::sync::OnceLock<Cxx> = std::sync::OnceLock::new();
    CXX.get_or_init(|| runtime().run(|fil| Cxx::load(fil, common::fixture("cxx.cpp")).unwrap()))
}

#[test]
fn containers_behind_a_c_api() {
    let c = cxx();
    runtime().run(|fil| {
        let counter = c.counter_new(fil).unwrap();
        for v in 1..=100 {
            c.counter_push(fil, counter, v).unwrap();
        }
        assert_eq!(c.counter_sum(fil, counter).unwrap(), 5050);
        c.counter_name(fil, counter, fil.c_str("answer").unwrap(), 42).unwrap();
        assert_eq!(c.counter_lookup(fil, counter, fil.c_str("answer").unwrap()).unwrap(), 42);
        assert_eq!(c.counter_lookup(fil, counter, fil.c_str("missing").unwrap()).unwrap(), -1);
        c.counter_free(fil, counter).unwrap();
        // The object is gone; Rust can't touch it any more.
        assert_eq!(counter.cast::<u8>().read(fil).unwrap_err(), Error::Freed);
    });
}

#[test]
fn strings_and_sorting() {
    let c = cxx();
    runtime().run(|fil| {
        let words = ["pear", "apple", "fig"];
        let arr = fil.alloc_zeroed::<Ptr<c_char>>(words.len()).unwrap();
        for (i, w) in words.iter().enumerate() {
            arr.write_at(fil, i, &fil.c_str(w).unwrap()).unwrap();
        }
        let joined = c.join_sorted(fil, arr, 3).unwrap();
        assert_eq!(joined.read_string(fil).unwrap(), "apple,fig,pear");
    });
}

#[test]
fn std_thread() {
    let c = cxx();
    let n = 8;
    let per = 10_000;
    let expected: i64 = (0..(n * per) as i64).sum();
    assert_eq!(runtime().run(|fil| c.parallel_sum(fil, n, per).unwrap()), expected);
}

#[test]
fn exceptions_caught_in_cxx() {
    let c = cxx();
    runtime().run(|fil| {
        assert_eq!(c.catch_inside(fil, 5).unwrap(), 5);
        assert_eq!(c.catch_inside(fil, -5).unwrap(), -1);
        assert_eq!(c.checked_div(fil, 10, 3).unwrap(), 3);
    });
}

#[test]
fn exceptions_escaping_to_rust_become_errors() {
    let c = cxx();
    for _ in 0..3 {
        runtime().run(|fil| {
            assert_eq!(c.checked_div(fil, 1, 0).unwrap_err(), Error::Exception);
            // The thread is still healthy: exceptions keep working.
            assert_eq!(c.catch_inside(fil, -1).unwrap(), -1);
            assert_eq!(c.checked_div(fil, 9, 3).unwrap(), 3);
        });
    }
}

#[test]
fn callbacks_called_from_cxx() {
    let c = cxx();
    runtime().run(|fil| {
        let cb = fil.callback::<fn(i32) -> i32, _>(|_, (x,)| x * 3).unwrap();
        assert_eq!(c.call_and_catch(fil, cb.get(fil), 4).unwrap(), 12);
    });
}

#[test]
fn destructors_run_while_unwinding_to_rust() {
    let c = cxx();
    let _counting = DESTRUCTORS.lock().unwrap();
    runtime().run(|fil| {
        let before = c.get_destructor_runs(fil).unwrap();
        assert_eq!(c.throw_with_guards(fil, 10).unwrap_err(), Error::Exception);
        assert_eq!(c.get_destructor_runs(fil).unwrap() - before, 11);
        // Non-std::exception types too.
        assert_eq!(c.throw_int(fil, 5).unwrap_err(), Error::Exception);
    });
}

#[test]
fn exceptions_stop_at_every_rust_boundary() {
    let c = cxx();
    runtime().run(|fil| {
        // C++ (with a catch) -> Rust callback -> C++ that throws: the
        // exception is reported to the callback, and never crosses Rust frames.
        let cb = fil
            .callback::<fn(i32) -> i32, _>(move |fil, (x,)| match c.checked_div(fil, x, 0) {
                Err(Error::Exception) => -7,
                other => panic!("unexpected {other:?}"),
            })
            .unwrap();
        assert_eq!(c.call_and_catch(fil, cb.get(fil), 4).unwrap(), -7);
    });
}

#[test]
fn exceptions_on_many_threads() {
    let c = cxx();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                for i in 0..50 {
                    runtime().run(|fil| {
                        assert_eq!(c.checked_div(fil, i, 0).unwrap_err(), Error::Exception);
                        assert_eq!(c.checked_div(fil, i * 2, 2).unwrap(), i);
                    });
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn exceptions_between_nested_cxx_and_rust_frames() {
    let c = cxx();
    let _counting = DESTRUCTORS.lock().unwrap();
    runtime().run(|fil| {
        // guarded_call (C++) -> outer (Rust) -> guarded_call (C++) -> inner
        // (Rust) -> throw_with_guards (C++, throws through 3 guards). Each
        // exception stops at the nearest Rust frame; every guard runs once.
        let inner = std::sync::Arc::new(
            fil.callback::<fn(i32) -> i32, _>(move |fil, (x,)| match c.throw_with_guards(fil, 2) {
                Err(Error::Exception) => x + 100,
                other => panic!("unexpected {other:?}"),
            })
            .unwrap(),
        );
        let i2 = inner.clone();
        let outer = fil
            .callback::<fn(i32) -> i32, _>(move |fil, (x,)| c.guarded_call(fil, i2.get(fil), x).unwrap() * 10)
            .unwrap();
        let before = c.get_destructor_runs(fil).unwrap();
        assert_eq!(c.guarded_call(fil, outer.get(fil), 5).unwrap(), 1050);
        assert_eq!(c.get_destructor_runs(fil).unwrap() - before, 3 + 2);
        drop(inner);
    });
}

#[test]
fn callbacks_on_cxx_threads() {
    let c = cxx();
    runtime().run(|fil| {
        // A Rust callback on a thread std::thread made, calling C++ that
        // throws: the exception stops at the callback, which returns normally.
        let cb = fil
            .callback::<fn(i32) -> i32, _>(move |fil, (x,)| match c.throw_int(fil, x) {
                Err(Error::Exception) => -x,
                other => panic!("unexpected {other:?}"),
            })
            .unwrap();
        assert_eq!(c.call_on_thread(fil, cb.get(fil), 4).unwrap(), -4);
        let plain = fil.callback::<fn(i32) -> i32, _>(|_, (x,)| x * 3).unwrap();
        for x in 0..20 {
            assert_eq!(c.call_on_thread(fil, plain.get(fil), x).unwrap(), x * 3);
        }
    });
}
