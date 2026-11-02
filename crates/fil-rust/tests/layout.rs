//! `#[derive(FilType)]` against the C compiler: for struct shapes covering
//! padding, nested structs and arrays, pointers, function pointers, and the
//! calling convention's register classes, the derived layout must match C's
//! and values must survive memory, by-value calls, and callbacks unchanged.

mod common;

use std::ffi::c_char;

use common::{fixture, runtime};
use fil_rust::{Callback, FilFn, FilPtr, FilType, Library, Ptr};

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Padded {
    a: c_char,
    b: f64,
    c: c_char,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Shorts {
    a: [i8; 3],
    b: i16,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Floats2 {
    a: f32,
    b: f32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Doubles5 {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct PtrMid<'f> {
    a: i8,
    p: FilPtr<'f, i32>,
    b: i8,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Nested {
    inner: Shorts,
    x: u64,
    flag: bool,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct U16x5 {
    a: [u16; 5],
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct One {
    c: c_char,
}

#[derive(FilType, Clone, Copy, Debug)]
struct WithFn<'f> {
    f: FilFn<'f, fn(i32) -> i32>,
    x: i32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct Points {
    pts: [Floats2; 3],
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct MixedInts {
    a: u8,
    b: u32,
    c: u16,
    d: u64,
    e: i8,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
struct IntFloat {
    i: i32,
    f: f32,
}

#[derive(FilType, Clone, Copy, Debug)]
struct ThreePtrs<'f> {
    a: FilPtr<'f, c_char>,
    b: FilPtr<'f, c_char>,
    c: FilPtr<'f, c_char>,
}

fn library() -> Library {
    runtime().load(fixture("layouts.c")).unwrap()
}

macro_rules! check_shape {
    ($lib:expr, $ty:ty, $name:literal, |$fil:ident| $reference:expr, |$v:ident| $bump:expr, $eq:expr) => {{
        let lib: &Library = $lib;
        runtime().run(|$fil| {
            let eq = $eq;
            let size = lib.global::<usize>($fil, concat!($name, "_size")).unwrap().read($fil).unwrap();
            let align = lib.global::<usize>($fil, concat!($name, "_align")).unwrap().read($fil).unwrap();
            assert_eq!((<$ty as FilType>::SIZE, <$ty as FilType>::ALIGN), (size, align), "{}: size and alignment", $name);

            let reference = $reference;
            fn bump<'f>($v: &<$ty as FilType>::Value<'f>) -> <$ty as FilType>::Value<'f> {
                $bump
            }

            // C writes, Rust reads.
            let fill = lib.function::<fn(Ptr<$ty>)>($fil, concat!($name, "_fill")).unwrap();
            let p = $fil.alloc_zeroed::<$ty>(1).unwrap();
            fill.call($fil, (p,)).unwrap();
            let got = p.read($fil).unwrap();
            assert!(eq(&got, &reference), "{}: C wrote {:?}, expected {:?}", $name, got, reference);

            // Rust writes, C reads.
            let check = lib.function::<fn(Ptr<$ty>) -> i32>($fil, concat!($name, "_check")).unwrap();
            let q = $fil.alloc::<$ty>(&reference).unwrap();
            assert_eq!(check.call($fil, (q,)).unwrap(), 1, "{}: C didn't see the value Rust wrote", $name);

            // By value, both ways.
            let c_bump = lib.function::<fn($ty) -> $ty>($fil, concat!($name, "_bump")).unwrap();
            let bumped = c_bump.call($fil, (reference,)).unwrap();
            assert!(eq(&bumped, &bump(&reference)), "{}: by value: {:?}", $name, bumped);

            // Through a Rust callback: C passes the struct to Rust and gets
            // Rust's result back.
            let via = lib.function::<fn(fn($ty) -> $ty, $ty) -> $ty>($fil, concat!($name, "_via")).unwrap();
            let cb = Callback::<fn($ty) -> $ty>::new($fil, move |_, ($v,)| $bump).unwrap();
            let back = via.call($fil, (cb.get($fil), reference)).unwrap();
            assert!(eq(&back, &bump(&reference)), "{}: through a callback: {:?}", $name, back);
        });
    }};
}

fn same<T: PartialEq>(a: &T, b: &T) -> bool {
    a == b
}

#[test]
fn scalars_and_padding() {
    let lib = library();
    check_shape!(&lib, Padded, "padded", |fil| Padded { a: b'a' as c_char, b: 2.5, c: b'c' as c_char },
        |v| Padded { a: v.a + 1, b: v.b * 2.0, c: v.c + 1 }, same);
    check_shape!(&lib, Shorts, "shorts", |fil| Shorts { a: [1, 2, 3], b: -300 },
        |v| Shorts { a: [v.a[0] + 1, v.a[1] + 1, v.a[2] + 1], b: v.b - 1 }, same);
    check_shape!(&lib, One, "one", |fil| One { c: b'z' as c_char }, |v| One { c: v.c - 1 }, same);
    check_shape!(&lib, MixedInts, "mixed_ints",
        |fil| MixedInts { a: 200, b: 4_000_000_000, c: 60000, d: u64::MAX, e: -100 },
        |v| MixedInts { a: v.a + 1, b: v.b + 1, c: v.c + 1, d: v.d - 1, e: v.e - 1 }, same);
}

#[test]
fn floating_point_classes() {
    let lib = library();
    check_shape!(&lib, Floats2, "floats2", |fil| Floats2 { a: 1.5, b: -0.25 },
        |v| Floats2 { a: v.a + 1.0, b: v.b + 1.0 }, same);
    check_shape!(&lib, IntFloat, "int_float", |fil| IntFloat { i: -7, f: 3.5 },
        |v| IntFloat { i: v.i * 2, f: v.f * 2.0 }, same);
    check_shape!(&lib, Doubles5, "doubles5", |fil| Doubles5 { a: 1.0, b: 2.0, c: 3.0, d: 4.0, e: 5.0 },
        |v| Doubles5 { a: v.a + 10.0, b: v.b + 10.0, c: v.c + 10.0, d: v.d + 10.0, e: v.e + 10.0 }, same);
}

#[test]
fn nested_structs_and_arrays() {
    let lib = library();
    check_shape!(&lib, Nested, "nested",
        |fil| Nested { inner: Shorts { a: [1, 2, 3], b: -300 }, x: 0x1122334455667788, flag: true },
        |v| Nested {
            inner: Shorts { a: [v.inner.a[0] + 1, v.inner.a[1] + 1, v.inner.a[2] + 1], b: v.inner.b - 1 },
            x: v.x + 1,
            flag: !v.flag,
        }, same);
    check_shape!(&lib, U16x5, "u16x5", |fil| U16x5 { a: [1000, 2000, 3000, 4000, 5000] },
        |v| U16x5 { a: v.a.map(|x| x + 1) }, same);
    check_shape!(&lib, Points, "points",
        |fil| Points { pts: [0.0f32, 1.0, 2.0].map(|i| Floats2 { a: i, b: -i }) },
        |v| Points { pts: v.pts.map(|p| Floats2 { a: p.a + 1.0, b: p.b + 1.0 }) }, same);
}

#[test]
fn pointer_fields() {
    let lib = library();
    check_shape!(&lib, PtrMid<'static>, "ptr_mid",
        |fil| PtrMid { a: -1, p: lib.global::<i32>(fil, "anchor").unwrap(), b: 1 },
        |v| PtrMid { a: v.a - 1, p: v.p, b: v.b + 1 }, same);
    check_shape!(&lib, ThreePtrs<'static>, "three_ptrs",
        |fil| ThreePtrs { a: fil.c_str("a").unwrap(), b: fil.c_str("bb").unwrap(), c: fil.c_str("ccc").unwrap() },
        |v| ThreePtrs { a: v.c, b: v.b, c: v.a },
        |x: &ThreePtrs<'_>, y: &ThreePtrs<'_>| {
            [(x.a, y.a), (x.b, y.b), (x.c, y.c)]
                .iter()
                .all(|(p, q)| p.read_string(fil).unwrap() == q.read_string(fil).unwrap())
        });
}

#[test]
fn function_pointer_fields() {
    let lib = library();
    check_shape!(&lib, WithFn<'static>, "with_fn",
        |fil| WithFn { f: lib.symbol(fil, "inc").unwrap().as_fn().unwrap(), x: 41 },
        |v| WithFn { f: v.f, x: v.x + 1 },
        |x: &WithFn<'_>, y: &WithFn<'_>| x.x == y.x && x.f.as_ptr() == y.f.as_ptr());
}
