//! Rust declarations for `fixtures/basic.c`.

#![allow(dead_code)]

use std::ffi::c_char;

use fil_rust::{FilPtr, FilType, Opaque, Ptr};

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 8, align = 4)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 16)]
pub struct Mixed {
    pub d: f64,
    pub i: i32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 32)]
pub struct Big {
    pub a: i64,
    pub b: i64,
    pub c: i64,
    pub d: i64,
}

#[derive(FilType, Clone, Copy, Debug)]
#[fil(size = 16)]
pub struct Named<'f> {
    pub name: FilPtr<'f, c_char>,
    pub len: i32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 4, align = 2)]
pub struct Inner {
    pub a: i16,
    pub b: i8,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 12, align = 4)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 24, align = 8)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 8, align = 4)]
pub struct FPair {
    pub f: f32,
    pub i: i32,
}

#[derive(FilType, Clone, Copy, Debug)]
#[fil(size = 40, align = 8)]
pub struct Outer<'f> {
    pub tag: i8,
    pub inner: Inner,
    pub arr: [i32; 3],
    pub d: f64,
    pub label: FilPtr<'f, c_char>,
}

#[derive(FilType, Clone, Copy, Debug)]
#[fil(size = 16)]
pub struct Node<'f> {
    pub value: i32,
    pub next: FilPtr<'f, Node<'static>>,
}

fil_rust::foreign! {
    pub struct Basic {
        fn add(a: i32, b: i32) -> i32;
        fn add_u8(a: u8, b: u8) -> u8;
        fn neg_i16(a: i16) -> i16;
        fn mul_i64(a: i64, b: i64) -> i64;
        fn max_u64() -> u64;
        fn scale(x: f64, factor: f32) -> f64;
        fn halve(x: f32) -> f32;
        fn is_even(x: i32) -> bool;
        fn do_nothing();
        fn sum12(a: i8, b: i16, c: i32, d: i64, e: f32, f: f64, g: u8, h: u16, i: u32, j: u64, k: i32, l: i64) -> i64;
        fn mix(a: i32, b: f64, c: i64, d: f32, e: i8, f: f64) -> f64;

        fn my_strlen(s: Ptr<c_char>) -> usize;
        fn dup_upper(s: Ptr<c_char>) -> Ptr<c_char>;
        fn fill(buf: Ptr<c_char>, n: usize, c: i8);
        fn sum_bytes(buf: Ptr<u8>, n: usize) -> i32;
        fn reverse_ints(a: Ptr<i32>, n: usize);
        fn read_at(a: Ptr<i32>, i: i64) -> i32;
        fn string_literal() -> Ptr<c_char>;
        fn format_into(buf: Ptr<c_char>, n: usize, x: i32, s: Ptr<c_char>) -> i32;
        fn split_words(s: Ptr<c_char>, count: Ptr<i32>) -> Ptr<Ptr<c_char>>;
        fn total_len(strs: Ptr<Ptr<c_char>>) -> usize;

        fn mkpt(x: i32, y: i32) -> Pt;
        fn pt_sum(p: Pt) -> i32;
        fn pt_add(a: Pt, b: Pt) -> Pt;
        fn pt_scale(p: Ptr<Pt>, k: i32);
        fn vec2_rot90(v: Vec2) -> Vec2;
        fn mixed_make(d: f64, i: i32) -> Mixed;
        fn mixed_total(m: Mixed) -> f64;
        fn big_make(base: i64) -> Big;
        fn big_sum(b: Big) -> i64;
        fn big_twice(b: Big) -> Big;
        fn named_make(s: Ptr<c_char>) -> Named<'static>;
        fn named_check(n: Named<'static>) -> i32;
        fn outer_make(label: Ptr<c_char>) -> Outer<'static>;
        fn inner_bump(v: Inner, k: i32) -> Inner;
        fn rgb_scale(c: Rgb, k: f32) -> Rgb;
        fn quad_dot(x: Quad, y: Quad) -> f32;
        fn vec3_cross(a: Vec3, b: Vec3) -> Vec3;
        fn fpair_swap(p: FPair) -> FPair;
        fn outer_sum(o: Ptr<Outer<'static>>) -> f64;

        fn list_build(n: i32) -> Ptr<Node<'static>>;
        fn list_sum(n: Ptr<Node<'static>>) -> i64;
        fn list_free(n: Ptr<Node<'static>>);

        fn bump() -> i32;
        fn get_counter() -> i32;
        fn free_it(p: Ptr);
        fn alloc_bytes(n: usize) -> Ptr<u8>;
        fn keep(p: Ptr);
        fn get_kept() -> Ptr;

        fn churn(rounds: i32, size: i32) -> i64;
        fn locked_add(x: i64) -> i64;
        fn locked_total() -> i64;
        fn say(s: Ptr<c_char>) -> i32;
    }
}

pub type IntFn = fn(i32) -> i32;

pub fn load(fil: &fil_rust::Fil<'_>) -> Basic {
    Basic::load(fil, super::fixture("basic.c")).expect("loading basic.c")
}

pub fn opaque<T>(p: FilPtr<'_, T>) -> FilPtr<'_, Opaque> {
    p.cast()
}
