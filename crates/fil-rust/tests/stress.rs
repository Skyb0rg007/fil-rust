//! Concurrency stress: many threads loading libraries, sharing handles and
//! buffers, and churning scopes while the collector runs. `FIL_RUST_STRESS`
//! multiplies the iteration counts; the Nix `gc-stress` test runs these with
//! the collector running constantly.

mod common;

use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use common::{basic, fixture, runtime};
use fil_rust::{FilHandle, Ptr};

fn rounds(base: usize) -> usize {
    base * std::env::var("FIL_RUST_STRESS").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
}

/// Runs `body(thread, round)` on `threads` threads at once.
fn on_threads(threads: usize, body: impl Fn(usize) + Send + Sync + 'static) {
    let body = Arc::new(body);
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (body, start) = (body.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                body(t)
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// A thread that collects garbage until told to stop.
struct Collector {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<usize>>,
}

impl Collector {
    fn start() -> Collector {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut n = 0;
            while !s.load(Ordering::Relaxed) {
                runtime().run(|fil| fil.collect_garbage().unwrap());
                n += 1;
            }
            n
        });
        Collector { stop, thread: Some(thread) }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let n = self.thread.take().unwrap().join().unwrap();
        assert!(n > 0 || std::thread::panicking());
    }
}

#[test]
fn libraries_are_loaded_from_many_threads_at_once() {
    let rt = runtime();
    let paths = [fixture("basic.c"), fixture("misuse.c"), fixture("layouts.c")];
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    on_threads(8, move |t| {
        for i in 0..rounds(10) {
            let path = &paths[(t + i) % paths.len()];
            // Loading a library that is already loaded returns it again.
            let lib = rt.load(path).unwrap();
            rt.run(|fil| match path.file_stem().unwrap().to_str().unwrap() {
                "libbasic" => {
                    let add = lib.function::<fn(i32, i32) -> i32>(fil, "add").unwrap();
                    assert_eq!(add.call(fil, (t as i32, i as i32)).unwrap(), (t + i) as i32);
                }
                "libmisuse" => {
                    let twice = lib.function::<fn(i32) -> i32>(fil, "twice").unwrap();
                    assert_eq!(twice.call(fil, (i as i32,)).unwrap(), 2 * i as i32);
                }
                _ => {
                    let inc = lib.function::<fn(i32) -> i32>(fil, "inc").unwrap();
                    assert_eq!(inc.call(fil, (i as i32,)).unwrap(), i as i32 + 1);
                }
            });
            c.fetch_add(1, Ordering::Relaxed);
        }
    });
    assert_eq!(calls.load(Ordering::Relaxed), 8 * rounds(10));
}

#[test]
fn a_shared_library_value_is_used_from_many_threads() {
    let rt = runtime();
    let lib = Arc::new(rt.load(fixture("basic.c")).unwrap());
    let _gc = Collector::start();
    on_threads(8, move |t| {
        for i in 0..rounds(50) {
            rt.run(|fil| {
                let s = fil.c_str(&format!("t{t}-{i}")).unwrap();
                let strlen = lib.function::<fn(Ptr<c_char>) -> usize>(fil, "my_strlen").unwrap();
                assert_eq!(strlen.call(fil, (s,)).unwrap(), format!("t{t}-{i}").len());
            });
        }
    });
}

#[test]
fn one_handle_is_read_by_many_threads_while_collecting() {
    let rt = runtime();
    let values: Vec<i64> = (0..256).collect();
    let handle: Arc<FilHandle<i64>> =
        Arc::new(rt.run(|fil| fil.root(fil.alloc_array::<i64>(&values).unwrap()).unwrap()));
    let _gc = Collector::start();
    let h = handle.clone();
    on_threads(8, move |t| {
        for i in 0..rounds(100) {
            let at = (t * 31 + i) % 256;
            let v = rt.run(|fil| h.get(fil).read_at(fil, at).unwrap());
            assert_eq!(v, at as i64);
        }
    });
    // The last reference goes away on a thread that never entered Fil-C.
    std::thread::spawn(move || drop(handle)).join().unwrap();
}

#[test]
fn handles_are_created_and_dropped_on_different_threads() {
    let rt = runtime();
    let _gc = Collector::start();
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, FilHandle<c_char>)>(64);
    let producers: Vec<_> = (0..4)
        .map(|t| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for i in 0..rounds(200) {
                    let h = rt.run(|fil| fil.root(fil.c_str(&format!("{t}:{i}")).unwrap()).unwrap());
                    tx.send((t * 100_000 + i, h)).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    // Half are checked in the runtime, half dropped unchecked.
    let mut checked = 0;
    for (id, h) in rx {
        if id % 2 == 0 {
            let s = rt.run(|fil| h.get(fil).read_string(fil).unwrap());
            assert_eq!(s, format!("{}:{}", id / 100_000, id % 100_000));
            checked += 1;
        }
    }
    for p in producers {
        p.join().unwrap();
    }
    assert_eq!(checked, 4 * rounds(200) / 2);
}

#[test]
fn buffers_filled_in_rust_are_shared_with_c_from_many_threads() {
    let rt = runtime();
    let b = basic();
    let _gc = Collector::start();
    on_threads(8, move |t| {
        for i in 0..rounds(50) {
            let len = 1 + (t * 7 + i * 13) % 300;
            let mut buf = rt.run(|fil| fil.buf(len).unwrap());
            for (k, byte) in buf.iter_mut().enumerate() {
                *byte = (k % 7) as u8;
            }
            let expected: i32 = (0..len).map(|k| (k % 7) as i32).sum();
            rt.run(|fil| {
                let p = buf.share(fil);
                assert_eq!(b.sum_bytes(fil, p, len).unwrap(), expected);
                // C can't read past what Rust allocated.
                assert!(p.read_at(fil, len.next_multiple_of(16) + 16).is_err());
            });
        }
    });
}

#[test]
fn nested_scopes_churn_while_collecting() {
    let rt = runtime();
    let b = basic();
    let _gc = Collector::start();
    on_threads(6, move |t| {
        for i in 0..rounds(30) {
            rt.run(|fil| {
                let outer = fil.alloc_array::<i32>(&[t as i32; 8]).unwrap();
                for depth in 0..5 {
                    fil.scope(|inner| {
                        let words = inner.c_str(&format!("{t} {i} {depth} words here")).unwrap();
                        let count = inner.alloc::<i32>(&0).unwrap();
                        let split = b.split_words(inner, words, count).unwrap();
                        assert_eq!(count.read(inner).unwrap(), 5);
                        assert!(b.total_len(inner, split).unwrap() > 0);
                        b.churn(inner, 1, 128).unwrap();
                    });
                }
                // The outer allocation survived every inner scope.
                assert_eq!(outer.read_array(fil, 8).unwrap(), vec![t as i32; 8]);
            });
        }
    });
}

#[test]
fn jobs_spawned_from_jobs_under_load() {
    let rt = runtime();
    let _gc = Collector::start();
    on_threads(4, move |t| {
        let total: usize = rt.run(|_| {
            let jobs: Vec<_> = (0..rounds(8))
                .map(|i| {
                    rt.spawn(move |fil| {
                        let s = fil.c_str(&"x".repeat(i + t)).unwrap();
                        s.read_string(fil).unwrap().len()
                    })
                })
                .collect();
            // Joining inside a job leaves the runtime while it waits.
            jobs.into_iter().map(|j| j.join()).sum()
        });
        assert_eq!(total, (0..rounds(8)).map(|i| i + t).sum());
    });
}
