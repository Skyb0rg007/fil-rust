//! The collector, driven from Rust. Each kind of memory has to survive a
//! collection while something refers to it, and go away once nothing does.
//!
//! `FIL_RUST_GC_STRESS` runs these with a collection at every safepoint and
//! swept memory scribbled over. What depends on when collections happen doesn't
//! apply then, and is skipped.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use common::{fixture, flavor, without_core_dumps};
use fil_rust::{Backend, Collection, Config, Error, Fil, FilHandle, Library, Ptr, Runtime};

fn stressed() -> bool {
    std::env::var_os("FIL_RUST_GC_STRESS").is_some()
}

fn config() -> Config {
    Config::from_env_for(flavor()).unwrap().runtime(Backend::Rust)
}

fn runtime() -> Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    *RUNTIME.get_or_init(|| Runtime::start(config().collection(Collection::Manual)).unwrap())
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("rt_gc.c")).unwrap())
}

/// Tests look at the state of the whole heap, so they take turns. Take this
/// before `run`: waiting for it inside would hold up a collection. Loading the
/// library here keeps that from happening in `library`, too.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    library();
    guard
}

type Collect = fn();

/// Makes the callback the fixture collects through.
fn collector(fil: &Fil<'_>) -> fil_rust::Callback<Collect> {
    fil.callback::<Collect, _>(|fil, ()| fil.collect_garbage().unwrap()).unwrap()
}

/// What's left in the heap after a collection.
fn live_after_collecting() -> usize {
    let rt = runtime();
    rt.collect().unwrap();
    rt.heap_stats().unwrap().live_bytes
}

#[test]
fn what_compiled_frames_hold_survives() {
    let _serial = serial();
    runtime().run(|fil| {
        let collect = collector(fil);
        for name in ["frame_slot", "escaping_local", "atomic_box", "closure_data", "thread_local", "aligned"] {
            let function = library().function::<fn(Collect, i64) -> i64>(fil, name).unwrap();
            assert_eq!(function.call(fil, (collect.get(fil), 1234)).unwrap(), 1234, "{name}");
        }
        let stack_aux = library().function::<fn(Collect, i64, i32) -> i64>(fil, "stack_aux").unwrap();
        assert_eq!(stack_aux.call(fil, (collect.get(fil), 1000, 8)).unwrap(), 8028);
    });
}

#[test]
fn arguments_survive_in_their_promoted_copies() {
    let _serial = serial();
    runtime().run(|fil| {
        let collect = collector(fil);
        let by_value = library().function::<fn(Collect, i64) -> i64>(fil, "by_value").unwrap();
        assert_eq!(by_value.call(fil, (collect.get(fil), 41)).unwrap(), 42);
        let variadic = library().function::<fn(Collect, i64) -> i64>(fil, "variadic").unwrap();
        assert_eq!(variadic.call(fil, (collect.get(fil), 42)).unwrap(), 42);
    });
}

/// Far longer than a recursive marker would get on any stack.
#[test]
fn long_chains_survive() {
    let _serial = serial();
    if stressed() {
        return;
    }
    runtime().run(|fil| {
        let collect = collector(fil);
        let chain = library().function::<fn(Collect, i64) -> i64>(fil, "chain").unwrap();
        let length = 2_000_000;
        assert_eq!(chain.call(fil, (collect.get(fil), length)).unwrap(), length * (length - 1) / 2);
    });
}

#[test]
fn globals_hold_on_until_they_let_go() {
    let _serial = serial();
    let rt = runtime();
    let call = |name: &str| rt.run(|fil| library().function::<fn()>(fil, name).unwrap().call(fil, ()).unwrap());
    let read = || rt.run(|fil| library().function::<fn() -> i64>(fil, "read_global").unwrap().call(fil, ()).unwrap());
    rt.run(|fil| library().function::<fn(i64)>(fil, "set_global").unwrap().call(fil, (77,)).unwrap());
    rt.collect().unwrap();
    assert_eq!(read(), 77);
    call("clear_global");
    rt.collect().unwrap();
    rt.run(|fil| library().function::<fn(i64)>(fil, "set_global").unwrap().call(fil, (78,)).unwrap());
    assert_eq!(read(), 78);
}

#[test]
fn rust_keeps_what_it_tracks_and_roots() {
    let _serial = serial();
    let rt = runtime();
    let handle: FilHandle<i64> = rt.run(|fil| {
        let tracked = fil.alloc::<i64>(&5).unwrap();
        fil.collect_garbage().unwrap();
        assert_eq!(tracked.read(fil).unwrap(), 5);
        fil.root(fil.alloc::<i64>(&6).unwrap()).unwrap()
    });
    rt.collect().unwrap();
    assert_eq!(rt.run(|fil| handle.get(fil).read(fil).unwrap()), 6);
}

#[test]
fn memory_goes_away_when_nothing_refers_to_it() {
    let _serial = serial();
    if stressed() {
        return;
    }
    let rt = runtime();
    let churn = |name: &str, rounds: i64| {
        rt.run(|fil| library().function::<fn(i64) -> i64>(fil, name).unwrap().call(fil, (rounds,)).unwrap())
    };
    // Globals come into being on first use.
    churn("churn_locals", 1);
    churn("churn_calls", 1);
    let baseline = live_after_collecting();
    for (name, rounds) in [("churn_locals", 100_000), ("churn_calls", 100_000)] {
        let before = rt.heap_stats().unwrap().total_reclaimed_bytes;
        churn(name, rounds);
        assert!(rt.heap_stats().unwrap().allocated_bytes > 1 << 20, "{name} allocated nothing");
        assert_eq!(live_after_collecting(), baseline, "{name}");
        assert!(rt.heap_stats().unwrap().total_reclaimed_bytes - before > 1 << 20, "{name}");
    }
    let frees = rt.run(|fil| library().function::<fn(i64)>(fil, "churn_freed").unwrap().call(fil, (100_000,)));
    frees.unwrap();
    assert_eq!(live_after_collecting(), baseline, "churn_freed");
    for size in [64, 100_000, 1 << 21] {
        let malloc = rt.run(|fil| {
            let churn = library().function::<fn(i64, usize) -> i64>(fil, "churn_malloc").unwrap();
            churn.call(fil, (200, size)).unwrap()
        });
        assert_eq!(malloc, 200);
        assert_eq!(live_after_collecting(), baseline, "blocks of {size}");
    }
}

#[test]
fn rust_references_end_with_their_scope() {
    let _serial = serial();
    if stressed() {
        return;
    }
    let rt = runtime();
    let baseline = live_after_collecting();
    rt.run(|fil| {
        fil.scope(|fil| {
            fil.alloc_bytes(1 << 22).unwrap();
            fil.collect_garbage().unwrap();
            assert!(rt.heap_stats().unwrap().live_bytes >= baseline + (1 << 22));
        });
    });
    assert_eq!(live_after_collecting(), baseline);
    let handle: FilHandle<u8> = rt.run(|fil| fil.root(fil.alloc_bytes(1 << 22).unwrap()).unwrap());
    assert!(live_after_collecting() >= baseline + (1 << 22));
    drop(handle);
    assert_eq!(live_after_collecting(), baseline);
}

#[test]
fn nothing_is_reclaimed_until_rust_asks() {
    let _serial = serial();
    if stressed() {
        return;
    }
    let rt = runtime();
    rt.collect().unwrap();
    let collections = rt.heap_stats().unwrap().collections;
    rt.set_collection(Collection::Never).unwrap();
    let churn = |rounds: i64| {
        rt.run(|fil| library().function::<fn(i64) -> i64>(fil, "churn_locals").unwrap().call(fil, (rounds,)).unwrap());
    };
    churn(100_000);
    let grown = rt.heap_stats().unwrap().allocated_bytes;
    assert!(grown > 1 << 20, "{grown}");
    churn(100_000);
    assert!(rt.heap_stats().unwrap().allocated_bytes > grown + (1 << 20));
    rt.collect().unwrap();
    assert_eq!(rt.heap_stats().unwrap().collections, collections, "a collection under Never");
    rt.set_collection(Collection::Manual).unwrap();
    churn(100_000);
    assert_eq!(rt.heap_stats().unwrap().collections, collections, "a collection nobody asked for");
    rt.collect().unwrap();
    assert_eq!(rt.heap_stats().unwrap().collections, collections + 1);
    assert_eq!(rt.heap_stats().unwrap().allocated_bytes, 0);
}

#[test]
fn compiled_loops_stop_for_collections() {
    let _serial = serial();
    if stressed() {
        return;
    }
    let rt = runtime();
    let collections = rt.heap_stats().unwrap().collections;
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for value in 0..3 {
            let done = &done;
            scope.spawn(move || {
                rt.run(|fil| {
                    let stop = fil.alloc::<i32>(&0).unwrap();
                    let stop_handle = fil.root(stop).unwrap();
                    let run = library().function::<fn(Ptr<i32>, i64) -> i64>(fil, "allocate_until").unwrap();
                    std::thread::scope(|inner| {
                        let handle = &stop_handle;
                        inner.spawn(move || {
                            while !done.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            rt.run(|fil| handle.get(fil).write(fil, &1).unwrap());
                        });
                        assert_eq!(run.call(fil, (stop, value)).unwrap(), value);
                    });
                });
            });
        }
        let start = Instant::now();
        let rounds = || {
            rt.run(|fil| {
                let rounds = library().global::<i64>(fil, "rounds").unwrap();
                rounds.read(fil).unwrap()
            })
        };
        // Enough to be sure they got going, with collections all along.
        let goal = rounds() + 20_000;
        while rt.heap_stats().unwrap().collections < collections + 20 || rounds() < goal {
            assert!(start.elapsed() < Duration::from_secs(60));
            rt.collect().unwrap();
            // Collecting back to back would leave them no time to run at all.
            std::thread::sleep(Duration::from_millis(1));
        }
        done.store(true, Ordering::Relaxed);
    });
}

#[test]
fn exited_threads_are_collected_around() {
    let _serial = serial();
    let rt = runtime();
    let exited = AtomicUsize::new(0);
    let release = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            rt.run(|fil| {
                let value = fil.alloc::<i64>(&11).unwrap();
                fil.exited(|| {
                    exited.store(1, Ordering::Release);
                    while !release.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                });
                assert_eq!(value.read(fil).unwrap(), 11);
            });
        });
        while exited.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        rt.collect().unwrap();
        release.store(true, Ordering::Release);
    });
}

#[test]
fn references_to_freed_objects_are_let_go() {
    let _serial = serial();
    let rt = runtime();
    rt.run(|fil| {
        let make = library().function::<fn(i64, usize) -> Ptr<u8>>(fil, "make_holder").unwrap();
        let holder = fil.root(make.call(fil, (31, 1 << 22)).unwrap()).unwrap();
        let field = library().function::<fn(Ptr<u8>, i32) -> Ptr<Ptr<i64>>>(fil, "freed_field").unwrap();
        let fields = [0, 1].map(|boxed| fil.root(field.call(fil, (holder.get(fil), boxed)).unwrap()).unwrap());
        // In a scope of its own: a reference Rust holds would keep the object.
        let lower = |field: &FilHandle<Ptr<i64>>| {
            fil.scope(|fil| field.get(fil).read(fil).unwrap().capability().unwrap().lower)
        };
        let before = fields.each_ref().map(lower);
        assert_eq!(before[0], before[1]);
        fil.collect_garbage().unwrap();
        if !stressed() {
            let stats = rt.heap_stats().unwrap();
            assert!(stats.reclaimed_bytes >= 1 << 22, "{stats:?}");
        }
        let read = library().function::<fn(Ptr<u8>) -> i64>(fil, "read_kept").unwrap();
        assert_eq!(read.call(fil, (holder.get(fil),)).unwrap(), 31);
        // Each field still reads as a pointer to something free, but not the
        // object whose memory is gone, which something else can now get.
        for (field, before) in fields.iter().zip(before) {
            let freed = field.get(fil).read(fil).unwrap();
            assert_ne!(freed.capability().unwrap().lower, before);
            assert_eq!(freed.add(1).read(fil), Err(Error::Freed));
        }
    });
}

#[test]
fn freed_objects_with_a_reference_on_the_stack_stay_tombstones() {
    let _serial = serial();
    let rt = runtime();
    rt.run(|fil| {
        let block = fil.alloc::<i64>(&1).unwrap();
        let free = fil.runtime().runtime_library().function::<fn(Ptr<i64>)>(fil, "zgc_free").unwrap();
        free.call(fil, (block,)).unwrap();
        fil.collect_garbage().unwrap();
        assert_eq!(block.read(fil), Err(Error::Freed));
        if !stressed() {
            assert!(rt.heap_stats().unwrap().tombstoned_bytes > 0);
        }
    });
}

#[test]
fn the_threshold_collects_without_being_asked() {
    if stressed() {
        return;
    }
    const CHILD: &str = "FIL_RUST_RT_GC_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let policy = Collection::Threshold { min_bytes: 1 << 20, growth_percent: 100 };
        let rt = Runtime::start(config().collection(policy)).unwrap();
        let library = rt.load(fixture("rt_gc.c")).unwrap();
        rt.run(|fil| {
            let churn = library.function::<fn(i64, usize) -> i64>(fil, "churn_malloc").unwrap();
            churn.call(fil, (50_000, 1024)).unwrap();
        });
        let stats = rt.heap_stats().unwrap();
        assert!(stats.collections > 10, "{stats:?}");
        assert!(stats.committed_bytes < 16 << 20, "{stats:?}");
        return;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "the_threshold_collects_without_being_asked", "--nocapture"]).env(CHILD, "1");
    let output = without_core_dumps(&mut command).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
