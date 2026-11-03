//! The process ending, forking, or crashing while the runtime is busy. Each
//! case runs in a child process (this test binary re-executed with
//! `FIL_RUST_CHILD` naming the case), under a timeout: hanging is a failure.

mod common;

use std::ffi::c_char;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::{basic, fixture, runtime};
use fil_rust::{Callback, FilHandle};

struct Outcome {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn signal(&self) -> Option<i32> {
        std::os::unix::process::ExitStatusExt::signal(&self.status)
    }

    fn describe(&self) -> String {
        format!("status {:?}\n--- stdout\n{}\n--- stderr\n{}", self.status, self.stdout, self.stderr)
    }

    /// The child ran its case to the end and exited normally.
    fn assert_clean(&self) {
        assert!(self.status.success() && self.stdout.contains("case done"), "{}", self.describe());
    }

    /// The child was stopped by a Fil-C safety check mentioning `what`.
    fn assert_fil_c_panic(&self, what: &str) {
        assert!(!self.status.success(), "{}", self.describe());
        assert!(self.stderr.contains("filc safety error") || self.stderr.contains("filc panic"), "{}", self.describe());
        assert!(self.stderr.contains(what), "expected {what:?}: {}", self.describe());
    }
}

fn child(case: &str) -> Outcome {
    let mut c = common::without_core_dumps(&mut Command::new(std::env::current_exe().unwrap()))
        .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env("FIL_RUST_CHILD", case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drain the pipes while waiting: a child writing more than a pipe holds
    // (a Fil-C backtrace thousands of frames deep) would otherwise block.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            pipe.read_to_end(&mut s).unwrap();
            String::from_utf8_lossy(&s).into_owned()
        })
    };
    let stdout = drain(Box::new(c.stdout.take().unwrap()));
    let stderr = drain(Box::new(c.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = c.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            c.kill().unwrap();
            c.wait().unwrap();
            panic!("child {case} hung");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Outcome { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() }
}

thread_local! {
    static KEPT: std::cell::RefCell<Option<FilHandle<c_char>>> = const { std::cell::RefCell::new(None) };
}

static AT_EXIT: Mutex<Option<FilHandle<c_char>>> = Mutex::new(None);

extern "C" fn at_exit() {
    // Runs after `main` returns, while the process is exiting.
    let handle = AT_EXIT.lock().unwrap().take();
    if let Some(h) = handle {
        let s = runtime().run(|fil| h.get(fil).read_string(fil).unwrap());
        println!("at exit: {s}");
    }
}

/// The child side.
#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    if case == "panic-hook-runs-exited" {
        // Installed before the runtime starts, so the runtime wraps it. A
        // hook that blocks, like the default one writing to stderr.
        std::panic::set_hook(Box::new(|info| {
            std::thread::sleep(Duration::from_millis(600));
            eprintln!("{info}");
        }));
    }
    let rt = runtime();
    let b = basic();
    let lifecycle = rt.load(fixture("lifecycle.c")).unwrap();
    let edges = rt.load(fixture("edges.c")).unwrap();
    match case.as_str() {
        "exit-while-c-sleeps" => {
            let l = lifecycle.clone();
            let _job = rt.spawn(move |fil| l.function::<fn()>(fil, "sleep_forever").unwrap().call(fil, ()).unwrap());
            std::thread::sleep(Duration::from_millis(100));
        }
        "exit-while-c-spins" => {
            for _ in 0..3 {
                let l = lifecycle.clone();
                let _job = rt.spawn(move |fil| l.function::<fn()>(fil, "spin_forever").unwrap().call(fil, ()).unwrap());
            }
            std::thread::sleep(Duration::from_millis(100));
            // Stdio written before exit is flushed even with the spinning threads.
            rt.run(|fil| b.say(fil, fil.c_str("said before exit").unwrap()).unwrap());
        }
        "exit-while-exited-blocks" => {
            let _job = rt.spawn(|fil| fil.exited(|| loop {
                std::thread::park();
            }));
            std::thread::sleep(Duration::from_millis(100));
        }
        "exit-from-a-callback" => {
            rt.run(|fil| {
                let cb = Callback::<fn(i32) -> i32>::new(fil, |_, (code,)| std::process::exit(code)).unwrap();
                b.library().function::<fn(fn(i32) -> i32, i32) -> i32>(fil, "apply").unwrap().call(fil, (cb.get(fil), 3)).unwrap();
            });
            unreachable!("exit returned");
        }
        "handles-dropped-by-thread-locals" => {
            // A thread that never entered the runtime drops a handle from a
            // thread-local destructor as it exits.
            let h = rt.run(|fil| fil.root(fil.c_str("tls").unwrap()).unwrap());
            std::thread::spawn(move || KEPT.with(|k| *k.borrow_mut() = Some(h))).join().unwrap();
            // This thread keeps one until the process exits.
            let h = rt.run(|fil| fil.root(fil.c_str("main tls").unwrap()).unwrap());
            KEPT.with(|k| *k.borrow_mut() = Some(h));
            rt.run(|fil| fil.collect_garbage().unwrap());
        }
        "runtime-used-at-exit" => {
            let h = rt.run(|fil| fil.root(fil.c_str("still here").unwrap()).unwrap());
            *AT_EXIT.lock().unwrap() = Some(h);
            unsafe { libc::atexit(at_exit) };
        }
        "deep-recursion" => {
            rt.run(|fil| {
                let down = edges.function::<fn(i32) -> i32>(fil, "down").unwrap();
                let again = down;
                let cb = Callback::<fn(i32) -> i32>::new(fil, move |fil, (n,)| again.call(fil, (n,)).unwrap()).unwrap();
                edges.function::<fn(fn(i32) -> i32)>(fil, "store_callback").unwrap().call(fil, (cb.get(fil),)).unwrap();
                let _ = down.call(fil, (10_000_000,));
            });
            unreachable!("unbounded recursion returned");
        }
        "fork-then-use" => {
            let kept = rt.run(|fil| fil.root(fil.c_str("parent's").unwrap()).unwrap());
            let job = rt.spawn(|_| 1);
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // The child: no executors. Using the runtime panics instead
                // of hanging; dropping what came from it leaks.
                let run = catch_unwind(AssertUnwindSafe(|| rt.run(|_| 0)));
                let spawn = catch_unwind(AssertUnwindSafe(|| drop(rt.spawn(|_| 0))));
                let join = catch_unwind(AssertUnwindSafe(|| job.join()));
                drop(kept);
                let message = run
                    .as_ref()
                    .err()
                    .and_then(|p| p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()))
                    .unwrap_or_default();
                let ok = run.is_err() && spawn.is_err() && join.is_err() && message.contains("fork");
                unsafe { libc::_exit(if ok { 42 } else { 1 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 42, "child status {status:#x}");
            // The parent's runtime is untouched.
            assert_eq!(job.join(), 1);
            assert_eq!(rt.run(|fil| kept.get(fil).read_string(fil).unwrap()), "parent's");
        }
        "constructor-breaks-memory-safety" => {
            let _ = rt.load(fixture("ctor_crash.c"));
            unreachable!("the constructor's out-of-bounds read was allowed");
        }
        "variadic-too-few" => {
            rt.run(|fil| {
                // Four words passed, five read. (Fil-C rounds the argument
                // area to 16 bytes, as it does for calls from C, so with an
                // odd number of words one extra read finds zero padding.)
                let ints = edges.function::<fn(i32) -> i64>(fil, "va_ints").unwrap();
                let _ = ints.dyn_call(fil).arg(4i32).arg(1i32).arg(2i32).arg(3i32).call::<i64>();
            });
            unreachable!("read a variadic argument that wasn't passed");
        }
        "variadic-integer-as-pointer" => {
            rt.run(|fil| {
                let mixed = edges.function::<fn(i32) -> i64>(fil, "va_mixed").unwrap();
                let s = fil.c_str("abc").unwrap();
                let _ = mixed.dyn_call(fil).arg(1i32).arg(s.addr() as u64).arg(1i64).call::<i64>();
            });
            unreachable!("an integer was used as a pointer");
        }
        "stale-pointer-from-dropped-handle" => {
            // C keeps a pointer it got through a handle; the handle is
            // dropped, the collector runs, and C uses the pointer: still
            // valid, since C's reference keeps the object alive.
            let h = rt.run(|fil| fil.root(fil.c_str("kept by C").unwrap()).unwrap());
            rt.run(|fil| b.keep(fil, h.get(fil).cast()).unwrap());
            drop(h);
            for _ in 0..3 {
                rt.run(|fil| fil.collect_garbage().unwrap());
            }
            let s = rt.run(|fil| b.get_kept(fil).unwrap().cast::<c_char>().read_string(fil).unwrap());
            assert_eq!(s, "kept by C");
        }
        "symbols-looked-up-while-a-constructor-runs" => {
            // Threads looking up symbols (dlsym takes the loader's lock)
            // while another thread's dlopen runs a constructor that creates
            // a thread, and collections run: a collection holds the handshake
            // lock while it waits for every entered thread, and creating a
            // thread takes that lock.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let s = stop.clone();
            let collector = std::thread::spawn(move || {
                while !s.load(std::sync::atomic::Ordering::Relaxed) {
                    rt.run(|fil| fil.collect_garbage().unwrap());
                }
            });
            let lookers: Vec<_> = (0..4)
                .map(|_| {
                    let (stop, edges) = (stop.clone(), edges.clone());
                    std::thread::spawn(move || {
                        let mut n = 0u64;
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            rt.run(|fil| {
                                for _ in 0..50 {
                                    edges.function::<fn(i32) -> i32>(fil, "down").unwrap();
                                }
                            });
                            n += 1;
                        }
                        n
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_millis(50));
            let slow = rt.load(fixture("ctor_slow.c")).unwrap();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            for l in lookers {
                assert!(l.join().unwrap() > 0);
            }
            collector.join().unwrap();
            assert_eq!(rt.run(|fil| slow.function::<fn() -> i32>(fil, "ctor_slow_started").unwrap().call(fil, ()).unwrap()), 1);
        }
        "executors-started-while-a-constructor-runs" => {
            // Busy executors make the pool grow while another thread's
            // dlopen runs a slow constructor that then creates a thread, and
            // collections run. Starting executors looks up pthread_create
            // with dlsym, which waits for the loader's lock; the thread
            // doing it must not wait entered, or the collection waits for it
            // while holding the lock that creating a thread needs.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let s = stop.clone();
            // One job that keeps collecting, so that it holds an executor
            // and a collection is under way when the pool grows.
            let collector = rt.spawn(move |fil| {
                // Resolved once: looking it up each time (as collect_garbage
                // does) would wait for the loader's lock too, and no
                // collection would be under way during the constructor.
                let gc = fil.runtime_library().function::<fn()>(fil, "zgc_request_and_wait").unwrap();
                while !s.load(std::sync::atomic::Ordering::Relaxed) {
                    gc.call(fil, ()).unwrap();
                }
            });
            let grower = std::thread::spawn(move || {
                // Inside the constructor's sleep: enough jobs that block
                // (outside the runtime) that every executor is busy.
                std::thread::sleep(Duration::from_millis(100));
                let jobs: Vec<_> = (0..24)
                    .map(|_| rt.spawn(|fil| fil.exited(|| std::thread::sleep(Duration::from_millis(400)))))
                    .collect();
                for j in jobs {
                    j.join();
                }
            });
            let slow = rt.load(fixture("ctor_slow.c")).unwrap();
            grower.join().unwrap();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            collector.join();
            assert_eq!(rt.run(|fil| slow.function::<fn() -> i32>(fil, "ctor_slow_started").unwrap().call(fil, ()).unwrap()), 1);
        }
        "panic-hook-runs-exited" => {
            // A panic inside a job runs the hook on an entered thread; if the
            // hook ran entered, collections would stall until it returned.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let collections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (s, c) = (stop.clone(), collections.clone());
            let collector = rt.spawn(move |fil| {
                let gc = fil.runtime_library().function::<fn()>(fil, "zgc_request_and_wait").unwrap();
                while !s.load(std::sync::atomic::Ordering::Relaxed) {
                    gc.call(fil, ()).unwrap();
                    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
            std::thread::sleep(Duration::from_millis(50));
            let before = collections.load(std::sync::atomic::Ordering::Relaxed);
            rt.run(|_fil| {
                let caught = catch_unwind(|| panic!("inside a job"));
                assert!(caught.is_err());
            });
            let during = collections.load(std::sync::atomic::Ordering::Relaxed) - before;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            collector.join();
            assert!(during > 10, "only {during} collections finished while the panic hook ran");
        }
        other => panic!("unknown case {other}"),
    }
    println!("case done");
}

#[test]
fn exiting_while_c_sleeps() {
    child("exit-while-c-sleeps").assert_clean();
}

#[test]
fn exiting_while_c_spins_flushes_stdio() {
    let o = child("exit-while-c-spins");
    o.assert_clean();
    assert!(o.stdout.contains("said before exit"), "{}", o.describe());
}

#[test]
fn exiting_while_a_job_blocks_outside_the_runtime() {
    child("exit-while-exited-blocks").assert_clean();
}

#[test]
fn exiting_from_inside_a_callback() {
    let o = child("exit-from-a-callback");
    assert_eq!(o.status.code(), Some(3), "{}", o.describe());
}

#[test]
fn handles_dropped_by_thread_local_destructors() {
    child("handles-dropped-by-thread-locals").assert_clean();
}

#[test]
fn the_runtime_works_in_atexit_handlers() {
    let o = child("runtime-used-at-exit");
    o.assert_clean();
    assert!(o.stdout.contains("at exit: still here"), "{}", o.describe());
}

#[test]
fn unbounded_recursion_through_callbacks_ends_the_process() {
    let o = child("deep-recursion");
    assert!(!o.status.success(), "{}", o.describe());
    // Whichever side's frame hits the limit reports it: Fil-C with a safety
    // error, Rust with its overflow handler.
    let fil_c = o.stderr.contains("filc safety error: stack overflow");
    let rust = o.stderr.contains("has overflowed its stack");
    let signal = matches!(o.signal(), Some(libc::SIGSEGV | libc::SIGBUS | libc::SIGABRT | libc::SIGTRAP));
    assert!(signal && (fil_c || rust), "{}", &o.describe()[..4000.min(o.describe().len())]);
}

#[test]
fn a_forked_child_cannot_use_the_runtime() {
    child("fork-then-use").assert_clean();
}

#[test]
fn constructors_are_checked_too() {
    child("constructor-breaks-memory-safety").assert_fil_c_panic("ctor_crash.c");
}

#[test]
fn variadic_misuse_is_stopped() {
    child("variadic-too-few").assert_fil_c_panic("edges.c");
    child("variadic-integer-as-pointer").assert_fil_c_panic("edges.c");
}

#[test]
fn pointers_kept_by_c_outlive_the_handle_they_came_from() {
    child("stale-pointer-from-dropped-handle").assert_clean();
}

#[test]
fn looking_up_symbols_while_a_constructor_creates_threads() {
    child("symbols-looked-up-while-a-constructor-runs").assert_clean();
}

#[test]
fn panic_hooks_run_outside_the_runtime() {
    child("panic-hook-runs-exited").assert_clean();
}

#[test]
fn starting_executors_while_a_constructor_creates_threads() {
    child("executors-started-while-a-constructor-runs").assert_clean();
}
