//! `<string.h>` and `<strings.h>`, over [`crate::view`]: a scan reads up to
//! the terminator or to the end of the object, and failing that, fails the way
//! compiled code reading past the end would.

use std::ffi::c_int;

use crate::abi::*;
use crate::memops;
use crate::natives::natives;
use crate::object::Access;
use crate::ptrs;
use crate::view::{self, Bytes};

/// `strnlen`: the length of the string at `ptr`, or `limit` if it's longer.
/// Past the end of the object without a terminator, this fails.
fn bounded_len(ptr: Ptr, limit: usize) -> usize {
    let bytes = Bytes::readable_up_to(ptr, limit);
    (0..limit).find(|&index| bytes.get(index) == 0).unwrap_or(limit)
}

/// The sign of the first difference, as `memcmp` and its kind give it.
fn compare(a: Ptr, b: Ptr, limit: usize, stop_at_nul: bool, fold: bool) -> c_int {
    let (a, b) = (Bytes::readable_up_to(a, limit), Bytes::readable_up_to(b, limit));
    for index in 0..limit {
        let (mut x, mut y) = (a.get(index), b.get(index));
        if fold {
            (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        }
        if x != y {
            return x as c_int - y as c_int;
        }
        if stop_at_nul && x == 0 {
            break;
        }
    }
    0
}

/// Where `needle` first is in `haystack`.
fn search(haystack: &[u8], needle: &[u8], fold: bool) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| {
        if fold { window.eq_ignore_ascii_case(needle) } else { window == needle }
    })
}

/// `bytes` and a terminator.
fn terminated(bytes: &[u8]) -> Vec<u8> {
    let mut copy = Vec::with_capacity(bytes.len() + 1);
    copy.extend_from_slice(bytes);
    copy.push(0);
    copy
}

/// The span of `string` whose bytes `set` does (`in_set`) or doesn't contain.
fn span(string: Ptr, set: Ptr, in_set: bool) -> usize {
    let set = view::c_string(set);
    let bytes = Bytes::readable(string);
    (0..).find(|&index| {
        let byte = bytes.get(index);
        byte == 0 || set.contains(&byte) != in_set
    })
    .unwrap()
}

/// The next token of what `start` points into, `strtok_r`'s way, and where to
/// go on from.
fn next_token(start: Ptr, delimiters: Ptr) -> (Ptr, Ptr) {
    let delimiters = view::c_string(delimiters);
    let bytes = Bytes::readable(start);
    let first = (0..).find(|&index| !delimiters.contains(&bytes.get(index)) || bytes.get(index) == 0).unwrap();
    if bytes.get(first) == 0 {
        return (Ptr::NULL, bytes.at(first));
    }
    let end = (first..).find(|&index| bytes.get(index) == 0 || delimiters.contains(&bytes.get(index))).unwrap();
    if bytes.get(end) == 0 {
        return (bytes.at(first), bytes.at(end));
    }
    view::write(bytes.at(end), &[0]);
    (bytes.at(first), bytes.at(end + 1))
}

natives! {
    fn strlen(thread: *mut Thread, string: Ptr) -> usize {
        Bytes::readable(string).c_str_len()
    }

    fn strnlen(thread: *mut Thread, string: Ptr, limit: usize) -> usize {
        bounded_len(string, limit)
    }

    fn strcmp(thread: *mut Thread, a: Ptr, b: Ptr) -> c_int {
        compare(a, b, usize::MAX, true, false)
    }

    fn strncmp(thread: *mut Thread, a: Ptr, b: Ptr, limit: usize) -> c_int {
        compare(a, b, limit, true, false)
    }

    fn strcasecmp(thread: *mut Thread, a: Ptr, b: Ptr) -> c_int {
        compare(a, b, usize::MAX, true, true)
    }

    fn strncasecmp(thread: *mut Thread, a: Ptr, b: Ptr, limit: usize) -> c_int {
        compare(a, b, limit, true, true)
    }

    // The C locale's collation is the bytes'.
    fn strcoll(thread: *mut Thread, a: Ptr, b: Ptr) -> c_int {
        compare(a, b, usize::MAX, true, false)
    }

    fn strxfrm(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> usize {
        let string = view::c_string(src);
        if string.len() < size {
            view::write(dst, &terminated(&string));
        }
        string.len()
    }

    fn memcmp(thread: *mut Thread, a: Ptr, b: Ptr, count: usize) -> c_int {
        compare(a, b, count, false, false)
    }

    fn bcmp(thread: *mut Thread, a: Ptr, b: Ptr, count: usize) -> c_int {
        compare(a, b, count, false, false)
    }

    fn __memcmpeq(thread: *mut Thread, a: Ptr, b: Ptr, count: usize) -> c_int {
        compare(a, b, count, false, false)
    }

    fn memcpy(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) -> Ptr {
        unsafe { memops::move_memory(thread, dst, src, count) };
        dst
    }

    fn memmove(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) -> Ptr {
        unsafe { memops::move_memory(thread, dst, src, count) };
        dst
    }

    fn mempcpy(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) -> Ptr {
        unsafe { memops::move_memory(thread, dst, src, count) };
        dst.with_ptr(dst.ptr.wrapping_add(count))
    }

    fn memset(thread: *mut Thread, dst: Ptr, value: c_int, count: usize) -> Ptr {
        unsafe { memops::set_memory(dst, value as u32, count) };
        dst
    }

    fn bzero(thread: *mut Thread, dst: Ptr, count: usize) -> () {
        unsafe { memops::set_memory(dst, 0, count) };
    }

    fn explicit_bzero(thread: *mut Thread, dst: Ptr, count: usize) -> () {
        unsafe { memops::set_memory(dst, 0, count) };
    }

    fn memccpy(thread: *mut Thread, dst: Ptr, src: Ptr, stop: c_int, count: usize) -> Ptr {
        let bytes = Bytes::readable_up_to(src, count);
        let found = (0..count).find(|&index| bytes.get(index) == stop as u8);
        let copied = found.map_or(count, |index| index + 1);
        unsafe { memops::move_memory(thread, dst, src, copied) };
        match found {
            Some(_) => dst.with_ptr(dst.ptr.wrapping_add(copied)),
            None => Ptr::NULL,
        }
    }

    fn memchr(thread: *mut Thread, string: Ptr, wanted: c_int, count: usize) -> Ptr {
        let bytes = Bytes::readable_up_to(string, count);
        match (0..count).find(|&index| bytes.get(index) == wanted as u8) {
            Some(index) => bytes.at(index),
            None => Ptr::NULL,
        }
    }

    fn memrchr(thread: *mut Thread, string: Ptr, wanted: c_int, count: usize) -> Ptr {
        let bytes = Bytes::exactly(string, count, Access::Read);
        match (0..count).rev().find(|&index| bytes.get(index) == wanted as u8) {
            Some(index) => bytes.at(index),
            None => Ptr::NULL,
        }
    }

    fn rawmemchr(thread: *mut Thread, string: Ptr, wanted: c_int) -> Ptr {
        let bytes = Bytes::readable(string);
        bytes.at((0..).find(|&index| bytes.get(index) == wanted as u8).unwrap())
    }

    fn memmem(thread: *mut Thread, haystack: Ptr, haystack_len: usize, needle: Ptr, needle_len: usize) -> Ptr {
        let hay = Bytes::exactly(haystack, haystack_len, Access::Read);
        let needle = Bytes::exactly(needle, needle_len, Access::Read).to_vec(needle_len);
        match search(&hay.to_vec(haystack_len), &needle, false) {
            Some(index) => hay.at(index),
            None => Ptr::NULL,
        }
    }

    fn strchr(thread: *mut Thread, string: Ptr, wanted: c_int) -> Ptr {
        let bytes = Bytes::readable(string);
        let mut index = 0;
        loop {
            let byte = bytes.get(index);
            if byte == wanted as u8 {
                return bytes.at(index);
            }
            if byte == 0 {
                return Ptr::NULL;
            }
            index += 1;
        }
    }

    fn strchrnul(thread: *mut Thread, string: Ptr, wanted: c_int) -> Ptr {
        let bytes = Bytes::readable(string);
        bytes.at((0..).find(|&index| matches!(bytes.get(index), 0) || bytes.get(index) == wanted as u8).unwrap())
    }

    fn strrchr(thread: *mut Thread, string: Ptr, wanted: c_int) -> Ptr {
        let bytes = Bytes::readable(string);
        let length = bytes.c_str_len();
        match (0..=length).rev().find(|&index| bytes.get(index) == wanted as u8) {
            Some(index) => bytes.at(index),
            None => Ptr::NULL,
        }
    }

    fn strstr(thread: *mut Thread, haystack: Ptr, needle: Ptr) -> Ptr {
        match search(&view::c_string(haystack), &view::c_string(needle), false) {
            Some(index) => haystack.with_ptr(haystack.ptr.wrapping_add(index)),
            None => Ptr::NULL,
        }
    }

    fn strcasestr(thread: *mut Thread, haystack: Ptr, needle: Ptr) -> Ptr {
        match search(&view::c_string(haystack), &view::c_string(needle), true) {
            Some(index) => haystack.with_ptr(haystack.ptr.wrapping_add(index)),
            None => Ptr::NULL,
        }
    }

    fn strspn(thread: *mut Thread, string: Ptr, accept: Ptr) -> usize {
        span(string, accept, true)
    }

    fn strcspn(thread: *mut Thread, string: Ptr, reject: Ptr) -> usize {
        span(string, reject, false)
    }

    fn strpbrk(thread: *mut Thread, string: Ptr, accept: Ptr) -> Ptr {
        let index = span(string, accept, false);
        if Bytes::readable(string).get(index) == 0 {
            Ptr::NULL
        } else {
            string.with_ptr(string.ptr.wrapping_add(index))
        }
    }

    fn strcpy(thread: *mut Thread, dst: Ptr, src: Ptr) -> Ptr {
        view::write(dst, &terminated(&view::c_string(src)));
        dst
    }

    fn stpcpy(thread: *mut Thread, dst: Ptr, src: Ptr) -> Ptr {
        let string = view::c_string(src);
        view::write(dst, &terminated(&string));
        dst.with_ptr(dst.ptr.wrapping_add(string.len()))
    }

    fn strncpy(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> Ptr {
        let length = bounded_len(src, size);
        let mut padded = Bytes::readable_up_to(src, length).to_vec(length);
        padded.resize(size, 0);
        view::write(dst, &padded);
        dst
    }

    fn stpncpy(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> Ptr {
        let length = bounded_len(src, size);
        let mut padded = Bytes::readable_up_to(src, length).to_vec(length);
        padded.resize(size, 0);
        view::write(dst, &padded);
        dst.with_ptr(dst.ptr.wrapping_add(length))
    }

    fn strcat(thread: *mut Thread, dst: Ptr, src: Ptr) -> Ptr {
        let end = Bytes::readable(dst).c_str_len();
        view::write(dst.with_ptr(dst.ptr.wrapping_add(end)), &terminated(&view::c_string(src)));
        dst
    }

    fn strncat(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> Ptr {
        let end = Bytes::readable(dst).c_str_len();
        let length = bounded_len(src, size);
        let string = Bytes::readable_up_to(src, length).to_vec(length);
        view::write(dst.with_ptr(dst.ptr.wrapping_add(end)), &terminated(&string));
        dst
    }

    fn strlcpy(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> usize {
        let string = view::c_string(src);
        if size != 0 {
            view::write(dst, &terminated(&string[..string.len().min(size - 1)]));
        }
        string.len()
    }

    fn strlcat(thread: *mut Thread, dst: Ptr, src: Ptr, size: usize) -> usize {
        let end = bounded_len(dst, size);
        let string = view::c_string(src);
        if end < size {
            let room = size - end - 1;
            view::write(dst.with_ptr(dst.ptr.wrapping_add(end)), &terminated(&string[..string.len().min(room)]));
        }
        end + string.len()
    }

    fn strdup(thread: *mut Thread, string: Ptr) -> Ptr {
        unsafe { view::new_c_string(thread, &view::c_string(string)) }
    }

    fn strndup(thread: *mut Thread, string: Ptr, limit: usize) -> Ptr {
        let length = bounded_len(string, limit);
        unsafe { view::new_c_string(thread, &Bytes::readable_up_to(string, length).to_vec(length)) }
    }

    fn strtok_r(thread: *mut Thread, string: Ptr, delimiters: Ptr, save: Ptr) -> Ptr {
        let start = if string.ptr.is_null() { unsafe { ptrs::load(save) } } else { string };
        if start.ptr.is_null() {
            return Ptr::NULL;
        }
        let (token, rest) = next_token(start, delimiters);
        unsafe { ptrs::store(thread, save, rest) };
        token
    }

    fn strtok(thread: *mut Thread, string: Ptr, delimiters: Ptr) -> Ptr {
        let state = &mut unsafe { crate::thread::extra(thread) }.libc;
        let start = if string.ptr.is_null() { state.strtok } else { string };
        if start.ptr.is_null() {
            return Ptr::NULL;
        }
        let (token, rest) = next_token(start, delimiters);
        unsafe { crate::thread::extra(thread) }.libc.strtok = rest;
        token
    }

    fn strsep(thread: *mut Thread, place: Ptr, delimiters: Ptr) -> Ptr {
        let start = unsafe { ptrs::load(place) };
        if start.ptr.is_null() {
            return Ptr::NULL;
        }
        let delimiters = view::c_string(delimiters);
        let bytes = Bytes::readable(start);
        let end = (0..).find(|&index| bytes.get(index) == 0 || delimiters.contains(&bytes.get(index))).unwrap();
        let rest = if bytes.get(end) == 0 {
            Ptr::NULL
        } else {
            view::write(bytes.at(end), &[0]);
            bytes.at(end + 1)
        };
        unsafe { ptrs::store(thread, place, rest) };
        start
    }
}

/// glibc's other names for some of these.
pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated___strdup", strdup::getter as *const ()),
        ("pizlonated___strndup", strndup::getter as *const ()),
        ("pizlonated___strtok_r", strtok_r::getter as *const ()),
        ("pizlonated___stpcpy", stpcpy::getter as *const ()),
        ("pizlonated___stpncpy", stpncpy::getter as *const ()),
        ("pizlonated___mempcpy", mempcpy::getter as *const ()),
        ("pizlonated___rawmemchr", rawmemchr::getter as *const ()),
        ("pizlonated___strcasecmp", strcasecmp::getter as *const ()),
        ("pizlonated___strcasestr", strcasestr::getter as *const ()),
        ("pizlonated___bzero", bzero::getter as *const ()),
        ("pizlonated_index", strchr::getter as *const ()),
        ("pizlonated_rindex", strrchr::getter as *const ()),
    ]
}
