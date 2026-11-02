//! Edge cases that stay inside one process: callbacks dropped mid-call,
//! variadic promotions, copies that span the runtime's copy chunks, objects
//! freed during a copy, deep recursion through callbacks, panics unwinding
//! through nested scopes, libraries reached by several paths, constructors
//! that start threads, and starting child programs.

mod common;

use std::ffi::c_char;
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use common::{basic, fixture, runtime};
use fil_rust::{Callback, Error, Fil, Library, Ptr};

/// The runtime copies between Rust and Fil-C memory in chunks of this size.
const CHUNK: usize = 64 * 1024;

fn edges(fil: &Fil<'_>) -> Library {
    fil.load(fixture("edges.c")).expect("loading edges.c")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(31).wrapping_add(seed as usize).wrapping_add(i >> 16)) as u8).collect()
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_callback_dropped_mid_call_finishes_that_call() {
    let rt = runtime();
    let b = basic();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
    let (entered_tx, dropped_rx) = (Mutex::new(entered_tx), Arc::new(Mutex::new(dropped_rx)));
    let cb = rt.run(|fil| {
        Callback::<fn(i32) -> i32>::new(fil, move |fil, (x,)| {
            entered_tx.lock().unwrap().send(()).unwrap();
            // Wait, outside the runtime, until the Callback has been dropped.
            let rx = dropped_rx.clone();
            fil.exited(move || rx.lock().unwrap().recv().unwrap());
            x * 2
        })
        .unwrap()
    });
    let slot = Arc::new(Mutex::new(Some(cb)));
    let s = slot.clone();
    let caller = std::thread::spawn(move || {
        rt.run(|fil| {
            let f = s.lock().unwrap().as_ref().unwrap().get(fil);
            b.library().function::<fn(fn(i32) -> i32, i32) -> i32>(fil, "apply").unwrap().call(fil, (f, 21)).unwrap()
        })
    });
    entered_rx.recv().unwrap();
    drop(slot.lock().unwrap().take());
    dropped_tx.send(()).unwrap();
    // The call already in progress keeps its closure alive until it returns.
    assert_eq!(caller.join().unwrap(), 42);
}

#[test]
fn variadic_integers_are_extended_by_their_signedness() {
    runtime().run(|fil| {
        let lib = edges(fil);
        let ints = lib.function::<fn(i32) -> i64>(fil, "va_ints").unwrap();
        let got = ints.dyn_call(fil).arg(5i32).arg(-1i8).arg(-300i16).arg(200u8).arg(true).arg(-7i32).call::<i64>().unwrap();
        let want = [-1i64, -300, 200, 1, -7].iter().fold(0i64, |acc, &v| acc * 1000 + v);
        assert_eq!(got, want);

        let uints = lib.function::<fn(i32) -> u64>(fil, "va_uints").unwrap();
        let got = uints.dyn_call(fil).arg(3i32).arg(u32::MAX).arg(65535u16).arg(255u8).call::<u64>().unwrap();
        assert_eq!(got, u32::MAX as u64 + 65535 + 255);
    });
}

#[test]
fn variadic_floats_must_be_passed_promoted() {
    runtime().run(|fil| {
        let lib = edges(fil);
        let doubles = lib.function::<fn(i32) -> f64>(fil, "va_doubles").unwrap();
        let got = doubles.dyn_call(fil).arg(3i32).arg(1.5f64).arg(-0.25f64).arg(2.5f32 as f64).call::<f64>().unwrap();
        assert_eq!(got, 3.75);
        // An unpromoted `f32` is passed as a float, which a variadic callee
        // reads as some other double: a wrong value, but nothing unsafe.
        let unpromoted = doubles.dyn_call(fil).arg(1i32).arg(2.5f32).call::<f64>();
        assert!(unpromoted.is_ok());
        assert_ne!(unpromoted.unwrap(), 2.5);
    });
}

#[test]
fn variadic_pointers_keep_their_capabilities() {
    runtime().run(|fil| {
        let lib = edges(fil);
        let mixed = lib.function::<fn(i32) -> i64>(fil, "va_mixed").unwrap();
        let got = mixed
            .dyn_call(fil)
            .arg(2i32)
            .arg(fil.c_str("abc").unwrap())
            .arg(10i64)
            .arg(fil.c_str("hello").unwrap())
            .arg(-2i64)
            .call::<i64>()
            .unwrap();
        assert_eq!(got, 3 * 10 + 5 * -2);
    });
}

#[test]
fn copies_across_chunk_boundaries() {
    let sizes = [1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK - 1, 2 * CHUNK, 2 * CHUNK + 1, 3 * CHUNK + 5, (1 << 20) + 3];
    runtime().run(|fil| {
        let lib = edges(fil);
        let block = lib.function::<fn(usize) -> Ptr<u8>>(fil, "block").unwrap();
        let hash = lib.function::<fn(Ptr<u8>, usize) -> u64>(fil, "fnv1a").unwrap();
        let fill = lib.function::<fn(Ptr<u8>, usize, u32)>(fil, "fill_pattern").unwrap();
        for &size in &sizes {
            for offset in [0usize, 1, 7] {
                let p = block.call(fil, (size + offset,)).unwrap().byte_add(offset);
                // Rust -> C.
                let data = pattern(size, size as u32 ^ 0x5a);
                p.write_bytes(fil, &data).unwrap();
                assert_eq!(hash.call(fil, (p, size)).unwrap(), fnv1a(&data), "size {size} offset {offset}");
                // C -> Rust, both ways of reading.
                fill.call(fil, (p, size, 9)).unwrap();
                let expected = pattern(size, 9);
                assert!(p.read_vec(fil, size).unwrap() == expected, "read_vec, size {size} offset {offset}");
                let mut buf = vec![0u8; size];
                p.read_bytes(fil, &mut buf).unwrap();
                assert!(buf == expected, "read_bytes, size {size} offset {offset}");
                // One byte more than the object holds (allocations round up)
                // fails before anything is written, though nearly all of the
                // range is in bounds.
                let room = p.remaining();
                assert!(room >= size && room < size + 16, "size {size}: room {room}");
                let too_long = vec![0xffu8; room + 1];
                assert_eq!(p.write_bytes(fil, &too_long).unwrap_err(), Error::OutOfBounds);
                assert_eq!(hash.call(fil, (p, size)).unwrap(), fnv1a(&expected), "size {size}: partial write");
                // Fil-C to Fil-C copies of the same sizes.
                let q = block.call(fil, (size,)).unwrap();
                q.copy_from(fil, p, size).unwrap();
                assert_eq!(hash.call(fil, (q, size)).unwrap(), fnv1a(&expected));
            }
        }
    });
}

#[test]
fn long_strings_are_read_whole() {
    runtime().run(|fil| {
        let lib = edges(fil);
        let long_string = lib.function::<fn(usize) -> Ptr<c_char>>(fil, "long_string").unwrap();
        for n in [CHUNK - 1, CHUNK, CHUNK + 1, 5 * CHUNK + 17] {
            let s = long_string.call(fil, (n,)).unwrap().read_string(fil).unwrap();
            assert_eq!(s.len(), n);
            assert!(s.bytes().enumerate().all(|(i, c)| c == b'a' + (i % 26) as u8));
        }
        // And from Rust to C.
        let big = "xyz".repeat(3 * CHUNK);
        let strlen = basic().library().function::<fn(Ptr<c_char>) -> usize>(fil, "my_strlen").unwrap();
        assert_eq!(strlen.call(fil, (fil.c_str(&big).unwrap(),)).unwrap(), big.len());
    });
}

#[test]
fn freeing_an_object_during_a_large_copy() {
    let rt = runtime();
    const SIZE: usize = 8 << 20;
    for round in 0..4 {
        let (h, release) = rt.run(|fil| {
            let lib = edges(fil);
            let p = lib.function::<fn(usize) -> Ptr<u8>>(fil, "block").unwrap().call(fil, (SIZE,)).unwrap();
            (fil.root(p).unwrap(), lib.function::<fn(Ptr<u8>)>(fil, "release").unwrap())
        });
        let h = Arc::new(h);
        let freed = Arc::new(AtomicBool::new(false));
        let (h2, f2) = (h.clone(), freed.clone());
        let freer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1 + round * 3));
            rt.run(|fil| release.call(fil, (h2.get(fil),)).unwrap());
            f2.store(true, Ordering::SeqCst);
        });
        let data = vec![round as u8; SIZE];
        let mut results = Vec::new();
        loop {
            let done = freed.load(Ordering::SeqCst);
            let r = rt.run(|fil| h.get(fil).write_bytes(fil, &data));
            // Every copy either completes or reports the free; never anything else.
            assert!(matches!(r, Ok(()) | Err(Error::Freed)), "{r:?}");
            results.push(r);
            if done {
                break;
            }
        }
        freer.join().unwrap();
        assert_eq!(results.last().unwrap(), &Err(Error::Freed));
        assert_eq!(rt.run(|fil| h.get(fil).read_vec(fil, 1)).unwrap_err(), Error::Freed);
    }
}

#[test]
fn recursion_through_callbacks() {
    let rt = runtime();
    rt.run(|fil| {
        let lib = edges(fil);
        let down = lib.function::<fn(i32) -> i32>(fil, "down").unwrap();
        let again = down;
        // down(n) calls back into Rust, which calls down(n - 1): every level
        // crosses the boundary twice.
        let cb = Callback::<fn(i32) -> i32>::new(fil, move |fil, (n,)| again.call(fil, (n,)).unwrap()).unwrap();
        lib.function::<fn(fn(i32) -> i32)>(fil, "store_callback").unwrap().call(fil, (cb.get(fil),)).unwrap();
        assert_eq!(down.call(fil, (300,)).unwrap(), 300);
        // The same from several threads at once.
        let jobs: Vec<_> = (0..4)
            .map(|i| {
                rt.spawn(move |fil| down.call(fil, (50 + i,)).unwrap())
            })
            .collect();
        let total: i32 = jobs.into_iter().map(|j| j.join()).sum();
        assert_eq!(total, 50 + 51 + 52 + 53);
    });
}

#[test]
fn panics_unwind_through_nested_scopes() {
    let rt = runtime();
    let b = basic();
    rt.run(|fil| {
        let outer = fil.c_str("outer").unwrap();
        let caught = catch_unwind(AssertUnwindSafe(|| {
            fil.scope(|a| {
                let _handle = a.root(a.c_str("rooted").unwrap()).unwrap();
                let _buf = a.buf(1000).unwrap();
                a.scope(|inner| {
                    let words = inner.c_str("one two three").unwrap();
                    let count = inner.alloc::<i32>(&0).unwrap();
                    b.split_words(inner, words, count).unwrap();
                    // Panic while blocked outside the runtime, two scopes deep.
                    inner.exited(|| panic!("deep panic"));
                })
            })
        }));
        assert_eq!(*caught.unwrap_err().downcast::<&str>().unwrap(), "deep panic");
        // The thread is back in the runtime, and the enclosing scope's
        // memory is intact.
        fil.collect_garbage().unwrap();
        assert_eq!(outer.read_string(fil).unwrap(), "outer");
        assert_eq!(b.my_strlen(fil, outer).unwrap(), 5);
    });
    // A panic leaving `run` reaches the caller, and the runtime goes on.
    let caught = catch_unwind(|| {
        rt.run(|fil| {
            let _h = fil.root(fil.c_str("x").unwrap()).unwrap();
            fil.scope(|_| panic!("out of run"))
        })
    });
    assert!(caught.is_err());
    assert_eq!(rt.run(|fil| b.add(fil, 2, 3).unwrap()), 5);
}

#[test]
fn a_library_reached_by_several_paths() {
    let rt = runtime();
    let original = fixture("edges.c");
    let dir = scratch_dir("library-paths");
    // A symlink is the same file: the same library, with the same globals.
    let link = dir.join("liblinked.so");
    std::os::unix::fs::symlink(&original, &link).unwrap();
    // A path with a space and bytes that aren't UTF-8.
    let odd_dir = dir.join(std::ffi::OsStr::from_bytes(b"with space \xff\xfe"));
    std::fs::create_dir_all(&odd_dir).unwrap();
    let odd = odd_dir.join("libodd.so");
    std::os::unix::fs::symlink(&original, &odd).unwrap();
    // A copy is a different file, so it is a separate library.
    let copy = dir.join("libcopy.so");
    std::fs::copy(&original, &copy).unwrap();

    rt.run(|fil| {
        let bump = |lib: &Library| lib.function::<fn() -> i32>(fil, "bump_shared").unwrap().call(fil, ()).unwrap();
        let a = fil.load(&original).unwrap();
        let b = fil.load(&link).unwrap();
        let c = fil.load(&odd).unwrap();
        let start = bump(&a);
        assert_eq!(bump(&b), start + 1);
        assert_eq!(bump(&c), start + 2);
        assert_eq!(c.path(), odd.as_path());
        let copy = fil.load(&copy).unwrap();
        assert_eq!(bump(&copy), 1);
        assert_eq!(bump(&a), start + 3);
    });
}

#[test]
fn constructors_may_start_threads() {
    let rt = runtime();
    let lib = rt.load(fixture("ctor_threads.c")).unwrap();
    let total = rt.run(|fil| lib.function::<fn() -> i32>(fil, "join_ctor_threads").unwrap().call(fil, ()).unwrap());
    assert_eq!(total, 4000);
}

#[test]
fn child_programs_can_be_started() {
    let rt = runtime();
    // From a plain thread.
    assert!(std::process::Command::new("true").status().unwrap().success());
    // From inside a job, outside the runtime while it waits.
    let out = rt.run(|fil| {
        fil.exited(|| std::process::Command::new("sh").args(["-c", "echo $((6 * 7))"]).output().unwrap())
    });
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    // Fil-C code forking and waiting for its child.
    let status = rt.run(|fil| edges(fil).function::<fn(i32) -> i32>(fil, "fork_and_exit").unwrap().call(fil, (7,)).unwrap());
    assert_eq!(status, 7);
    // And the runtime goes on.
    assert_eq!(rt.run(|fil| basic().add(fil, 1, 1).unwrap()), 2);
}
