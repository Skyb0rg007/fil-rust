//! Rust closures called from Fil-C.

mod common;

use std::ffi::c_char;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::basic::{Big, IntFn, Pt};
use common::{basic, runtime};
use fil_rust::{Callback, FilFn, FilPtr, Ptr};

#[test]
fn simple_callback() {
    let b = basic();
    runtime().run(|fil| {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let square = Callback::<IntFn>::new(fil, move |_fil, (x,)| {
            counter.fetch_add(1, Ordering::Relaxed);
            x * x
        })
        .unwrap();
        let apply = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap();
        assert_eq!(apply.call(fil, (square.get(fil), 7)).unwrap(), 49);
        let twice = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply_twice").unwrap();
        assert_eq!(twice.call(fil, (square.get(fil), 3)).unwrap(), 81);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        // Rust can call it too, through the Fil-C calling convention.
        assert_eq!(square.get(fil).call(fil, (-5,)).unwrap(), 25);
    });
}

#[test]
fn callback_with_floats_and_void() {
    let b = basic();
    runtime().run(|fil| {
        let hyp = fil.callback::<fn(f64, f64) -> f64, _>(|_, (a, b)| (a * a + b * b).sqrt()).unwrap();
        let apply_d = b.library().function::<fn(fn(f64, f64) -> f64, f64, f64) -> f64>(fil, "apply_d").unwrap();
        assert_eq!(apply_d.call(fil, (hyp.get(fil), 3.0, 4.0)).unwrap(), 5.0);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let visit = fil
            .callback::<fn(i32, i32), _>(move |_, (index, value)| sink.lock().unwrap().push((index, value)))
            .unwrap();
        let each = b.library().function::<fn(Ptr<i32>, usize, fn(i32, i32))>(fil, "each").unwrap();
        let data = fil.alloc_array::<i32>(&[10, 20, 30]).unwrap();
        each.call(fil, (data, 3, visit.get(fil))).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![(0, 10), (1, 20), (2, 30)]);
    });
}

#[test]
fn callbacks_with_structs() {
    let b = basic();
    runtime().run(|fil| {
        let flip = fil.callback::<fn(Pt) -> Pt, _>(|_, (p,)| Pt { x: p.y, y: p.x }).unwrap();
        let apply_pt = b.library().function::<fn(fn(Pt) -> Pt, Pt) -> Pt>(fil, "apply_pt").unwrap();
        assert_eq!(apply_pt.call(fil, (flip.get(fil), Pt { x: 1, y: 2 })).unwrap(), Pt { x: 2, y: 1 });

        // A large struct arrives inline and is returned through sret.
        let bump = fil
            .callback::<fn(Big) -> Big, _>(|_, (v,)| Big { a: v.a + 1, b: v.b + 2, c: v.c + 3, d: v.d + 4 })
            .unwrap();
        let apply_big = b.library().function::<fn(fn(Big) -> Big, Big) -> Big>(fil, "apply_big").unwrap();
        let r = apply_big.call(fil, (bump.get(fil), Big { a: 10, b: 20, c: 30, d: 40 })).unwrap();
        assert_eq!(r, Big { a: 11, b: 22, c: 33, d: 44 });
    });
}

#[test]
fn callbacks_receive_and_return_pointers() {
    let b = basic();
    runtime().run(|fil| {
        // Receives a C string with its capability; returns a new one.
        let shout = fil
            .callback::<fn(Ptr<c_char>) -> Ptr<c_char>, _>(|fil, (s,)| {
                let text = s.read_string(fil).unwrap();
                fil.c_str(&format!("{}!", text.to_uppercase())).unwrap()
            })
            .unwrap();
        let apply_str = b
            .library()
            .function::<fn(fn(Ptr<c_char>) -> Ptr<c_char>, Ptr<c_char>) -> Ptr<c_char>>(fil, "apply_str")
            .unwrap();
        let r = apply_str.call(fil, (shout.get(fil), fil.c_str("hey").unwrap())).unwrap();
        assert_eq!(r.read_string(fil).unwrap(), "HEY!");
    });
}

#[test]
fn qsort_with_a_rust_comparator() {
    let b = basic();
    runtime().run(|fil| {
        type Cmp = fn(Ptr<i32>, Ptr<i32>) -> i32;
        // Descending order; the comparator reads through checked pointers.
        let cmp = Callback::<Cmp>::new(fil, |fil, (a, b)| {
            let (a, b) = (a.read(fil).unwrap(), b.read(fil).unwrap());
            b.cmp(&a) as i32
        })
        .unwrap();
        let sort = b.library().function::<fn(Ptr<i32>, usize, Cmp)>(fil, "sort_ints").unwrap();
        let values: Vec<i32> = (0..500).map(|i| (i * 7919) % 1000 - 500).collect();
        let arr = fil.alloc_array::<i32>(&values).unwrap();
        sort.call(fil, (arr, values.len(), cmp.get(fil))).unwrap();
        let mut expected = values.clone();
        expected.sort_by(|a, b| b.cmp(a));
        assert_eq!(arr.read_array(fil, values.len()).unwrap(), expected);
    });
}

#[test]
fn callbacks_from_many_fil_c_threads() {
    let b = basic();
    let calls = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicI64::new(0));
    let (c2, t2) = (calls.clone(), total.clone());
    let result = runtime().run(|fil| {
        let cb = Callback::<IntFn>::new(fil, move |fil, (x,)| {
            c2.fetch_add(1, Ordering::Relaxed);
            t2.fetch_add(x as i64, Ordering::Relaxed);
            // Allocate in the callback to exercise the GC from these threads.
            let p = fil.alloc::<i32>(&x).unwrap();
            p.read(fil).unwrap() % 7
        })
        .unwrap();
        let run = b.library().function::<fn(IntFn, i32, i32) -> i64>(fil, "call_from_threads").unwrap();
        run.call(fil, (cb.get(fil), 8, 2000)).unwrap()
    });
    assert_eq!(calls.load(Ordering::Relaxed), 8 * 2000);
    let expected_total: i64 = (0..8).flat_map(|t| (0..2000).map(move |i| (t * 1000 + i) as i64)).sum();
    assert_eq!(total.load(Ordering::Relaxed), expected_total);
    let expected_result: i64 = (0..8).flat_map(|t| (0..2000).map(move |i| ((t * 1000 + i) % 7) as i64)).sum();
    assert_eq!(result, expected_result);
}

#[test]
fn callbacks_call_back_into_fil_c() {
    let b = basic();
    runtime().run(|fil| {
        let add = b.library().function::<fn(i32, i32) -> i32>(fil, "add").unwrap();
        let nested = Callback::<IntFn>::new(fil, move |fil, (x,)| {
            // Re-entering Fil-C from the callback, and nesting Runtime::run.
            let y = add.call(fil, (x, 100)).unwrap();
            fil.runtime().run(|inner| add.call(inner, (y, 1000)).unwrap())
        })
        .unwrap();
        assert_eq!(b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap().call(fil, (nested.get(fil), 1)).unwrap(), 1101);
    });
}

#[test]
fn callbacks_outlive_the_scope_that_created_them() {
    let rt = runtime();
    let b = basic();
    let cb = rt.run(|fil| Callback::<IntFn>::new(fil, |_, (x,)| x + 1).unwrap());
    // Use it from different jobs (and executor threads), after collections.
    for i in 0..10 {
        rt.run(|fil| {
            fil.collect_garbage().unwrap();
            let apply = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap();
            assert_eq!(apply.call(fil, (cb.get(fil), i)).unwrap(), i + 1);
        });
    }
    // Callbacks are Send + Sync.
    let cb = Arc::new(cb);
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let cb = cb.clone();
            std::thread::spawn(move || runtime().run(|fil| cb.get(fil).call(fil, (i,)).unwrap()))
        })
        .collect();
    let results: Vec<i32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results, vec![1, 2, 3, 4]);
}

#[test]
fn function_pointers_as_struct_fields() {
    #[derive(fil_rust::FilType, Clone, Copy)]
    struct Ops<'f> {
        unary: FilFn<'f, IntFn>,
        bias: i32,
    }
    let b = basic();
    runtime().run(|fil| {
        let neg = Callback::<IntFn>::new(fil, |_, (x,)| -x).unwrap();
        let ops = fil.alloc::<Ops<'static>>(&Ops { unary: neg.get(fil), bias: 3 }).unwrap();
        let back = ops.read(fil).unwrap();
        assert_eq!(back.unary.call(fil, (back.bias,)).unwrap(), -3);
        // And a C function pointer stored the same way.
        let add = b.library().symbol(fil, "add").unwrap();
        let slot = fil.alloc_zeroed::<FilFn<'static, fn(i32, i32) -> i32>>(1).unwrap();
        slot.write(fil, &add.as_fn().unwrap()).unwrap();
        assert_eq!(slot.read(fil).unwrap().call(fil, (2, 2)).unwrap(), 4);
    });
}

#[test]
fn many_callbacks() {
    let b = basic();
    runtime().run(|fil| {
        let apply = b.library().function::<fn(IntFn, i32) -> i32>(fil, "apply").unwrap();
        let cbs: Vec<Callback<IntFn>> =
            (0..300).map(|k| Callback::new(fil, move |_, (x,)| x * k).unwrap()).collect();
        for (k, cb) in cbs.iter().enumerate() {
            assert_eq!(apply.call(fil, (cb.get(fil), 2)).unwrap(), 2 * k as i32);
        }
    });
}

#[test]
fn callback_sees_null_pointers() {
    let b = basic();
    runtime().run(|fil| {
        let is_null = fil
            .callback::<fn(Ptr<c_char>) -> Ptr<c_char>, _>(|_, (s,)| {
                assert!(s.is_null());
                FilPtr::null()
            })
            .unwrap();
        let apply_str = b
            .library()
            .function::<fn(fn(Ptr<c_char>) -> Ptr<c_char>, Ptr<c_char>) -> Ptr<c_char>>(fil, "apply_str")
            .unwrap();
        assert!(apply_str.call(fil, (is_null.get(fil), FilPtr::null())).unwrap().is_null());
    });
}
