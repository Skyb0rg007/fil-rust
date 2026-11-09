//! `<ctype.h>` in the C locale. glibc's headers turn `isalpha` and the rest
//! into lookups in the tables `__ctype_b_loc` and its kind point into, 128
//! entries in so that `EOF` and negative `char`s index them, so those tables
//! are real objects here. They're the runtime's, read-only, and live forever.

use std::ffi::c_int;
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::natives;
use crate::{globals, object, ptrs, thread};

const UPPER: u16 = bit(0);
const LOWER: u16 = bit(1);
const ALPHA: u16 = bit(2);
const DIGIT: u16 = bit(3);
const XDIGIT: u16 = bit(4);
const SPACE: u16 = bit(5);
const PRINT: u16 = bit(6);
const GRAPH: u16 = bit(7);
const BLANK: u16 = bit(8);
const CNTRL: u16 = bit(9);
const PUNCT: u16 = bit(10);
const ALNUM: u16 = bit(11);

/// glibc's `_ISbit`: the bits of a big-endian short, read little-endian.
const fn bit(n: u32) -> u16 {
    if n < 8 { (1 << n) << 8 } else { (1 << n) >> 8 }
}

/// What the C locale says `c` is.
fn classes(c: c_int) -> u16 {
    let Ok(byte) = u8::try_from(c) else { return 0 };
    if !byte.is_ascii() {
        return 0;
    }
    let mut classes = 0;
    let mut set = |yes: bool, class: u16| {
        if yes {
            classes |= class;
        }
    };
    set(byte.is_ascii_uppercase(), UPPER);
    set(byte.is_ascii_lowercase(), LOWER);
    set(byte.is_ascii_alphabetic(), ALPHA);
    set(byte.is_ascii_digit(), DIGIT);
    set(byte.is_ascii_hexdigit(), XDIGIT);
    set(matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'), SPACE);
    set((0x20..0x7f).contains(&byte), PRINT);
    set(byte.is_ascii_graphic(), GRAPH);
    set(matches!(byte, b' ' | b'\t'), BLANK);
    set(byte.is_ascii_control(), CNTRL);
    set(byte.is_ascii_punctuation(), PUNCT);
    set(byte.is_ascii_alphanumeric(), ALNUM);
    classes
}

fn to_lower(c: c_int) -> c_int {
    if (b'A' as c_int..=b'Z' as c_int).contains(&c) { c + 32 } else { c }
}

fn to_upper(c: c_int) -> c_int {
    if (b'a' as c_int..=b'z' as c_int).contains(&c) { c - 32 } else { c }
}

/// The characters the tables cover: `EOF`, negative `char`s, and `unsigned
/// char`s.
const FIRST: c_int = -128;
const COUNT: usize = 384;

/// A read-only object holding one pointer, to `entries` of `T` made by
/// `entry`, 128 in.
unsafe fn make_table<T: Copy>(me: *mut Thread, entry: impl Fn(c_int) -> T) -> Ptr {
    unsafe {
        let size = size_of::<T>();
        let array = object::allocate(me, COUNT * size, 0);
        thread::track(me, array);
        let payload = Ptr::for_object(array);
        for index in 0..COUNT {
            payload.ptr.add(index * size).cast::<T>().write_unaligned(entry(FIRST + index as c_int));
        }
        object::make_readonly(array);
        globals::keep(array);
        let holder = object::allocate(me, WORD_SIZE, 0);
        thread::track(me, holder);
        let location = Ptr::for_object(holder);
        ptrs::store(me, location, payload.with_ptr(payload.ptr.add((-FIRST) as usize * size)));
        object::make_readonly(holder);
        globals::keep(holder);
        location
    }
}

/// The three tables' pointer holders, made once.
fn tables(me: *mut Thread) -> [Ptr; 3] {
    static TABLES: Mutex<Option<[(usize, usize); 3]>> = Mutex::new(None);
    let mut tables = unsafe { thread::lock_entered(me, &TABLES) };
    let made = tables.get_or_insert_with(|| unsafe {
        [make_table(me, classes), make_table(me, to_lower), make_table(me, to_upper)].map(|ptr| (ptr.ptr as usize, ptr.lower as usize))
    });
    made.map(|(ptr, lower)| Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 })
}

fn is(c: c_int, class: u16) -> c_int {
    (classes(c) & class != 0) as c_int
}

natives! {
    fn __ctype_b_loc(thread: *mut Thread) -> Ptr {
        tables(thread)[0]
    }

    fn __ctype_tolower_loc(thread: *mut Thread) -> Ptr {
        tables(thread)[1]
    }

    fn __ctype_toupper_loc(thread: *mut Thread) -> Ptr {
        tables(thread)[2]
    }

    fn __ctype_get_mb_cur_max(thread: *mut Thread) -> usize {
        1
    }

    fn isalnum(thread: *mut Thread, c: c_int) -> c_int { is(c, ALNUM) }
    fn isalpha(thread: *mut Thread, c: c_int) -> c_int { is(c, ALPHA) }
    fn isblank(thread: *mut Thread, c: c_int) -> c_int { is(c, BLANK) }
    fn iscntrl(thread: *mut Thread, c: c_int) -> c_int { is(c, CNTRL) }
    fn isdigit(thread: *mut Thread, c: c_int) -> c_int { is(c, DIGIT) }
    fn isgraph(thread: *mut Thread, c: c_int) -> c_int { is(c, GRAPH) }
    fn islower(thread: *mut Thread, c: c_int) -> c_int { is(c, LOWER) }
    fn isprint(thread: *mut Thread, c: c_int) -> c_int { is(c, PRINT) }
    fn ispunct(thread: *mut Thread, c: c_int) -> c_int { is(c, PUNCT) }
    fn isspace(thread: *mut Thread, c: c_int) -> c_int { is(c, SPACE) }
    fn isupper(thread: *mut Thread, c: c_int) -> c_int { is(c, UPPER) }
    fn isxdigit(thread: *mut Thread, c: c_int) -> c_int { is(c, XDIGIT) }
    fn isascii(thread: *mut Thread, c: c_int) -> c_int { (c & !0x7f == 0) as c_int }
    fn toascii(thread: *mut Thread, c: c_int) -> c_int { c & 0x7f }
    fn tolower(thread: *mut Thread, c: c_int) -> c_int { to_lower(c) }
    fn toupper(thread: *mut Thread, c: c_int) -> c_int { to_upper(c) }
    fn _tolower(thread: *mut Thread, c: c_int) -> c_int { to_lower(c) }
    fn _toupper(thread: *mut Thread, c: c_int) -> c_int { to_upper(c) }
    fn __isctype(thread: *mut Thread, c: c_int, mask: c_int) -> c_int {
        (classes(c) & mask as u16 != 0) as c_int
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![("pizlonated_isctype", __isctype::getter as *const ())]
}
