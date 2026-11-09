//! `<wchar.h>` and `<wctype.h>`. Wide strings are read through the view a
//! `wchar_t` at a time, with the terminator inside the object unless a count
//! bounds them. Conversions and classification are the host's, in the C
//! locale the process runs in, handed copies; their results are written back
//! through checked pointers.

use std::ffi::{c_char, c_int, c_long, c_longlong, c_ulong, c_ulonglong};

use crate::abi::*;
use crate::natives::{LongDouble, Rest, natives};
use crate::object::{self, Access};
use crate::start::set_errno;
use crate::view::{self, Bytes};
use crate::{ptrs, thread};

use super::formatting;
use super::varargs::Arguments;

type Wide = libc::wchar_t;
const UNIT: usize = size_of::<Wide>();

/// The unit at `index` in `bytes`.
fn unit(bytes: Bytes, index: usize) -> Wide {
    Wide::from_le_bytes(std::array::from_fn(|offset| bytes.get(index * UNIT + offset)))
}

/// The wide string at `at`, without its terminator: up to `limit` units if
/// there's a limit, and otherwise up to a terminator that has to be in the
/// object.
fn wide(at: Ptr, limit: Option<usize>) -> Vec<Wide> {
    let bytes = match limit {
        Some(limit) => Bytes::readable_up_to(at, limit.saturating_mul(UNIT)),
        None => Bytes::readable(at),
    };
    let units = bytes.len() / UNIT;
    let mut out = Vec::new();
    for index in 0..units {
        let value = unit(bytes, index);
        if value == 0 {
            return out;
        }
        out.push(value);
    }
    if limit.is_none_or(|limit| units < limit) {
        crate::failure::safety_panic(format_args!("wide string is not null-terminated (ptr = {:p}).", at.ptr));
    }
    out
}

/// `count` units at `at`, which have to be there.
fn units(at: Ptr, count: usize) -> Vec<Wide> {
    let bytes = Bytes::exactly(at, count.saturating_mul(UNIT), Access::Read);
    (0..count).map(|index| unit(bytes, index)).collect()
}

fn bytes_of(units: &[Wide]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// Writes `units` at `at`.
fn put(at: Ptr, units: &[Wide]) {
    view::write(at, &bytes_of(units));
}

fn offset(at: Ptr, units: usize) -> Ptr {
    at.with_ptr(at.ptr.wrapping_add(units * UNIT))
}

/// A copy of the conversion state at `state`, or a fresh one for null.
fn state_in(state: Ptr) -> libc::mbstate_t {
    let mut copy: libc::mbstate_t = unsafe { std::mem::zeroed() };
    if !state.ptr.is_null() {
        let bytes = Bytes::exactly(state, size_of::<libc::mbstate_t>(), Access::Write).to_vec(size_of::<libc::mbstate_t>());
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&raw mut copy).cast::<u8>(), bytes.len()) };
    }
    copy
}

fn state_out(state: Ptr, copy: &libc::mbstate_t) {
    if !state.ptr.is_null() {
        let bytes = unsafe { std::slice::from_raw_parts((copy as *const libc::mbstate_t).cast::<u8>(), size_of::<libc::mbstate_t>()) };
        view::write(state, bytes);
    }
}

/// Runs a host call, giving its `errno` to the thread's if it set one.
unsafe fn host<T>(me: *mut Thread, call: impl FnOnce() -> T) -> T {
    unsafe {
        *libc::__errno_location() = 0;
        let value = call();
        let error = *libc::__errno_location();
        if error != 0 {
            set_errno(me, error);
        }
        value
    }
}

/// Runs a host parser on a terminated copy of the wide string at `string`,
/// storing where it stopped at `end`, as `strtol` and its kind do.
unsafe fn parse<T>(me: *mut Thread, string: Ptr, end: Ptr, parser: impl FnOnce(*const Wide, *mut *mut Wide) -> T) -> T {
    let bytes = Bytes::readable(string);
    let available = bytes.len() / UNIT;
    let mut copy: Vec<Wide> = (0..available).map(|index| unit(bytes, index)).take_while(|&unit| unit != 0).collect();
    let length = copy.len();
    copy.push(0);
    let mut stop = std::ptr::null_mut::<Wide>();
    let value = unsafe { host(me, || parser(copy.as_ptr(), &mut stop)) };
    let consumed = (!stop.is_null()).then(|| (stop as usize - copy.as_ptr() as usize) / UNIT);
    if consumed == Some(length) && length == available {
        // It ran into the end of the object, which compiled code would read.
        bytes.get(length * UNIT);
    }
    if let Some(consumed) = consumed
        && !end.ptr.is_null()
    {
        unsafe { ptrs::store(me, end, offset(string, consumed)) };
    }
    value
}

/// What `mbsrtowcs` and `mbsnrtowcs` do: converts the string `*source`
/// points to, at most `limit` bytes of it, into at most `count` units at
/// `destination`, or counts them for a null one.
unsafe fn to_wide(me: *mut Thread, destination: Ptr, source: Ptr, limit: Option<usize>, count: usize, state: Ptr) -> usize {
    let string = unsafe { ptrs::load(source) };
    let bytes = match limit {
        Some(limit) => Bytes::readable_up_to(string, limit),
        None => Bytes::readable(string),
    };
    let length = bytes.find(0).map_or_else(|| if limit.is_some() { bytes.len() } else { bytes.c_str_len() }, |at| at + 1);
    let copy = bytes.to_vec(length);
    let mut out = vec![0 as Wide; if destination.ptr.is_null() { 0 } else { count }];
    let mut at = copy.as_ptr().cast::<c_char>();
    let mut converted = state_in(state);
    let result = unsafe {
        host(me, || {
            c::mbsnrtowcs(
                if destination.ptr.is_null() { std::ptr::null_mut() } else { out.as_mut_ptr() },
                &mut at,
                copy.len(),
                count,
                &mut converted,
            )
        })
    };
    state_out(state, &converted);
    if destination.ptr.is_null() || result == usize::MAX {
        return result;
    }
    let written = (result + 1).min(count);
    put(destination, &out[..written.min(out.len())]);
    let next = if at.is_null() { Ptr::NULL } else { string.with_ptr(string.ptr.wrapping_add(at as usize - copy.as_ptr() as usize)) };
    unsafe { ptrs::store(me, source, next) };
    result
}

/// What `wcsrtombs` and `wcsnrtombs` do, the other way.
unsafe fn to_bytes(me: *mut Thread, destination: Ptr, source: Ptr, limit: Option<usize>, count: usize, state: Ptr) -> usize {
    let string = unsafe { ptrs::load(source) };
    let mut copy = match limit {
        Some(limit) => {
            let bytes = Bytes::readable_up_to(string, limit.saturating_mul(UNIT));
            (0..bytes.len() / UNIT).map(|index| unit(bytes, index)).collect::<Vec<_>>()
        }
        None => wide(string, None),
    };
    let ended = copy.iter().position(|&unit| unit == 0);
    if let Some(ended) = ended {
        copy.truncate(ended);
    }
    let terminated = ended.is_some() || limit.is_none();
    let length = copy.len();
    copy.push(0);
    let mut out = vec![0u8; if destination.ptr.is_null() { 0 } else { count }];
    let mut at = copy.as_ptr();
    let mut converted = state_in(state);
    let result = unsafe {
        host(me, || {
            c::wcsnrtombs(
                if destination.ptr.is_null() { std::ptr::null_mut() } else { out.as_mut_ptr().cast() },
                &mut at,
                if terminated { length + 1 } else { length },
                count,
                &mut converted,
            )
        })
    };
    state_out(state, &converted);
    if destination.ptr.is_null() || result == usize::MAX {
        return result;
    }
    let written = if at.is_null() { result + 1 } else { result };
    view::write(destination, &out[..written.min(out.len())]);
    let next = if at.is_null() { Ptr::NULL } else { offset(string, (at as usize - copy.as_ptr() as usize) / UNIT) };
    unsafe { ptrs::store(me, source, next) };
    result
}

/// `swprintf`'s: the format as bytes, formatted, and back to wide.
unsafe fn print_wide(me: *mut Thread, buffer: Ptr, size: usize, format: Ptr, arguments: Arguments) -> c_int {
    let format: Vec<u8> = wide(format, None).iter().map(|&unit| if (0..0x80).contains(&unit) { unit as u8 } else { b'?' }).collect();
    let text = unsafe { formatting::format_text(me, &format, arguments) };
    let mut out = vec![0 as Wide; text.len() + 1];
    let mut copy = text.clone();
    copy.push(0);
    let count = unsafe { c::mbstowcs(out.as_mut_ptr(), copy.as_ptr().cast(), out.len()) };
    if count == usize::MAX {
        unsafe { set_errno(me, libc::EILSEQ) };
        return -1;
    }
    if size == 0 {
        return -1;
    }
    if count >= size {
        out.truncate(size - 1);
        out.push(0);
        put(buffer, &out);
        return -1;
    }
    put(buffer, &out[..count + 1]);
    count as c_int
}

/// The host's.
mod c {
    use super::*;

    unsafe extern "C" {
        pub fn __isoc23_wcstol(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_long;
        pub fn __isoc23_wcstoul(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_ulong;
        pub fn __isoc23_wcstoll(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_longlong;
        pub fn __isoc23_wcstoull(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_ulonglong;
        pub fn wcstod(string: *const Wide, end: *mut *mut Wide) -> f64;
        pub fn wcstof(string: *const Wide, end: *mut *mut Wide) -> f32;
        pub fn wcstol(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_long;
        pub fn wcstoul(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_ulong;
        pub fn wcstoll(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_longlong;
        pub fn wcstoull(string: *const Wide, end: *mut *mut Wide, base: c_int) -> c_ulonglong;
        pub fn btowc(c: c_int) -> u32;
        pub fn wctob(c: u32) -> c_int;
        pub fn mbtowc(out: *mut Wide, s: *const c_char, n: usize) -> c_int;
        pub fn mblen(s: *const c_char, n: usize) -> c_int;
        pub fn wctomb(s: *mut c_char, c: Wide) -> c_int;
        pub fn iswalnum(c: u32) -> c_int;
        pub fn iswalpha(c: u32) -> c_int;
        pub fn iswblank(c: u32) -> c_int;
        pub fn iswcntrl(c: u32) -> c_int;
        pub fn iswdigit(c: u32) -> c_int;
        pub fn iswgraph(c: u32) -> c_int;
        pub fn iswlower(c: u32) -> c_int;
        pub fn iswprint(c: u32) -> c_int;
        pub fn iswpunct(c: u32) -> c_int;
        pub fn iswspace(c: u32) -> c_int;
        pub fn iswupper(c: u32) -> c_int;
        pub fn iswxdigit(c: u32) -> c_int;
        pub fn towlower(c: u32) -> u32;
        pub fn towupper(c: u32) -> u32;
        pub fn wcwidth(c: Wide) -> c_int;
        pub fn wctype(name: *const c_char) -> c_ulong;
        pub fn iswctype(c: u32, class: c_ulong) -> c_int;
        pub fn wctrans(name: *const c_char) -> *const i32;
        pub fn towctrans(c: u32, translation: *const i32) -> u32;
        pub fn mbsnrtowcs(dst: *mut Wide, src: *mut *const c_char, nms: usize, len: usize, ps: *mut libc::mbstate_t) -> usize;
        pub fn wcsnrtombs(dst: *mut c_char, src: *mut *const Wide, nwc: usize, len: usize, ps: *mut libc::mbstate_t) -> usize;
        pub fn mbstowcs(dst: *mut Wide, src: *const c_char, len: usize) -> usize;
        pub fn mbrtowc(out: *mut Wide, s: *const c_char, n: usize, ps: *mut libc::mbstate_t) -> usize;
        pub fn mbrlen(s: *const c_char, n: usize, ps: *mut libc::mbstate_t) -> usize;
        pub fn wcrtomb(s: *mut c_char, c: Wide, ps: *mut libc::mbstate_t) -> usize;
        pub fn mbsinit(ps: *const libc::mbstate_t) -> c_int;
    }
}

/// `wcscmp`'s order.
fn compare(a: &[Wide], b: &[Wide]) -> c_int {
    for (x, y) in a.iter().chain(&[0]).zip(b.iter().chain(&[0])) {
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}

/// `wcsncmp`'s: the first `count` units at most, stopping at a terminator.
fn compare_bounded(a: Ptr, b: Ptr, count: usize) -> c_int {
    let (a, b) = (wide(a, Some(count)), wide(b, Some(count)));
    let pad = |units: &[Wide]| -> Vec<Wide> { units.iter().copied().chain(std::iter::once(0)).take(count).collect() };
    compare(&pad(&a), &pad(&b))
}

fn name(at: Ptr) -> std::ffi::CString {
    unsafe { std::ffi::CString::from_vec_unchecked(view::c_string(at)) }
}

natives! {
    fn wcslen(thread: *mut Thread, string: Ptr) -> usize {
        wide(string, None).len()
    }

    fn wcsnlen(thread: *mut Thread, string: Ptr, count: usize) -> usize {
        wide(string, Some(count)).len()
    }

    fn wcscmp(thread: *mut Thread, a: Ptr, b: Ptr) -> c_int {
        compare(&wide(a, None), &wide(b, None))
    }

    fn wcsncmp(thread: *mut Thread, a: Ptr, b: Ptr, count: usize) -> c_int {
        compare_bounded(a, b, count)
    }

    fn wcschr(thread: *mut Thread, string: Ptr, c: u32) -> Ptr {
        let units = wide(string, None);
        if c == 0 {
            return offset(string, units.len());
        }
        units.iter().position(|&unit| unit as u32 == c).map_or(Ptr::NULL, |at| offset(string, at))
    }

    fn wcsrchr(thread: *mut Thread, string: Ptr, c: u32) -> Ptr {
        let units = wide(string, None);
        if c == 0 {
            return offset(string, units.len());
        }
        units.iter().rposition(|&unit| unit as u32 == c).map_or(Ptr::NULL, |at| offset(string, at))
    }

    fn wcscpy(thread: *mut Thread, destination: Ptr, source: Ptr) -> Ptr {
        let mut units = wide(source, None);
        units.push(0);
        put(destination, &units);
        destination
    }

    fn wcsncpy(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> Ptr {
        let mut units = wide(source, Some(count));
        units.resize(count, 0);
        put(destination, &units);
        destination
    }

    fn wcscat(thread: *mut Thread, destination: Ptr, source: Ptr) -> Ptr {
        let start = wide(destination, None).len();
        let mut units = wide(source, None);
        units.push(0);
        put(offset(destination, start), &units);
        destination
    }

    fn wcsncat(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> Ptr {
        let start = wide(destination, None).len();
        let mut units = wide(source, Some(count));
        units.push(0);
        put(offset(destination, start), &units);
        destination
    }

    fn wcsdup(thread: *mut Thread, source: Ptr) -> Ptr {
        let mut units = wide(source, None);
        units.push(0);
        let object = unsafe { object::allocate(thread, units.len() * UNIT, 0) };
        unsafe { thread::track(thread, object) };
        let copy = Ptr::for_object(object);
        put(copy, &units);
        copy
    }

    fn wcsxfrm(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> usize {
        let mut units = wide(source, None);
        let length = units.len();
        if length < count {
            units.push(0);
            put(destination, &units);
        }
        length
    }

    fn wmemchr(thread: *mut Thread, string: Ptr, c: u32, count: usize) -> Ptr {
        let bytes = Bytes::readable_up_to(string, count.saturating_mul(UNIT));
        for index in 0..count {
            if unit(bytes, index) as u32 == c {
                return offset(string, index);
            }
        }
        Ptr::NULL
    }

    fn wmemcmp(thread: *mut Thread, a: Ptr, b: Ptr, count: usize) -> c_int {
        let (a, b) = (Bytes::readable_up_to(a, count.saturating_mul(UNIT)), Bytes::readable_up_to(b, count.saturating_mul(UNIT)));
        for index in 0..count {
            let (x, y) = (unit(a, index), unit(b, index));
            if x != y {
                return if x < y { -1 } else { 1 };
            }
        }
        0
    }

    fn wmemcpy(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> Ptr {
        put(destination, &units(source, count));
        destination
    }

    fn wmemmove(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> Ptr {
        put(destination, &units(source, count));
        destination
    }

    fn wmempcpy(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> Ptr {
        put(destination, &units(source, count));
        offset(destination, count)
    }

    fn wmemset(thread: *mut Thread, destination: Ptr, c: u32, count: usize) -> Ptr {
        put(destination, &vec![c as Wide; count]);
        destination
    }

    fn wcstol(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| c::wcstol(s, e, base)) }
    }

    fn wcstoul(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| c::wcstoul(s, e, base)) }
    }

    fn wcstoll(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| c::wcstoll(s, e, base)) }
    }

    fn wcstoull(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| c::wcstoull(s, e, base)) }
    }

    fn __isoc23_wcstol(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| c::__isoc23_wcstol(s, e, base)) }
    }

    fn __isoc23_wcstoul(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| c::__isoc23_wcstoul(s, e, base)) }
    }

    fn __isoc23_wcstoll(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> i64 {
        unsafe { parse(thread, string, end, |s, e| c::__isoc23_wcstoll(s, e, base)) }
    }

    fn __isoc23_wcstoull(thread: *mut Thread, string: Ptr, end: Ptr, base: c_int) -> u64 {
        unsafe { parse(thread, string, end, |s, e| c::__isoc23_wcstoull(s, e, base)) }
    }

    fn wcstod(thread: *mut Thread, string: Ptr, end: Ptr) -> f64 {
        unsafe { parse(thread, string, end, |s, e| c::wcstod(s, e)) }
    }

    fn wcstof(thread: *mut Thread, string: Ptr, end: Ptr) -> f32 {
        unsafe { parse(thread, string, end, |s, e| c::wcstof(s, e)) }
    }

    fn wcstold(thread: *mut Thread, string: Ptr, end: Ptr) -> LongDouble {
        LongDouble(unsafe { parse(thread, string, end, |s, e| c::wcstod(s, e)) })
    }

    fn btowc(thread: *mut Thread, c: c_int) -> u32 {
        unsafe { c::btowc(c) }
    }

    fn wctob(thread: *mut Thread, c: u32) -> c_int {
        unsafe { c::wctob(c) }
    }

    fn mbrtowc(thread: *mut Thread, out: Ptr, string: Ptr, count: usize, state: Ptr) -> usize {
        let mut converted = state_in(state);
        if string.ptr.is_null() {
            let result = unsafe { host(thread, || c::mbrtowc(std::ptr::null_mut(), std::ptr::null(), 0, &mut converted)) };
            state_out(state, &converted);
            return result;
        }
        let bytes = Bytes::readable_up_to(string, count);
        let copy = bytes.to_vec(bytes.len());
        let mut unit: Wide = 0;
        let result = unsafe { host(thread, || c::mbrtowc(&mut unit, copy.as_ptr().cast(), copy.len(), &mut converted)) };
        state_out(state, &converted);
        if result < usize::MAX - 1 && !out.ptr.is_null() {
            put(out, &[unit]);
        }
        result
    }

    fn mbrlen(thread: *mut Thread, string: Ptr, count: usize, state: Ptr) -> usize {
        let mut converted = state_in(state);
        let bytes = Bytes::readable_up_to(string, count);
        let copy = bytes.to_vec(bytes.len());
        let result = unsafe { host(thread, || c::mbrlen(copy.as_ptr().cast(), copy.len(), &mut converted)) };
        state_out(state, &converted);
        result
    }

    fn mbtowc(thread: *mut Thread, out: Ptr, string: Ptr, count: usize) -> c_int {
        if string.ptr.is_null() {
            return 0;
        }
        let bytes = Bytes::readable_up_to(string, count);
        let copy = bytes.to_vec(bytes.len());
        let mut unit: Wide = 0;
        let result = unsafe { host(thread, || c::mbtowc(&mut unit, copy.as_ptr().cast(), copy.len())) };
        if result >= 0 && !out.ptr.is_null() {
            put(out, &[unit]);
        }
        result
    }

    fn mblen(thread: *mut Thread, string: Ptr, count: usize) -> c_int {
        if string.ptr.is_null() {
            return 0;
        }
        let bytes = Bytes::readable_up_to(string, count);
        let copy = bytes.to_vec(bytes.len());
        unsafe { host(thread, || c::mblen(copy.as_ptr().cast(), copy.len())) }
    }

    fn wcrtomb(thread: *mut Thread, out: Ptr, c: Wide, state: Ptr) -> usize {
        let mut converted = state_in(state);
        let mut buffer = [0u8; 16];
        let result = unsafe {
            host(thread, || {
                if out.ptr.is_null() {
                    c::wcrtomb(std::ptr::null_mut(), c, &mut converted)
                } else {
                    c::wcrtomb(buffer.as_mut_ptr().cast(), c, &mut converted)
                }
            })
        };
        state_out(state, &converted);
        if !out.ptr.is_null() && result != usize::MAX {
            view::write(out, &buffer[..result]);
        }
        result
    }

    fn wctomb(thread: *mut Thread, out: Ptr, c: Wide) -> c_int {
        if out.ptr.is_null() {
            return 0;
        }
        let mut buffer = [0u8; 16];
        let result = unsafe { host(thread, || c::wctomb(buffer.as_mut_ptr().cast(), c)) };
        if result > 0 {
            view::write(out, &buffer[..result as usize]);
        }
        result
    }

    fn mbsrtowcs(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize, state: Ptr) -> usize {
        unsafe { to_wide(thread, destination, source, None, count, state) }
    }

    fn mbsnrtowcs(thread: *mut Thread, destination: Ptr, source: Ptr, limit: usize, count: usize, state: Ptr) -> usize {
        unsafe { to_wide(thread, destination, source, Some(limit), count, state) }
    }

    fn wcsrtombs(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize, state: Ptr) -> usize {
        unsafe { to_bytes(thread, destination, source, None, count, state) }
    }

    fn wcsnrtombs(thread: *mut Thread, destination: Ptr, source: Ptr, limit: usize, count: usize, state: Ptr) -> usize {
        unsafe { to_bytes(thread, destination, source, Some(limit), count, state) }
    }

    fn mbstowcs(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> usize {
        let mut bytes = view::c_string(source);
        bytes.push(0);
        let mut out = vec![0 as Wide; if destination.ptr.is_null() { 0 } else { count }];
        let result = unsafe {
            host(thread, || c::mbstowcs(if destination.ptr.is_null() { std::ptr::null_mut() } else { out.as_mut_ptr() }, bytes.as_ptr().cast(), count))
        };
        if !destination.ptr.is_null() && result != usize::MAX {
            put(destination, &out[..(result + 1).min(count)]);
        }
        result
    }

    fn wcstombs(thread: *mut Thread, destination: Ptr, source: Ptr, count: usize) -> usize {
        let mut units = wide(source, None);
        units.push(0);
        let mut out = vec![0u8; if destination.ptr.is_null() { 0 } else { count }];
        let result = unsafe {
            host(thread, || libc::wcstombs(if destination.ptr.is_null() { std::ptr::null_mut() } else { out.as_mut_ptr().cast() }, units.as_ptr(), count))
        };
        if !destination.ptr.is_null() && result != usize::MAX {
            view::write(destination, &out[..(result + 1).min(count)]);
        }
        result
    }

    fn mbsinit(thread: *mut Thread, state: Ptr) -> c_int {
        if state.ptr.is_null() {
            return 1;
        }
        let converted = state_in(state);
        unsafe { c::mbsinit(&converted) }
    }

    fn iswalnum(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswalnum(c) } }
    fn iswalpha(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswalpha(c) } }
    fn iswblank(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswblank(c) } }
    fn iswcntrl(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswcntrl(c) } }
    fn iswdigit(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswdigit(c) } }
    fn iswgraph(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswgraph(c) } }
    fn iswlower(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswlower(c) } }
    fn iswprint(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswprint(c) } }
    fn iswpunct(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswpunct(c) } }
    fn iswspace(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswspace(c) } }
    fn iswupper(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswupper(c) } }
    fn iswxdigit(thread: *mut Thread, c: u32) -> c_int { unsafe { c::iswxdigit(c) } }
    fn towlower(thread: *mut Thread, c: u32) -> u32 { unsafe { c::towlower(c) } }
    fn towupper(thread: *mut Thread, c: u32) -> u32 { unsafe { c::towupper(c) } }
    fn wcwidth(thread: *mut Thread, c: Wide) -> c_int { unsafe { c::wcwidth(c) } }

    // A class or translation is the host's, an integer to Fil-C code.
    fn wctype(thread: *mut Thread, class: Ptr) -> u64 {
        let class = name(class);
        unsafe { c::wctype(class.as_ptr()) }
    }

    fn iswctype(thread: *mut Thread, c: u32, class: u64) -> c_int {
        unsafe { c::iswctype(c, class) }
    }

    fn wctrans(thread: *mut Thread, translation: Ptr) -> u64 {
        let translation = name(translation);
        unsafe { c::wctrans(translation.as_ptr()) as u64 }
    }

    fn towctrans(thread: *mut Thread, c: u32, translation: u64) -> u32 {
        if translation == 0 {
            return c;
        }
        // Only what `wctrans` gave out.
        let known = ["tolower", "toupper"].map(|name| unsafe { c::wctrans(std::ffi::CString::new(name).unwrap().as_ptr()) } as u64);
        if !known.contains(&translation) {
            crate::failure::safety_panic(format_args!("towctrans: not a translation wctrans gave out."));
        }
        unsafe { c::towctrans(c, translation as *const i32) }
    }

    fn swprintf(thread: *mut Thread, buffer: Ptr, size: usize, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_wide(thread, buffer, size, format, Arguments::Call(rest)) }
    }

    fn vswprintf(thread: *mut Thread, buffer: Ptr, size: usize, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_wide(thread, buffer, size, format, Arguments::List(list)) }
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    let mut aliases = vec![
        ("pizlonated___mbrlen", mbrlen::getter as *const ()),
        ("pizlonated___mbrtowc", mbrtowc::getter as *const ()),
        ("pizlonated_wcscoll", wcscmp::getter as *const ()),
        ("pizlonated_wcscoll_l", wcscmp::getter as *const ()),
        ("pizlonated___wcscoll_l", wcscmp::getter as *const ()),
        ("pizlonated_wcsxfrm_l", wcsxfrm::getter as *const ()),
        ("pizlonated___wcsxfrm_l", wcsxfrm::getter as *const ()),
        ("pizlonated_wcstol_l", wcstol::getter as *const ()),
        ("pizlonated_wcstoul_l", wcstoul::getter as *const ()),
        ("pizlonated_wcstoll_l", wcstoll::getter as *const ()),
        ("pizlonated_wcstoull_l", wcstoull::getter as *const ()),
        ("pizlonated_wcstoq", wcstoll::getter as *const ()),
        ("pizlonated_wcstouq", wcstoull::getter as *const ()),
        ("pizlonated_wcstoimax", wcstoll::getter as *const ()),
        ("pizlonated_wcstoumax", wcstoull::getter as *const ()),
        ("pizlonated___isoc23_wcstol_l", __isoc23_wcstol::getter as *const ()),
        ("pizlonated___isoc23_wcstoul_l", __isoc23_wcstoul::getter as *const ()),
        ("pizlonated___isoc23_wcstoll_l", __isoc23_wcstoll::getter as *const ()),
        ("pizlonated___isoc23_wcstoull_l", __isoc23_wcstoull::getter as *const ()),
        ("pizlonated___isoc23_wcstoimax", __isoc23_wcstoll::getter as *const ()),
        ("pizlonated___isoc23_wcstoumax", __isoc23_wcstoull::getter as *const ()),
        ("pizlonated_wcstod_l", wcstod::getter as *const ()),
        ("pizlonated_wcstof_l", wcstof::getter as *const ()),
        ("pizlonated_wcstold_l", wcstold::getter as *const ()),
        ("pizlonated_wctype_l", wctype::getter as *const ()),
        ("pizlonated_iswctype_l", iswctype::getter as *const ()),
        ("pizlonated___iswctype", iswctype::getter as *const ()),
        ("pizlonated_wctrans_l", wctrans::getter as *const ()),
        ("pizlonated_towctrans_l", towctrans::getter as *const ()),
    ];
    for (names, getter) in [
        (["iswalnum_l", "__iswalnum_l"], iswalnum::getter as *const ()),
        (["iswalpha_l", "__iswalpha_l"], iswalpha::getter as *const ()),
        (["iswblank_l", "__iswblank_l"], iswblank::getter as *const ()),
        (["iswcntrl_l", "__iswcntrl_l"], iswcntrl::getter as *const ()),
        (["iswdigit_l", "__iswdigit_l"], iswdigit::getter as *const ()),
        (["iswgraph_l", "__iswgraph_l"], iswgraph::getter as *const ()),
        (["iswlower_l", "__iswlower_l"], iswlower::getter as *const ()),
        (["iswprint_l", "__iswprint_l"], iswprint::getter as *const ()),
        (["iswpunct_l", "__iswpunct_l"], iswpunct::getter as *const ()),
        (["iswspace_l", "__iswspace_l"], iswspace::getter as *const ()),
        (["iswupper_l", "__iswupper_l"], iswupper::getter as *const ()),
        (["iswxdigit_l", "__iswxdigit_l"], iswxdigit::getter as *const ()),
        (["towlower_l", "__towlower_l"], towlower::getter as *const ()),
        (["towupper_l", "__towupper_l"], towupper::getter as *const ()),
    ] {
        for name in names {
            aliases.push((Box::leak(format!("pizlonated_{name}").into_boxed_str()), getter));
        }
    }
    aliases
}

/// What the stdio module needs to write a wide character to a stream.
pub(crate) fn encode(c: Wide) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 16];
    let mut state: libc::mbstate_t = unsafe { std::mem::zeroed() };
    let length = unsafe { c::wcrtomb(buffer.as_mut_ptr().cast(), c, &mut state) };
    (length != usize::MAX).then(|| buffer[..length].to_vec())
}

/// And to read one, from a byte.
pub(crate) fn decode(byte: u8) -> Option<Wide> {
    let unit = unsafe { c::btowc(byte as c_int) };
    (unit != u32::MAX).then_some(unit as Wide)
}
