//! Properties checked over random inputs (`PROPTEST_CASES` sets how many).
//!
//! The central one: an access through a `FilPtr` succeeds exactly when it
//! lies inside the object, and one that fails changes nothing.

mod common;

use common::basic::{Big, Pt};
use common::{basic, runtime};
use fil_rust::{Error, FilHandle, Ptr};
use proptest::prelude::*;

fn config() -> ProptestConfig {
    ProptestConfig { cases: 128, ..ProptestConfig::default() }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn byte_accesses_succeed_exactly_in_bounds(
        size in 1usize..200,
        offset in -64isize..264,
        len in 1usize..300,
        fill in any::<u8>(),
    ) {
        runtime().run(|fil| {
            let p = fil.alloc_bytes(size).unwrap();
            // The capability may round the size up; it is the truth.
            let real = p.len();
            prop_assert!(real >= size);
            let before = p.read_vec(fil, real).unwrap();
            let q = p.byte_offset(offset);
            let in_bounds = offset >= 0 && offset as usize + len <= real;

            let read = q.read_vec(fil, len);
            prop_assert_eq!(read.is_ok(), in_bounds, "read of {} at {} in {}", len, offset, real);
            if let Err(e) = read {
                prop_assert_eq!(e, Error::OutOfBounds);
            }

            let write = q.write_bytes(fil, &vec![fill; len]);
            prop_assert_eq!(write.is_ok(), in_bounds);
            let after = p.read_vec(fil, real).unwrap();
            if in_bounds {
                let start = offset as usize;
                prop_assert!(after[start..start + len].iter().all(|&b| b == fill));
                prop_assert_eq!(&after[..start], &before[..start]);
                prop_assert_eq!(&after[start + len..], &before[start + len..]);
            } else {
                prop_assert_eq!(after, before, "a failed write changed memory");
            }
            Ok(())
        })?;
    }

    #[test]
    fn typed_accesses_succeed_exactly_in_bounds(
        values in prop::collection::vec(any::<u32>(), 1..40),
        index in 0usize..50,
        count in 0usize..50,
    ) {
        runtime().run(|fil| {
            let p = fil.alloc_array::<u32>(&values).unwrap();
            let n = p.len();
            prop_assert!(n >= values.len());
            prop_assert_eq!(p.read_at(fil, index).is_ok(), index < n);
            prop_assert_eq!(p.write_at(fil, index, &7).is_ok(), index < n);
            prop_assert_eq!(p.read_array(fil, count).is_ok(), count <= n);
            // A struct straddling the end is refused whole.
            let at = p.add(index).cast::<Pt>();
            prop_assert_eq!(at.read(fil).is_ok(), index + 2 <= n);
            let old = p.read_array(fil, n).unwrap();
            let wrote = at.write(fil, &Pt { x: -1, y: -2 });
            prop_assert_eq!(wrote.is_ok(), index + 2 <= n);
            if wrote.is_err() {
                prop_assert_eq!(p.read_array(fil, n).unwrap(), old);
            }
            Ok(())
        })?;
    }

    #[test]
    fn values_round_trip(
        bytes in prop::collection::vec(any::<u8>(), 0..2000),
        ints in prop::collection::vec(any::<i64>(), 1..100),
        floats in prop::collection::vec(any::<f64>(), 1..100),
        text in "[^\0]{0,200}",
        flag in any::<bool>(),
    ) {
        runtime().run(|fil| {
            prop_assert_eq!(fil.bytes(&bytes).unwrap().read_vec(fil, bytes.len()).unwrap(), bytes.clone());
            prop_assert_eq!(fil.alloc_array::<i64>(&ints).unwrap().read_array(fil, ints.len()).unwrap(), ints.clone());
            let back = fil.alloc_array::<f64>(&floats).unwrap().read_array(fil, floats.len()).unwrap();
            prop_assert!(back.iter().zip(&floats).all(|(a, b)| a.to_bits() == b.to_bits()), "f64 bits");
            prop_assert_eq!(fil.c_str(&text).unwrap().read_string(fil).unwrap(), text.clone());
            prop_assert_eq!(fil.alloc::<bool>(&flag).unwrap().read(fil).unwrap(), flag);
            let mut buf = fil.buf_from(&bytes).unwrap();
            prop_assert_eq!(&*buf, &bytes[..]);
            buf.iter_mut().for_each(|b| *b = b.wrapping_add(1));
            let shared = buf.share(fil);
            let bumped: Vec<u8> = bytes.iter().map(|b| b.wrapping_add(1)).collect();
            prop_assert_eq!(shared.read_vec(fil, bytes.len()).unwrap(), bumped);
            Ok(())
        })?;
    }

    #[test]
    fn strings_are_read_up_to_the_first_nul(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        runtime().run(|fil| {
            let mut terminated = bytes.clone();
            terminated.push(0);
            let p = fil.bytes(&terminated).unwrap().cast::<std::ffi::c_char>();
            let upto = &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())];
            let cstring = p.read_cstring(fil).unwrap();
            prop_assert_eq!(cstring.as_bytes(), upto);
            prop_assert_eq!(p.to_string_lossy(fil).unwrap(), String::from_utf8_lossy(upto).into_owned());
            match std::str::from_utf8(upto) {
                Ok(s) => prop_assert_eq!(p.read_string(fil).unwrap(), s),
                Err(_) => prop_assert_eq!(p.read_string(fil).unwrap_err(), Error::Utf8),
            }
            Ok(())
        })?;
    }

    #[test]
    fn c_and_rust_agree_on_arithmetic(
        a in any::<i32>(), b in any::<i32>(),
        x in any::<u8>(), y in any::<u8>(),
        s in any::<i16>(),
        m in any::<i64>(), n in any::<i64>(),
        d in -1e300f64..1e300, f in -1e30f32..1e30,
    ) {
        let lib = basic();
        runtime().run(|fil| {
            prop_assert_eq!(lib.add(fil, a, b).unwrap(), a.wrapping_add(b));
            prop_assert_eq!(lib.add_u8(fil, x, y).unwrap(), x.wrapping_add(y));
            prop_assert_eq!(lib.neg_i16(fil, s).unwrap(), s.wrapping_neg());
            prop_assert_eq!(lib.mul_i64(fil, m, n).unwrap(), m.wrapping_mul(n));
            prop_assert_eq!(lib.scale(fil, d, f).unwrap().to_bits(), (d * f as f64).to_bits());
            prop_assert_eq!(lib.halve(fil, f).unwrap().to_bits(), (f / 2.0).to_bits());
            Ok(())
        })?;
    }

    #[test]
    fn c_and_rust_agree_on_memory(
        text in "[ -~]{0,100}",
        ints in prop::collection::vec(any::<i32>(), 0..64),
        bytes in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let lib = basic();
        runtime().run(|fil| {
            let s = fil.c_str(&text).unwrap();
            prop_assert_eq!(lib.my_strlen(fil, s).unwrap(), text.len());
            prop_assert_eq!(lib.dup_upper(fil, s).unwrap().read_string(fil).unwrap(), text.to_ascii_uppercase());
            if !ints.is_empty() {
                let p = fil.alloc_array::<i32>(&ints).unwrap();
                lib.reverse_ints(fil, p, ints.len()).unwrap();
                let mut rev = ints.clone();
                rev.reverse();
                prop_assert_eq!(p.read_array(fil, ints.len()).unwrap(), rev);
            }
            let b = fil.bytes(&bytes).unwrap();
            prop_assert_eq!(lib.sum_bytes(fil, b, bytes.len()).unwrap(), bytes.iter().map(|&v| v as i32).sum::<i32>());
            Ok(())
        })?;
    }

    #[test]
    fn structs_by_value_agree(
        ax in any::<i32>(), ay in any::<i32>(), bx in any::<i32>(), by in any::<i32>(),
        base in -1_000_000_000_000i64..1_000_000_000_000,
    ) {
        let lib = basic();
        runtime().run(|fil| {
            let sum = lib.pt_add(fil, Pt { x: ax, y: ay }, Pt { x: bx, y: by }).unwrap();
            prop_assert_eq!(sum, Pt { x: ax.wrapping_add(bx), y: ay.wrapping_add(by) });
            prop_assert_eq!(lib.pt_sum(fil, Pt { x: ax, y: ay }).unwrap(), ax.wrapping_add(ay));
            let big = lib.big_make(fil, base).unwrap();
            prop_assert_eq!(big, Big { a: base, b: base + 1, c: base + 2, d: base + 3 });
            prop_assert_eq!(lib.big_twice(fil, big).unwrap(), Big { a: 2 * base, b: 2 * base + 2, c: 2 * base + 4, d: 2 * base + 6 });
            let p = fil.alloc::<Pt>(&Pt { x: ax, y: ay }).unwrap();
            lib.pt_scale(fil, p, 3).unwrap();
            prop_assert_eq!(p.read(fil).unwrap(), Pt { x: ax.wrapping_mul(3), y: ay.wrapping_mul(3) });
            Ok(())
        })?;
    }

    #[test]
    fn pointers_keep_capabilities_only_when_copied_as_pointers(
        slots in 1usize..16,
        from in 0usize..16,
        to in 0usize..16,
    ) {
        let (from, to) = (from % slots, to % slots);
        runtime().run(|fil| {
            let target = fil.c_str("target").unwrap();
            let table = fil.alloc_zeroed::<Ptr<std::ffi::c_char>>(slots).unwrap();
            table.write_at(fil, from, &target).unwrap();
            // A pointer-aware copy keeps the capability...
            let copy = fil.alloc_zeroed::<Ptr<std::ffi::c_char>>(slots).unwrap();
            copy.copy_from(fil, table, slots * 8).unwrap();
            prop_assert_eq!(copy.read_at(fil, from).unwrap().read_string(fil).unwrap(), "target");
            // ...copying the bytes doesn't.
            let bytes = table.add(from).read_vec(fil, 8).unwrap();
            let laundered = fil.alloc_zeroed::<Ptr<std::ffi::c_char>>(slots).unwrap();
            laundered.add(to).write_bytes(fil, &bytes).unwrap();
            let p = laundered.read_at(fil, to).unwrap();
            prop_assert_eq!(p.addr(), target.addr());
            prop_assert_eq!(p.read_string(fil).unwrap_err(), Error::NullCapability);
            Ok(())
        })?;
    }
}

#[derive(Clone, Debug)]
enum HandleOp {
    Root(u32),
    Drop(usize),
    Clone(usize),
    Collect,
    Check,
}

fn handle_op() -> impl Strategy<Value = HandleOp> {
    prop_oneof![
        4 => any::<u32>().prop_map(HandleOp::Root),
        3 => any::<usize>().prop_map(HandleOp::Drop),
        1 => any::<usize>().prop_map(HandleOp::Clone),
        1 => Just(HandleOp::Collect),
        1 => Just(HandleOp::Check),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..config() })]

    /// Handles behave like owned values: whatever sequence of rooting,
    /// cloning, dropping (from any thread) and collecting, every live handle
    /// still reads the value it was made with.
    #[test]
    fn handles_behave_like_owned_values(ops in prop::collection::vec(handle_op(), 1..120)) {
        let rt = runtime();
        let mut live: Vec<(FilHandle<u32>, u32)> = Vec::new();
        for op in ops {
            match op {
                HandleOp::Root(v) => live.push((rt.run(|fil| fil.root(fil.alloc::<u32>(&v).unwrap()).unwrap()), v)),
                HandleOp::Drop(i) if !live.is_empty() => {
                    let (h, _) = live.swap_remove(i % live.len());
                    if i % 2 == 0 {
                        std::thread::spawn(move || drop(h)).join().unwrap();
                    }
                }
                HandleOp::Clone(i) if !live.is_empty() => {
                    let (h, v) = &live[i % live.len()];
                    let v = *v;
                    let c = rt.run(|fil| h.clone_in(fil).unwrap());
                    live.push((c, v));
                }
                HandleOp::Collect => rt.run(|fil| fil.collect_garbage().unwrap()),
                _ => {}
            }
            if matches!(op, HandleOp::Check | HandleOp::Collect) {
                rt.run(|fil| -> Result<(), TestCaseError> {
                    for (h, v) in &live {
                        prop_assert_eq!(h.get(fil).read(fil).unwrap(), *v);
                    }
                    Ok(())
                })?;
            }
        }
        rt.run(|fil| -> Result<(), TestCaseError> {
            for (h, v) in &live {
                prop_assert_eq!(h.get(fil).read(fil).unwrap(), *v);
            }
            Ok(())
        })?;
    }
}
