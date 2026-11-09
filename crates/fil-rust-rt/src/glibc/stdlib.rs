//! `<stdlib.h>`: allocation, and number parsing over the host's parsers.

use std::ffi::{c_char, c_int, c_long, c_longlong, c_ulong, c_ulonglong};
use std::ptr::null_mut;

use crate::abi::*;
use crate::natives::natives;
use crate::glibc::set_errno;
use crate::view::Bytes;
use crate::{natives, object, ptrs};

const PAGE: usize = 4096;

unsafe fn allocate(me: *mut Thread, size: usize, alignment: usize) -> Ptr {
    Ptr::for_object(unsafe { object::allocate_aligned(me, size, alignment, 0) })
}

/// What `calloc` and `reallocarray` do when `count * size` overflows.
unsafe fn too_big(me: *mut Thread) -> Ptr {
    unsafe { set_errno(me, libc::ENOMEM) };
    Ptr::NULL
}

/// Runs a host parser on a copy of the string at `string`: up to its
/// terminator, or the end of its object, whichever is first. The end it
/// reports goes to `end`, with `string`'s capability, and its errno to the
/// thread's. A parse that runs into the end of an unterminated string fails,
/// as compiled code reading the next byte would; one that stops before it
/// doesn't, as numbers are often parsed out of the middle of a buffer.
unsafe fn parse<T>(me: *mut Thread, string: Ptr, end: Ptr, parser: impl FnOnce(*const c_char, *mut *mut c_char) -> T) -> T {
    let bytes = Bytes::readable(string);
    let length = (0..bytes.len()).find(|&index| bytes.get(index) == 0).unwrap_or(bytes.len());
    let mut copy = bytes.to_vec(length);
    copy.push(0);
    let mut stop: *mut c_char = null_mut();
    let (value, error) = unsafe {
        *libc::__errno_location() = 0;
        let value = parser(copy.as_ptr().cast(), &mut stop);
        (value, *libc::__errno_location())
    };
    // The host leaves the end alone for a bad base.
    let consumed = (!stop.is_null()).then(|| stop as usize - copy.as_ptr() as usize);
    if consumed == Some(length) && length == bytes.len() {
        bytes.get(length);
    }
    if error != 0 {
        unsafe { set_errno(me, error) };
    }
    if let Some(consumed) = consumed
        && !end.ptr.is_null()
    {
        unsafe { ptrs::store(me, end, bytes.at(consumed)) };
    }
    value
}

unsafe extern "C" {
    fn __isoc23_strtol(string: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long;
    fn __isoc23_strtoul(string: *const c_char, end: *mut *mut c_char, base: c_int) -> c_ulong;
    fn __isoc23_strtoll(string: *const c_char, end: *mut *mut c_char, base: c_int) -> c_longlong;
    fn __isoc23_strtoull(string: *const c_char, end: *mut *mut c_char, base: c_int) -> c_ulonglong;
}

natives! {
    fn malloc(thread: *mut Thread, size: usize) -> Ptr {
        unsafe { allocate(thread, size, MIN_ALIGN) }
    }

    fn free(thread: *mut Thread, ptr_: Ptr) -> () {
        unsafe { object::free(ptr_) };
    }

    fn calloc(thread: *mut Thread, count: usize, size: usize) -> Ptr {
        match count.checked_mul(size) {
            Some(total) => unsafe { allocate(thread, total, MIN_ALIGN) },
            None => unsafe { too_big(thread) },
        }
    }

    fn realloc(thread: *mut Thread, old: Ptr, size: usize) -> Ptr {
        // glibc frees for a size of 0.
        if size == 0 && !old.ptr.is_null() {
            unsafe { object::free(old) };
            return Ptr::NULL;
        }
        unsafe { natives::realloc(thread, old, size, MIN_ALIGN) }
    }

    fn reallocarray(thread: *mut Thread, old: Ptr, count: usize, size: usize) -> Ptr {
        match count.checked_mul(size) {
            Some(total) => unsafe { natives::realloc(thread, old, total, MIN_ALIGN) },
            None => unsafe { too_big(thread) },
        }
    }

    fn aligned_alloc(thread: *mut Thread, alignment: usize, size: usize) -> Ptr {
        if !alignment.is_power_of_two() {
            unsafe { set_errno(thread, libc::EINVAL) };
            return Ptr::NULL;
        }
        unsafe { allocate(thread, size, alignment) }
    }

    fn memalign(thread: *mut Thread, alignment: usize, size: usize) -> Ptr {
        if !alignment.is_power_of_two() {
            unsafe { set_errno(thread, libc::EINVAL) };
            return Ptr::NULL;
        }
        unsafe { allocate(thread, size, alignment) }
    }

    fn posix_memalign(thread: *mut Thread, place: Ptr, alignment: usize, size: usize) -> c_int {
        if !alignment.is_power_of_two() || !alignment.is_multiple_of(WORD_SIZE) {
            return libc::EINVAL;
        }
        let allocated = unsafe { allocate(thread, size, alignment) };
        unsafe { ptrs::store(thread, place, allocated) };
        0
    }

    fn valloc(thread: *mut Thread, size: usize) -> Ptr {
        unsafe { allocate(thread, size, PAGE) }
    }

    fn pvalloc(thread: *mut Thread, size: usize) -> Ptr {
        unsafe { allocate(thread, size.next_multiple_of(PAGE), PAGE) }
    }

    fn malloc_usable_size(thread: *mut Thread, ptr_: Ptr) -> usize {
        if ptr_.ptr.is_null() {
            return 0;
        }
        Bytes::readable(ptr_).len()
    }

    fn strtol(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| libc::strtol(s, e, base)) }
    }

    fn strtoul(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| libc::strtoul(s, e, base)) }
    }

    fn strtoll(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| libc::strtoll(s, e, base)) }
    }

    fn strtoull(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| libc::strtoull(s, e, base)) }
    }

    fn __isoc23_strtol(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| __isoc23_strtol(s, e, base)) }
    }

    fn __isoc23_strtoul(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| __isoc23_strtoul(s, e, base)) }
    }

    fn __isoc23_strtoll(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| __isoc23_strtoll(s, e, base)) }
    }

    fn __isoc23_strtoull(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| __isoc23_strtoull(s, e, base)) }
    }

    fn strtod(thread: *mut Thread, string: Ptr, end: Ptr) -> f64 {
        unsafe { parse(thread, string, end, |s, e| libc::strtod(s, e)) }
    }

    fn strtof(thread: *mut Thread, string: Ptr, end: Ptr) -> f32 {
        unsafe { parse(thread, string, end, |s, e| libc::strtof(s, e)) }
    }

    // A `double`'s worth of the value, as a `long double`.
    fn strtold(thread: *mut Thread, string: Ptr, end: Ptr) -> natives::LongDouble {
        natives::LongDouble(unsafe { parse(thread, string, end, |s, e| libc::strtod(s, e)) })
    }

    fn atoi(thread: *mut Thread, string: Ptr) -> c_int {
        unsafe { parse(thread, string, Ptr::NULL, |s, e| libc::strtol(s, e, 10)) as c_int }
    }

    fn atol(thread: *mut Thread, string: Ptr) -> i64 {
        unsafe { parse(thread, string, Ptr::NULL, |s, e| libc::strtol(s, e, 10)) }
    }

    fn atoll(thread: *mut Thread, string: Ptr) -> i64 {
        unsafe { parse(thread, string, Ptr::NULL, |s, e| libc::strtoll(s, e, 10)) }
    }

    fn atof(thread: *mut Thread, string: Ptr) -> f64 {
        unsafe { parse(thread, string, Ptr::NULL, |s, e| libc::strtod(s, e)) }
    }
}

/// glibc's other names for some of these.
pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated_cfree", free::getter as *const ()),
        ("pizlonated_strtoq", strtoll::getter as *const ()),
        ("pizlonated_strtouq", strtoull::getter as *const ()),
        ("pizlonated_strtoimax", strtoll::getter as *const ()),
        ("pizlonated_strtoumax", strtoull::getter as *const ()),
        ("pizlonated___isoc23_strtoimax", __isoc23_strtoll::getter as *const ()),
        ("pizlonated___isoc23_strtoumax", __isoc23_strtoull::getter as *const ()),
        // The C locale's, which is the only one: the locale comes last and
        // goes unread.
        ("pizlonated_strtol_l", strtol::getter as *const ()),
        ("pizlonated___strtol_l", strtol::getter as *const ()),
        ("pizlonated_strtoul_l", strtoul::getter as *const ()),
        ("pizlonated___strtoul_l", strtoul::getter as *const ()),
        ("pizlonated_strtoll_l", strtoll::getter as *const ()),
        ("pizlonated___strtoll_l", strtoll::getter as *const ()),
        ("pizlonated_strtoull_l", strtoull::getter as *const ()),
        ("pizlonated___strtoull_l", strtoull::getter as *const ()),
        ("pizlonated___isoc23_strtol_l", __isoc23_strtol::getter as *const ()),
        ("pizlonated___isoc23_strtoul_l", __isoc23_strtoul::getter as *const ()),
        ("pizlonated___isoc23_strtoll_l", __isoc23_strtoll::getter as *const ()),
        ("pizlonated___isoc23_strtoull_l", __isoc23_strtoull::getter as *const ()),
        ("pizlonated_strtod_l", strtod::getter as *const ()),
        ("pizlonated___strtod_l", strtod::getter as *const ()),
        ("pizlonated___strtod_internal", strtod::getter as *const ()),
        ("pizlonated_strtof_l", strtof::getter as *const ()),
        ("pizlonated___strtof_l", strtof::getter as *const ()),
        ("pizlonated___strtof_internal", strtof::getter as *const ()),
        ("pizlonated_strtold_l", strtold::getter as *const ()),
        ("pizlonated___strtold_l", strtold::getter as *const ()),
        ("pizlonated___strtold_internal", strtold::getter as *const ()),
    ]
}
