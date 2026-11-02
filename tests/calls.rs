//! Calling Fil-C functions: scalars, strings, structs, varargs, globals.

mod common;

use std::ffi::c_char;

use common::basic::{Big, FPair, Inner, Mixed, Named, Outer, Pt, Quad, Rgb, Vec2, Vec3};
use common::{basic, runtime};
use fil_rust::{Error, FilType, Function, Ptr};

#[test]
fn integer_arguments_and_returns() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.add(fil, 2, 3).unwrap(), 5);
        assert_eq!(b.add(fil, i32::MAX, 1).unwrap(), i32::MIN);
        assert_eq!(b.add(fil, -10, 4).unwrap(), -6);
        assert_eq!(b.add_u8(fil, 200, 100).unwrap(), 44);
        assert_eq!(b.neg_i16(fil, 300).unwrap(), -300);
        assert_eq!(b.neg_i16(fil, i16::MIN).unwrap(), i16::MIN);
        assert_eq!(b.mul_i64(fil, 1 << 40, 3).unwrap(), 3 << 40);
        assert_eq!(b.max_u64(fil).unwrap(), u64::MAX);
        assert!(b.is_even(fil, 4).unwrap());
        assert!(!b.is_even(fil, 7).unwrap());
        b.do_nothing(fil).unwrap();
    });
}

#[test]
fn float_arguments_and_returns() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.scale(fil, 1.5, 4.0).unwrap(), 6.0);
        assert_eq!(b.halve(fil, 5.0).unwrap(), 2.5);
        assert!(b.halve(fil, f32::NAN).unwrap().is_nan());
        assert_eq!(b.mix(fil, 1, 0.5, 100, 0.25, -2, 1000.0).unwrap(), 1099.75);
    });
}

#[test]
fn twelve_mixed_arguments() {
    let b = basic();
    let r = runtime()
        .run(|fil| b.sum12(fil, -1, -2, 3, 4, 5.9, 6.9, 7, 8, 9, 10, 11, 12))
        .unwrap();
    // The C side truncates the floats.
    assert_eq!(r, [-1i64, -2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].iter().sum::<i64>());
}

#[test]
fn strings_in_and_out() {
    let b = basic();
    runtime().run(|fil| {
        let s = fil.c_str("hello, world").unwrap();
        assert_eq!(b.my_strlen(fil, s).unwrap(), 12);
        let up = b.dup_upper(fil, s).unwrap();
        assert_eq!(up.read_string(fil).unwrap(), "HELLO, WORLD");
        // The result is a fresh malloc'd object.
        assert!(up.len() >= 13);
        // The input is untouched.
        assert_eq!(s.read_string(fil).unwrap(), "hello, world");
        let lit = b.string_literal(fil).unwrap();
        assert_eq!(lit.read_string(fil).unwrap(), "a string literal");
        assert!(lit.capability().unwrap().readonly);
    });
}

#[test]
fn buffers_written_by_c() {
    let b = basic();
    runtime().run(|fil| {
        let buf = fil.alloc_bytes(16).unwrap();
        b.fill(fil, buf.cast(), 10, b'z' as i8).unwrap();
        assert_eq!(buf.read_vec(fil, 16).unwrap(), b"zzzzzzzzzz\0\0\0\0\0\0");

        let data = fil.bytes(&[1, 2, 3, 250]).unwrap();
        assert_eq!(b.sum_bytes(fil, data, 4).unwrap(), 256);

        let ints = fil.alloc_array::<i32>(&[1, 2, 3, 4, 5]).unwrap();
        b.reverse_ints(fil, ints, 5).unwrap();
        assert_eq!(ints.read_array(fil, 5).unwrap(), vec![5, 4, 3, 2, 1]);

        let out = fil.alloc_bytes(32).unwrap();
        let s = fil.c_str("abc").unwrap();
        let n = b.format_into(fil, out.cast(), 32, 42, s).unwrap();
        assert_eq!(n, 6);
        assert_eq!(out.cast::<c_char>().read_string(fil).unwrap(), "42:abc");
    });
}

#[test]
fn pointers_to_pointers() {
    let b = basic();
    runtime().run(|fil| {
        let s = fil.c_str("the quick brown fox jumps").unwrap();
        let count = fil.alloc_zeroed::<i32>(1).unwrap();
        let words = b.split_words(fil, s, count).unwrap();
        let n = count.read(fil).unwrap() as usize;
        assert_eq!(n, 5);
        let strs: Vec<String> = (0..n)
            .map(|i| words.read_at(fil, i).unwrap().read_string(fil).unwrap())
            .collect();
        assert_eq!(strs, ["the", "quick", "brown", "fox", "jumps"]);
        assert!(words.read_at(fil, n).unwrap().is_null());
        assert_eq!(b.total_len(fil, words).unwrap(), 21);

        // Build a char** on the Rust side.
        let parts = ["ab", "cde", "f"];
        let arr = fil.alloc_zeroed::<Ptr<c_char>>(parts.len() + 1).unwrap();
        for (i, p) in parts.iter().enumerate() {
            arr.write_at(fil, i, &fil.c_str(p).unwrap()).unwrap();
        }
        assert_eq!(b.total_len(fil, arr).unwrap(), 6);
    });
}

#[test]
fn small_structs_by_value() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.mkpt(fil, 3, -4).unwrap(), Pt { x: 3, y: -4 });
        assert_eq!(b.pt_sum(fil, Pt { x: 10, y: 32 }).unwrap(), 42);
        assert_eq!(b.pt_add(fil, Pt { x: 1, y: 2 }, Pt { x: 10, y: 20 }).unwrap(), Pt { x: 11, y: 22 });
        assert_eq!(b.vec2_rot90(fil, Vec2 { x: 1.0, y: 2.0 }).unwrap(), Vec2 { x: -2.0, y: 1.0 });
        assert_eq!(b.mixed_make(fil, 0.5, 7).unwrap(), Mixed { d: 0.5, i: 7 });
        assert_eq!(b.mixed_total(fil, Mixed { d: 1.25, i: 2 }).unwrap(), 3.25);
    });
}

#[test]
fn structs_whose_lowering_depends_on_the_architecture() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.inner_bump(fil, Inner { a: -3, b: 100 }, 5).unwrap(), Inner { a: 2, b: 105 });
        assert_eq!(
            b.rgb_scale(fil, Rgb { r: 0.5, g: 1.0, b: 2.0 }, 4.0).unwrap(),
            Rgb { r: 2.0, g: 4.0, b: 8.0 }
        );
        let q = Quad { a: 1.0, b: 2.0, c: 3.0, d: 4.0 };
        assert_eq!(b.quad_dot(fil, q, Quad { a: 4.0, b: 3.0, c: 2.0, d: 1.0 }).unwrap(), 20.0);
        let x = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
        let y = Vec3 { x: 0.0, y: 1.0, z: 0.0 };
        assert_eq!(b.vec3_cross(fil, x, y).unwrap(), Vec3 { x: 0.0, y: 0.0, z: 1.0 });
        assert_eq!(b.fpair_swap(fil, FPair { f: 7.9, i: -2 }).unwrap(), FPair { f: -2.0, i: 7 });
    });
}

#[test]
fn large_structs_by_value_and_sret() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.big_make(fil, 10).unwrap(), Big { a: 10, b: 11, c: 12, d: 13 });
        assert_eq!(b.big_sum(fil, Big { a: 1, b: 2, c: 3, d: 4 }).unwrap(), 10);
        assert_eq!(
            b.big_twice(fil, Big { a: 1, b: -2, c: 3, d: i64::MAX / 2 }).unwrap(),
            Big { a: 2, b: -4, c: 6, d: i64::MAX - 1 }
        );
    });
}

#[test]
fn structs_holding_pointers() {
    let b = basic();
    runtime().run(|fil| {
        let s = fil.c_str("pointer").unwrap();
        let n = b.named_make(fil, s).unwrap();
        assert_eq!(n.len, 7);
        assert_eq!(n.name, s);
        assert_eq!(n.name.read_string(fil).unwrap(), "pointer");
        assert_eq!(b.named_check(fil, n).unwrap(), 1);
        assert_eq!(b.named_check(fil, Named { name: s, len: 3 }).unwrap(), 0);
        // A pointer with no capability travels as such: the callee sees NULL.
        assert_eq!(b.named_check(fil, Named { name: fil_rust::FilPtr::null(), len: 0 }).unwrap(), 0);
    });
}

#[test]
fn nested_structs_and_arrays() {
    assert_eq!(<Outer<'static> as FilType>::SIZE, 40);
    let b = basic();
    runtime().run(|fil| {
        let label = fil.c_str("lbl").unwrap();
        let o = b.outer_make(fil, label).unwrap();
        assert_eq!(o.tag, b'T' as i8);
        assert_eq!(o.inner, Inner { a: -7, b: b'q' as i8 });
        assert_eq!(o.arr, [1, 2, 3]);
        assert_eq!(o.d, 2.5);
        assert_eq!(o.label.read_string(fil).unwrap(), "lbl");

        let mine = Outer { tag: 1, inner: Inner { a: 2, b: 3 }, arr: [4, 5, 6], d: 0.5, label };
        let p = fil.alloc::<Outer<'static>>(&mine).unwrap();
        assert_eq!(b.outer_sum(fil, p).unwrap(), 1.0 + 2.0 + 3.0 + 15.0 + 0.5 + 3.0);
    });
}

#[test]
fn struct_pointers_mutated_by_c() {
    let b = basic();
    runtime().run(|fil| {
        let p = fil.alloc::<Pt>(&Pt { x: 2, y: -3 }).unwrap();
        b.pt_scale(fil, p, 5).unwrap();
        assert_eq!(p.read(fil).unwrap(), Pt { x: 10, y: -15 });
    });
}

#[test]
fn linked_lists() {
    let b = basic();
    runtime().run(|fil| {
        let head = b.list_build(fil, 100).unwrap();
        assert_eq!(b.list_sum(fil, head).unwrap(), 5050);
        // Walk it from Rust.
        let mut values = Vec::new();
        let mut node = head;
        while !node.is_null() {
            let n = node.read(fil).unwrap();
            values.push(n.value);
            node = n.next;
        }
        assert_eq!(values, (1..=100).collect::<Vec<_>>());
        // Splice in a node allocated by Rust.
        let tail = fil.alloc::<common::basic::Node<'static>>(&common::basic::Node {
            value: 1000,
            next: fil_rust::FilPtr::null(),
        });
        let tail = tail.unwrap();
        let second = head.read(fil).unwrap().next;
        let mut first = head.read(fil).unwrap();
        first.next = tail;
        head.write(fil, &first).unwrap();
        assert_eq!(b.list_sum(fil, head).unwrap(), 1001);
        // Free the C nodes we detached (the Rust node is freed too: Fil-C
        // allows free() on any heap object).
        b.list_free(fil, second).unwrap();
        b.list_free(fil, head).unwrap();
        assert_eq!(head.read(fil).unwrap_err(), Error::Freed);
    });
}

fil_rust::foreign! {
    /// Functions and globals of basic.c, declared together.
    struct Globals {
        static global_counter: i32;
        fn bump() -> i32;
        /// `const char *global_name`
        static mut global_name: Ptr<c_char>;
        static global_point: Pt;
        fn get_counter() -> i32;
        static global_array: [i32; 4];
    }
}

fil_rust::foreign! {
    struct MissingGlobal {
        static no_such_global: i32;
    }
}

fil_rust::foreign! {
    struct FunctionAsGlobal {
        static add: i32;
    }
}

/// Serializes the tests that write `global_counter`. Taken outside
/// `Runtime::run`, never while inside the runtime.
static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn globals_declared_with_foreign() {
    let b = basic();
    let _guard = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    runtime().run(|fil| {
        let g = Globals::from_library(fil, b.library().clone()).unwrap();
        g.global_counter(fil).write(fil, &99).unwrap();
        assert_eq!(g.bump(fil).unwrap(), 100);
        assert_eq!(g.get_counter(fil).unwrap(), 100);
        assert_eq!(g.global_counter(fil).read(fil).unwrap(), 100);
        assert_eq!(g.global_point(fil).read(fil).unwrap(), Pt { x: 3, y: 4 });
        assert_eq!(g.global_array(fil).read(fil).unwrap(), [10, 20, 30, 40]);
        assert!(!g.global_name(fil).read(fil).unwrap().is_null());

        // Globals are checked when the interface is created.
        let missing = MissingGlobal::from_library(fil, b.library().clone()).unwrap_err();
        assert!(matches!(missing, Error::MissingSymbol(ref s) if s == "no_such_global"), "{missing:?}");
        let special = FunctionAsGlobal::from_library(fil, b.library().clone()).unwrap_err();
        assert_eq!(special, Error::SpecialObject);
    });
}

#[test]
fn global_variables() {
    let rt = runtime();
    let b = basic();
    let _guard = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    rt.run(|fil| {
        let lib = b.library();
        let counter = lib.global::<i32>(fil, "global_counter").unwrap();
        counter.write(fil, &41).unwrap();
        assert_eq!(b.bump(fil).unwrap(), 42);
        assert_eq!(counter.read(fil).unwrap(), 42);

        let name = lib.global::<Ptr<c_char>>(fil, "global_name").unwrap();
        assert_eq!(name.read(fil).unwrap().read_string(fil).unwrap(), "fixture");
        name.write(fil, &fil.c_str("renamed").unwrap()).unwrap();
        assert_eq!(name.read(fil).unwrap().read_string(fil).unwrap(), "renamed");

        let point = lib.global::<Pt>(fil, "global_point").unwrap();
        assert_eq!(point.read(fil).unwrap(), Pt { x: 3, y: 4 });
        let array = lib.global::<[i32; 4]>(fil, "global_array").unwrap();
        assert_eq!(array.read(fil).unwrap(), [10, 20, 30, 40]);
        assert_eq!(array.cast::<i32>().len(), 4);
        assert_eq!(array.cast::<i32>().read_at(fil, 4).unwrap_err(), Error::OutOfBounds);

        // Functions aren't data.
        assert_eq!(lib.global::<i32>(fil, "add").unwrap_err(), Error::SpecialObject);
        assert!(matches!(lib.global::<i32>(fil, "no_such_global"), Err(Error::MissingSymbol(_))));
    });
}

#[test]
fn variadic_functions() {
    let b = basic();
    runtime().run(|fil| {
        let lib = b.library();
        let sum_ints = lib.function::<fn(i32) -> i32>(fil, "sum_ints").unwrap();
        let r = sum_ints.dyn_call(fil).arg(4i32).arg(10i32).arg(20i32).arg(30i32).arg(40i32).call::<i32>();
        assert_eq!(r.unwrap(), 100);

        let sum_doubles = lib.function::<fn(i32) -> f64>(fil, "sum_doubles").unwrap();
        let r = sum_doubles.dyn_call(fil).arg(3i32).arg(0.5f64).arg(0.25f64).arg(2.0f64).call::<f64>();
        assert_eq!(r.unwrap(), 2.75);

        let snprintf = lib.function::<fn(Ptr<c_char>, usize, Ptr<c_char>) -> i32>(fil, "my_snprintf").unwrap();
        let buf = fil.alloc_bytes(64).unwrap();
        let n = snprintf
            .dyn_call(fil)
            .arg(buf)
            .arg(64usize)
            .arg(fil.c_str("%s=%d (%.2f) %c").unwrap())
            .arg(fil.c_str("x").unwrap())
            .arg(-17i32)
            .arg(2.5051f64)
            .arg(b'!' as i32)
            .call::<i32>()
            .unwrap();
        assert_eq!(buf.cast::<c_char>().read_string(fil).unwrap(), "x=-17 (2.51) !");
        assert_eq!(n, 14);

        // libc's own printf family works the same way.
        let libc_snprintf = fil.libc().function::<fn(Ptr<c_char>, usize, Ptr<c_char>) -> i32>(fil, "snprintf").unwrap();
        libc_snprintf
            .dyn_call(fil)
            .arg(buf)
            .arg(64usize)
            .arg(fil.c_str("[%5s|%-3d]").unwrap())
            .arg(fil.c_str("ab").unwrap())
            .arg(7i32)
            .call::<i32>()
            .unwrap();
        assert_eq!(buf.cast::<c_char>().read_string(fil).unwrap(), "[   ab|7  ]");
    });
}

#[test]
fn structs_through_dyn_call() {
    let b = basic();
    runtime().run(|fil| {
        let f = b.library().function::<fn()>(fil, "big_twice").unwrap();
        let r = f.dyn_call(fil).arg_value::<Big>(&Big { a: 1, b: 2, c: 3, d: 4 }).call::<Big>().unwrap();
        assert_eq!(r, Big { a: 2, b: 4, c: 6, d: 8 });
        let g = b.library().function::<fn()>(fil, "pt_sum").unwrap();
        assert_eq!(g.dyn_call(fil).arg_value::<Pt>(&Pt { x: 5, y: 6 }).call::<i32>().unwrap(), 11);
    });
}

#[test]
fn libc_functions() {
    runtime().run(|fil| {
        let libc = fil.libc();
        let atoi = libc.function::<fn(Ptr<c_char>) -> i32>(fil, "atoi").unwrap();
        assert_eq!(atoi.call(fil, (fil.c_str("  -1234xyz").unwrap(),)).unwrap(), -1234);
        let strtod = libc.function::<fn(Ptr<c_char>, Ptr<Ptr<c_char>>) -> f64>(fil, "strtod").unwrap();
        let end = fil.alloc_zeroed::<Ptr<c_char>>(1).unwrap();
        let s = fil.c_str("2.5e3rest").unwrap();
        assert_eq!(strtod.call(fil, (s, end)).unwrap(), 2500.0);
        let rest = end.read(fil).unwrap();
        assert_eq!(rest.addr() - s.addr(), 5);
        assert_eq!(rest.read_string(fil).unwrap(), "rest");
        let malloc = libc.function::<fn(usize) -> Ptr<u8>>(fil, "malloc").unwrap();
        let p = malloc.call(fil, (100,)).unwrap();
        assert!(p.len() >= 100);
        let free = libc.function::<fn(Ptr<u8>)>(fil, "free").unwrap();
        free.call(fil, (p,)).unwrap();
        assert!(p.capability().unwrap().freed);
        assert_eq!(p.read_vec(fil, 1).unwrap_err(), Error::Freed);
        assert!(fil.version().unwrap() > 0);
    });
}

#[test]
fn functions_are_send_and_sync() {
    let b = basic();
    let add: Function<fn(i32, i32) -> i32> = runtime().run(|fil| b.library().function(fil, "add")).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|i| std::thread::spawn(move || runtime().run(|fil| add.call(fil, (i, i)).unwrap())))
        .collect();
    let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results, [0, 2, 4, 6]);
}

#[test]
fn function_pointers_returned_by_c() {
    let b = basic();
    runtime().run(|fil| {
        let get = b.library().function::<fn() -> Ptr>(fil, "get_add_fn").unwrap();
        let p = get.call(fil, ()).unwrap();
        let f = p.as_fn::<fn(i32, i32) -> i32>().unwrap();
        assert_eq!(f.call(fil, (20, 22)).unwrap(), 42);
        // Data pointers aren't functions.
        let data = fil.alloc_bytes(8).unwrap();
        assert_eq!(data.as_fn::<fn()>().unwrap_err(), Error::NotAFunction);
        assert_eq!(fil_rust::FilPtr::<u8>::null().as_fn::<fn()>().unwrap_err(), Error::NullCapability);
    });
}

#[test]
fn missing_symbols() {
    let b = basic();
    runtime().run(|fil| {
        let err = b.library().function::<fn()>(fil, "definitely_missing").unwrap_err();
        assert_eq!(err, Error::MissingSymbol("definitely_missing".into()));
        assert!(!b.library().has_symbol("definitely_missing"));
        assert!(b.library().has_symbol("add"));
    });
}

#[test]
fn return_type_larger_than_callee_returns() {
    let b = basic();
    runtime().run(|fil| {
        // do_nothing returns void (one 8-byte slot); asking for a 16-byte
        // struct must fail cleanly rather than read stale buffer contents.
        let f = b.library().function::<fn() -> Mixed>(fil, "do_nothing").unwrap();
        assert!(matches!(f.call(fil, ()), Err(Error::ReturnTooSmall { .. })));
    });
}

#[test]
fn printf_output_is_flushed() {
    let b = basic();
    runtime().run(|fil| {
        assert_eq!(b.say(fil, fil.c_str("hello from Fil-C").unwrap()).unwrap(), 17);
        fil.flush().unwrap();
    });
}
