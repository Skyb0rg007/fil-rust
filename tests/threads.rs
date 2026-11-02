//! Concurrency: many Rust threads using the runtime at once, background jobs,
//! leaving the runtime while blocking, and runtime configuration.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::{basic, runtime};
use fil_rust::{Error, Runtime};

#[test]
fn many_rust_threads_share_a_fil_c_mutex() {
    let rt = runtime();
    let b = basic();
    let start = rt.run(|fil| b.locked_total(fil).unwrap());
    let threads: Vec<_> = (0..16)
        .map(|_| {
            std::thread::spawn(move || {
                for _ in 0..200 {
                    rt.run(|fil| b.locked_add(fil, 1).unwrap());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(rt.run(|fil| b.locked_total(fil).unwrap()) - start, 16 * 200);
}

#[test]
fn spawned_jobs_run_concurrently() {
    let rt = runtime();
    let n = rt.config().worker_count().min(4);
    // `n` jobs that each wait for all the others prove they run in parallel.
    let barrier = Arc::new(Barrier::new(n));
    let jobs: Vec<_> = (0..n)
        .map(|i| {
            let barrier = barrier.clone();
            rt.spawn(move |fil| {
                fil.exited(|| {
                    barrier.wait();
                });
                i * 10
            })
        })
        .collect();
    let mut results: Vec<usize> = jobs.into_iter().map(|j| j.join()).collect();
    results.sort();
    assert_eq!(results, (0..n).map(|i| i * 10).collect::<Vec<_>>());
}

#[test]
fn blocking_while_exited_does_not_stall_the_collector() {
    let rt = runtime();
    let release = Arc::new(AtomicBool::new(false));
    let r2 = release.clone();
    // This job leaves the runtime and blocks for a while.
    let blocker = rt.spawn(move |fil| {
        let p = fil.alloc::<u32>(&99).unwrap();
        fil.exited(move || {
            while !r2.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        // Pointers from before `exited` are still valid.
        p.read(fil).unwrap()
    });
    // Meanwhile collections complete.
    let t = Instant::now();
    for _ in 0..3 {
        rt.run(|fil| fil.collect_garbage().unwrap());
    }
    assert!(t.elapsed() < Duration::from_secs(20));
    release.store(true, Ordering::Release);
    assert_eq!(blocker.join(), 99);
}

#[test]
fn long_rust_loops_with_pollcheck() {
    let rt = runtime();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let spinner = rt.spawn(move |fil| {
        let mut n = 0u64;
        while !s2.load(Ordering::Relaxed) {
            n += 1;
            if n % 1024 == 0 {
                fil.pollcheck();
            }
        }
        n
    });
    rt.run(|fil| fil.collect_garbage().unwrap());
    stop.store(true, Ordering::Relaxed);
    assert!(spinner.join() > 0);
}

#[test]
fn run_nests_inline() {
    let rt = runtime();
    let b = basic();
    let r = rt.run(|outer| {
        let p = outer.alloc::<i32>(&5).unwrap();
        let inner_result = rt.run(|inner| b.add(inner, 1, 2).unwrap());
        // The outer scope's pointers are untouched by the nested scope.
        p.read(outer).unwrap() + inner_result
    });
    assert_eq!(r, 8);
}

#[test]
fn panics_propagate_to_the_caller() {
    let rt = runtime();
    let r = std::panic::catch_unwind(|| rt.run(|_fil| panic!("boom in a job")));
    let msg = r.unwrap_err();
    assert_eq!(msg.downcast_ref::<&str>(), Some(&"boom in a job"));
    // The executor survives.
    assert_eq!(rt.run(|fil| basic().add(fil, 1, 1).unwrap()), 2);
    let r = std::panic::catch_unwind(|| rt.spawn(|_fil| panic!("boom in a spawned job")).join());
    assert!(r.is_err());
    assert_eq!(rt.run(|fil| basic().add(fil, 2, 2).unwrap()), 4);
}

#[test]
fn borrowing_from_the_caller() {
    let rt = runtime();
    let words = vec!["alpha".to_string(), "beta".to_string()];
    let mut lengths = Vec::new();
    // `run` blocks, so the job may borrow (even mutably) from the caller.
    rt.run(|fil| {
        for w in &words {
            let s = fil.c_str(w).unwrap();
            lengths.push(basic().my_strlen(fil, s).unwrap());
        }
    });
    assert_eq!(lengths, vec![5, 4]);
}

#[test]
fn join_timeout() {
    let rt = runtime();
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let job = rt.spawn(move |fil| {
        fil.exited(|| {
            while !g2.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        7
    });
    let job = job.join_timeout(Duration::from_millis(20)).unwrap_err();
    gate.store(true, Ordering::Release);
    assert_eq!(job.join_timeout(Duration::from_secs(30)).ok(), Some(7));
}

#[test]
fn runtime_is_a_process_singleton() {
    let rt = runtime();
    assert!(Runtime::get().is_some());
    assert_eq!(Runtime::start(common::config()).unwrap().flavor(), rt.flavor());
    let other = fil_rust::Config::new(rt.flavor(), "/somewhere/else");
    assert_eq!(Runtime::start(other).unwrap_err(), Error::AlreadyStarted);
}

#[test]
fn stress_mixed_workload() {
    let rt = runtime();
    let b = basic();
    let done = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let done = done.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    match (t + i) % 4 {
                        0 => rt.run(|fil| {
                            let h = b.list_build(fil, 50).unwrap();
                            assert_eq!(b.list_sum(fil, h).unwrap(), 1275);
                            b.list_free(fil, h).unwrap();
                        }),
                        1 => rt.run(|fil| {
                            let cb = fil.callback::<fn(i32) -> i32, _>(|_, (x,)| x ^ 0x55).unwrap();
                            let apply = b.library().function::<fn(fn(i32) -> i32, i32) -> i32>(fil, "apply").unwrap();
                            assert_eq!(apply.call(fil, (cb.get(fil), i as i32)).unwrap(), i as i32 ^ 0x55);
                        }),
                        2 => {
                            let h = rt.run(|fil| fil.root(fil.c_str("rooted").unwrap()).unwrap());
                            rt.run(|fil| assert_eq!(h.get(fil).read_string(fil).unwrap(), "rooted"));
                        }
                        _ => rt.run(|fil| {
                            if i % 10 == 0 {
                                fil.collect_garbage().unwrap();
                            }
                            b.churn(fil, 2, 64).unwrap();
                        }),
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(done.load(Ordering::Relaxed), 8);
}
