//! Memory safety: capability checks on every access, freed objects, handles,
//! Rust-owned buffers, and garbage collection.

mod common;

use std::ffi::c_char;

use common::basic::Pt;
use common::{basic, runtime};
use fil_rust::{Error, FilPtr, Opaque, Ptr};

/// Serializes tests that use the fixture's single `kept` global.
static KEPT: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn out_of_bounds_accesses_are_errors() {
    runtime().run(|fil| {
        let p = fil.alloc_array::<i32>(&[1, 2, 3, 4]).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(p.read_at(fil, 3).unwrap(), 4);
        assert_eq!(p.read_at(fil, 4).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.write_at(fil, 4, &0).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.read_array(fil, 5).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.add(3).byte_add(2).read(fil).unwrap_err(), Error::OutOfBounds, "straddles the end");
        assert_eq!(p.byte_offset(-4).read(fil).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.read_vec(fil, 17).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.byte_add(usize::MAX - 2).read(fil).unwrap_err(), Error::OutOfBounds);
        // A failed write changes nothing.
        assert_eq!(p.write_bytes(fil, &[9u8; 20]).unwrap_err(), Error::OutOfBounds);
        assert_eq!(p.read_array(fil, 4).unwrap(), vec![1, 2, 3, 4]);
        // Pointer arithmetic itself is fine, like in C: only access is checked.
        let end = p.add(4);
        assert_eq!(end.len(), 0);
        assert_eq!(end.byte_offset(-4).cast::<i32>().read(fil).unwrap(), 4);
    });
}

#[test]
fn strings_must_be_terminated_inside_the_object() {
    runtime().run(|fil| {
        // Exactly 16 bytes, so there's no rounding slack holding a zero.
        let p = fil.bytes(b"no terminator!!!").unwrap();
        assert_eq!(p.len(), 16);
        assert_eq!(p.cast::<c_char>().read_string(fil).unwrap_err(), Error::OutOfBounds);
        let q = fil.bytes(b"ok\0trailing").unwrap();
        assert_eq!(q.cast::<c_char>().read_string(fil).unwrap(), "ok");
        let bad = fil.bytes(b"\xff\xfe\0").unwrap();
        assert_eq!(bad.cast::<c_char>().read_string(fil).unwrap_err(), Error::Utf8);
        assert_eq!(bad.cast::<c_char>().to_string_lossy(fil).unwrap(), "\u{fffd}\u{fffd}");
        assert_eq!(fil.c_str("a\0b").unwrap_err(), Error::InteriorNul);
    });
}

#[test]
fn pointers_without_capabilities() {
    runtime().run(|fil| {
        let null = FilPtr::<u8>::null();
        assert!(null.is_null());
        assert_eq!(null.capability(), None);
        assert_eq!(null.read(fil).unwrap_err(), Error::NullCapability);
        // An integer cast to a pointer has no capability either, even if it
        // happens to point at live memory.
        let real = fil.alloc_bytes(8).unwrap();
        let forged = FilPtr::<u8>::from_addr(real.addr());
        assert!(!forged.has_capability());
        assert_eq!(forged.read(fil).unwrap_err(), Error::NullCapability);
        assert_eq!(forged.write(fil, &1).unwrap_err(), Error::NullCapability);
        assert_eq!(forged.remaining(), 0);
    });
}

#[test]
fn freed_objects_are_errors() {
    let b = basic();
    runtime().run(|fil| {
        let p = b.alloc_bytes(fil, 32).unwrap();
        p.write_bytes(fil, b"data").unwrap();
        b.free_it(fil, p.cast()).unwrap();
        let cap = p.capability().unwrap();
        assert!(cap.freed);
        assert_eq!(p.read_vec(fil, 4).unwrap_err(), Error::Freed);
        assert_eq!(p.write_bytes(fil, b"x").unwrap_err(), Error::Freed);
        assert_eq!(p.remaining(), 0);
        // Memory Rust allocated can be freed by C too.
        let q = fil.alloc::<Pt>(&Pt { x: 1, y: 2 }).unwrap();
        b.free_it(fil, q.cast()).unwrap();
        assert_eq!(q.read(fil).unwrap_err(), Error::Freed);
    });
}

#[test]
fn read_only_memory() {
    let b = basic();
    runtime().run(|fil| {
        let lit = b.string_literal(fil).unwrap();
        assert!(lit.capability().unwrap().readonly);
        assert_eq!(lit.write_bytes(fil, b"A").unwrap_err(), Error::ReadOnly);
        assert_eq!(lit.read_string(fil).unwrap(), "a string literal");
    });
}

#[test]
fn special_objects_are_not_data() {
    let b = basic();
    runtime().run(|fil| {
        let f = b.library().symbol(fil, "add").unwrap();
        let cap = f.capability().unwrap();
        assert!(cap.function && cap.special && cap.global);
        assert_eq!(f.cast::<u8>().read(fil).unwrap_err(), Error::SpecialObject);
        assert_eq!(f.cast::<u8>().write(fil, &0).unwrap_err(), Error::SpecialObject);
    });
}

#[test]
fn misaligned_pointer_stores() {
    runtime().run(|fil| {
        let buf = fil.alloc_bytes(32).unwrap();
        let target = fil.alloc_bytes(1).unwrap();
        let slot = buf.byte_add(4).cast::<Ptr<u8>>();
        assert_eq!(slot.write(fil, &target).unwrap_err(), Error::Misaligned);
        assert_eq!(slot.read(fil).unwrap_err(), Error::Misaligned);
        let slot = buf.byte_add(8).cast::<Ptr<u8>>();
        slot.write(fil, &target).unwrap();
        assert_eq!(slot.read(fil).unwrap(), target);
    });
}

#[test]
fn pointers_stored_in_memory_keep_capabilities() {
    runtime().run(|fil| {
        let s = fil.c_str("capability").unwrap();
        let slots = fil.alloc_zeroed::<Ptr<c_char>>(2).unwrap();
        slots.write_at(fil, 0, &s).unwrap();
        // memmove preserves capabilities, like Fil-C's memmove.
        slots.add(1).copy_from(fil, slots, 8).unwrap();
        assert_eq!(slots.read_at(fil, 1).unwrap().read_string(fil).unwrap(), "capability");
        // Copying a pointer's bytes out and back in does *not* carry the
        // capability: integers can't become pointers.
        let bytes = slots.read_vec(fil, 8).unwrap();
        let fresh = fil.alloc_zeroed::<Ptr<c_char>>(1).unwrap();
        fresh.write_bytes(fil, &bytes).unwrap();
        let p = fresh.read(fil).unwrap();
        assert_eq!(p.addr(), s.addr());
        assert!(!p.has_capability());
        assert_eq!(p.read_string(fil).unwrap_err(), Error::NullCapability);
    });
}

#[test]
fn fill_and_copy() {
    runtime().run(|fil| {
        let p = fil.alloc_bytes(10_000).unwrap();
        // Allocations may be rounded up; the capability says how far.
        let len = p.len();
        assert!(len >= 10_000);
        p.fill(fil, 0xab, len).unwrap();
        assert!(p.read_vec(fil, len).unwrap().iter().all(|&b| b == 0xab));
        assert_eq!(p.fill(fil, 0, len + 1).unwrap_err(), Error::OutOfBounds);
        let q = fil.alloc_bytes(112).unwrap();
        assert_eq!(q.len(), 112);
        q.copy_from(fil, p.byte_add(50), 112).unwrap();
        assert_eq!(q.read_vec(fil, 112).unwrap(), vec![0xab; 112]);
        assert_eq!(q.copy_from(fil, p, 113).unwrap_err(), Error::OutOfBounds);
        // Large copies go through in chunks.
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let r = fil.bytes(&big).unwrap();
        assert_eq!(r.read_vec(fil, big.len()).unwrap(), big);
    });
}

#[test]
fn handles_outlive_their_scope() {
    let rt = runtime();
    let handle = rt.run(|fil| {
        let p = fil.alloc_array::<u64>(&[7, 8, 9]).unwrap();
        fil.root(p).unwrap()
    });
    // Force collections between scopes; the handle keeps the object alive.
    for _ in 0..3 {
        rt.run(|fil| fil.collect_garbage().unwrap());
    }
    let values = rt.run(|fil| handle.get(fil).read_array(fil, 3).unwrap());
    assert_eq!(values, vec![7, 8, 9]);
    // Handles are Send + Sync: share one between threads.
    let handle = std::sync::Arc::new(handle);
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let handle = handle.clone();
            std::thread::spawn(move || {
                runtime().run(|fil| {
                    let p = handle.get(fil);
                    p.read_at(fil, i % 3).unwrap()
                })
            })
        })
        .collect();
    let got: Vec<u64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(got, vec![7, 8, 9, 7]);
}

#[test]
fn many_handles() {
    let rt = runtime();
    let handles: Vec<_> = rt.run(|fil| {
        (0..5000u32)
            .map(|i| fil.scope(|fil| fil.root(fil.alloc::<u32>(&i).unwrap()).unwrap()))
            .collect()
    });
    rt.run(|fil| fil.collect_garbage().unwrap());
    rt.run(|fil| {
        for (i, h) in handles.iter().enumerate() {
            assert_eq!(h.get(fil).read(fil).unwrap(), i as u32);
        }
    });
    let cloned = rt.run(|fil| handles[42].clone_in(fil).unwrap());
    drop(handles);
    // Slots of dropped handles are reused.
    let again: Vec<_> = rt.run(|fil| (0..5000).map(|_| fil.root(fil.alloc_bytes(1).unwrap()).unwrap()).collect());
    assert_eq!(rt.run(|fil| cloned.get(fil).read(fil).unwrap()), 42);
    drop(again);
}

#[test]
fn rust_owned_buffers() {
    let rt = runtime();
    let b = basic();
    let mut buf = rt.run(|fil| fil.buf(64).unwrap());
    // Plain slice access, outside the runtime, from another thread.
    let mut buf = std::thread::spawn(move || {
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = i as u8;
        }
        buf
    })
    .join()
    .unwrap();
    assert_eq!(buf.len(), 64);
    buf[63] = 200;
    let sum: i32 = (0..63).sum::<i32>() + 200;
    let got = rt.run(move |fil| {
        let p = buf.share(fil);
        b.sum_bytes(fil, p, 64).unwrap()
    });
    assert_eq!(got, sum);

    let from = rt.run(|fil| fil.buf_from(b"copied in").unwrap());
    assert_eq!(&*from, b"copied in");
    let empty = rt.run(|fil| fil.buf(0).unwrap());
    assert!(empty.is_empty());
}

#[test]
fn objects_kept_by_c_survive_collection() {
    let _kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let rt = runtime();
    let b = basic();
    rt.run(|fil| {
        let s = fil.c_str("kept alive by a C global").unwrap();
        b.keep(fil, s.cast()).unwrap();
    });
    for _ in 0..3 {
        rt.run(|fil| {
            // Generate garbage, then collect.
            fil.scope(|fil| {
                for _ in 0..1000 {
                    fil.alloc_bytes(256).unwrap();
                }
            });
            fil.collect_garbage().unwrap();
        });
    }
    let s = rt.run(|fil| b.get_kept(fil).unwrap().cast::<c_char>().read_string(fil).unwrap());
    assert_eq!(s, "kept alive by a C global");
}

#[test]
fn pointers_stay_valid_for_their_whole_scope() {
    let b = basic();
    runtime().run(|fil| {
        let mut ptrs = Vec::new();
        for i in 0..200 {
            ptrs.push(fil.alloc::<i64>(&i).unwrap());
            if i % 50 == 0 {
                fil.collect_garbage().unwrap();
                b.churn(fil, 20, 512).unwrap();
            }
        }
        fil.collect_garbage().unwrap();
        for (i, p) in ptrs.iter().enumerate() {
            assert_eq!(p.read(fil).unwrap(), i as i64);
        }
    });
}

#[test]
fn nested_scopes_bound_memory() {
    runtime().run(|fil| {
        let outer = fil.alloc::<u32>(&1).unwrap();
        let total: u64 = (0..20_000)
            .map(|i| {
                fil.scope(|fil| {
                    let p = fil.alloc_array::<u32>(&[i, 1, 2, 3]).unwrap();
                    p.read_array(fil, 4).unwrap().iter().map(|&v| v as u64).sum::<u64>()
                })
            })
            .sum();
        assert_eq!(total, (0..20_000u64).sum::<u64>() + 6 * 20_000);
        assert_eq!(outer.read(fil).unwrap(), 1);
    });
}

#[test]
fn gc_under_concurrent_allocation() {
    let rt = runtime();
    let b = basic();
    let threads: Vec<_> = (0..6)
        .map(|t| {
            std::thread::spawn(move || {
                for round in 0..20 {
                    rt.run(|fil| {
                        let s = fil.c_str(&format!("thread {t} round {round}")).unwrap();
                        let up = b.dup_upper(fil, s).unwrap();
                        assert_eq!(b.churn(fil, 5, 128).unwrap(), (0..64).sum::<i64>() * 5);
                        if round % 7 == 0 {
                            fil.collect_garbage().unwrap();
                        }
                        assert_eq!(up.read_string(fil).unwrap(), format!("THREAD {t} ROUND {round}"));
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
fn opaque_pointers_round_trip() {
    let _kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let b = basic();
    runtime().run(|fil| {
        let p = fil.alloc::<Pt>(&Pt { x: 5, y: 6 }).unwrap();
        let o: FilPtr<'_, Opaque> = p.cast();
        b.keep(fil, o).unwrap();
        let back = b.get_kept(fil).unwrap().cast::<Pt>();
        assert_eq!(back, p);
        assert_eq!(back.read(fil).unwrap(), Pt { x: 5, y: 6 });
        let cap = back.capability().unwrap();
        assert!(cap.upper - cap.lower >= 8);
        assert!(!cap.global && !cap.readonly && !cap.freed);
    });
}
