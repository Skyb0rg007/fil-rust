//! `<time.h>` over the host's, so the time zone is the process's. A `struct
//! tm` crosses as a copy: its `tm_zone` is a capability on the Fil-C side and
//! a plain pointer on the host's, and the host's names come back interned.

use std::ffi::{CString, c_char, c_int, c_long};
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::natives;
use crate::object::Access;
use crate::start::set_errno;
use crate::sys::{one_out, plain};
use crate::thread::{self, NativeFrame};
use crate::view::{self, Bytes};
use crate::{globals, object, ptrs};

use super::interned;

/// glibc's `struct tm`: nine `int`s, `tm_gmtoff` at 40 and `tm_zone` at 48.
const TM_SIZE: usize = 56;
const GMTOFF: usize = 40;
const ZONE: usize = 48;

/// A host `tm` made from the program's, with the zone name it points to.
struct HostTm {
    tm: libc::tm,
    _zone: Option<CString>,
}

fn read_tm(user: Ptr) -> HostTm {
    let bytes = Bytes::exactly(user, TM_SIZE, Access::Read);
    let word = |offset: usize, count: usize| -> u64 {
        (0..count).fold(0, |value, index| value | (bytes.get(offset + index) as u64) << (8 * index))
    };
    let int = |index: usize| word(index * 4, 4) as u32 as c_int;
    let zone_ptr = unsafe { ptrs::load(bytes.at(ZONE)) };
    let zone = (!zone_ptr.ptr.is_null()).then(|| unsafe { CString::from_vec_unchecked(view::c_string(zone_ptr)) });
    let tm = libc::tm {
        tm_sec: int(0),
        tm_min: int(1),
        tm_hour: int(2),
        tm_mday: int(3),
        tm_mon: int(4),
        tm_year: int(5),
        tm_wday: int(6),
        tm_yday: int(7),
        tm_isdst: int(8),
        tm_gmtoff: word(GMTOFF, 8) as c_long,
        tm_zone: zone.as_ref().map_or(std::ptr::null(), |zone| zone.as_ptr()),
    };
    HostTm { tm, _zone: zone }
}

unsafe fn write_tm(me: *mut Thread, user: Ptr, tm: &libc::tm) {
    let ints = [tm.tm_sec, tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon, tm.tm_year, tm.tm_wday, tm.tm_yday, tm.tm_isdst];
    let mut bytes: Vec<u8> = ints.iter().flat_map(|int| int.to_le_bytes()).collect();
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&tm.tm_gmtoff.to_le_bytes());
    view::write(user, &bytes);
    let zone = if tm.tm_zone.is_null() {
        Ptr::NULL
    } else {
        unsafe { interned(me, std::ffi::CStr::from_ptr(tm.tm_zone).to_bytes()) }
    };
    unsafe { ptrs::store(me, user.with_ptr(user.ptr.add(ZONE)), zone) };
}

fn read_time(ptr: Ptr) -> libc::time_t {
    let bytes = Bytes::exactly(ptr, 8, Access::Read);
    i64::from_le_bytes(std::array::from_fn(|index| bytes.get(index)))
}

/// Breaks `time` down with `convert` into `result`, which must have room.
unsafe fn broken_down(
    me: *mut Thread,
    time: Ptr,
    result: Ptr,
    convert: unsafe extern "C" fn(*const libc::time_t, *mut libc::tm) -> *mut libc::tm,
) -> Ptr {
    let time = read_time(time);
    object::check_access(result, TM_SIZE, Access::Write);
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let error = unsafe {
        plain(me, || {
            *libc::__errno_location() = 0;
            if convert(&time, &mut tm).is_null() { *libc::__errno_location() } else { 0 }
        })
    };
    if error != 0 {
        unsafe { set_errno(me, error) };
        return Ptr::NULL;
    }
    unsafe { write_tm(me, result, &tm) };
    result
}

/// A static buffer, like glibc's for `localtime` and `ctime`: one for the
/// process, made once, living for good.
unsafe fn static_buffer(me: *mut Thread, cell: &'static Mutex<Option<(usize, usize)>>, size: usize) -> Ptr {
    let mut made = unsafe { thread::lock_entered(me, cell) };
    let (ptr, lower) = *made.get_or_insert_with(|| unsafe {
        let object = object::allocate(me, size, 0);
        globals::keep(object);
        let ptr = Ptr::for_object(object);
        (ptr.ptr as usize, ptr.lower as usize)
    });
    Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 }
}

static STATIC_TM: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static STATIC_TEXT: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// What `asctime_r` writes, at most 26 bytes with the terminator.
unsafe fn asctime_into(me: *mut Thread, tm: Ptr, buffer: Ptr) -> Ptr {
    let host = read_tm(tm);
    let mut text = [0 as c_char; 64];
    if unsafe { libc::asctime_r(&host.tm, text.as_mut_ptr()) }.is_null() {
        unsafe { set_errno(me, libc::EOVERFLOW) };
        return Ptr::NULL;
    }
    let text = unsafe { std::ffi::CStr::from_ptr(text.as_ptr()) }.to_bytes_with_nul();
    view::write(buffer, text);
    buffer
}

natives! {
    fn time(thread: *mut Thread, place: Ptr) -> i64 {
        let now = unsafe { plain(thread, || libc::time(std::ptr::null_mut())) };
        if !place.ptr.is_null() {
            unsafe { one_out::<libc::time_t>(place).write_unaligned(now) };
        }
        now
    }

    fn clock(thread: *mut Thread) -> c_long {
        unsafe extern "C" {
            fn clock() -> c_long;
        }
        unsafe { plain(thread, || clock()) }
    }

    fn difftime(thread: *mut Thread, end: i64, start: i64) -> f64 {
        end as f64 - start as f64
    }

    fn tzset(thread: *mut Thread) -> () {
        unsafe extern "C" {
            fn tzset();
        }
        unsafe { plain(thread, || tzset()) };
    }

    fn localtime_r(thread: *mut Thread, time: Ptr, result: Ptr) -> Ptr {
        unsafe { broken_down(thread, time, result, libc::localtime_r) }
    }

    fn gmtime_r(thread: *mut Thread, time: Ptr, result: Ptr) -> Ptr {
        unsafe { broken_down(thread, time, result, libc::gmtime_r) }
    }

    fn localtime(thread: *mut Thread, time: Ptr) -> Ptr {
        let result = unsafe { static_buffer(thread, &STATIC_TM, TM_SIZE) };
        unsafe { broken_down(thread, time, result, libc::localtime_r) }
    }

    fn gmtime(thread: *mut Thread, time: Ptr) -> Ptr {
        let result = unsafe { static_buffer(thread, &STATIC_TM, TM_SIZE) };
        unsafe { broken_down(thread, time, result, libc::gmtime_r) }
    }

    fn mktime(thread: *mut Thread, tm: Ptr) -> i64 {
        let mut host = read_tm(tm);
        object::check_access(tm, TM_SIZE, Access::Write);
        let (result, error) = unsafe {
            plain(thread, || {
                *libc::__errno_location() = 0;
                (libc::mktime(&mut host.tm), *libc::__errno_location())
            })
        };
        if result == -1 && error != 0 {
            unsafe { set_errno(thread, error) };
        } else {
            unsafe { write_tm(thread, tm, &host.tm) };
        }
        result
    }

    fn timegm(thread: *mut Thread, tm: Ptr) -> i64 {
        let mut host = read_tm(tm);
        object::check_access(tm, TM_SIZE, Access::Write);
        let result = unsafe { libc::timegm(&mut host.tm) };
        unsafe { write_tm(thread, tm, &host.tm) };
        result
    }

    fn strftime(thread: *mut Thread, buffer: Ptr, size: usize, format: Ptr, tm: Ptr) -> usize {
        let format = unsafe { CString::from_vec_unchecked(view::c_string(format)) };
        let host = read_tm(tm);
        let mut text = vec![0u8; size];
        let written = unsafe { plain(thread, || libc::strftime(text.as_mut_ptr().cast(), size, format.as_ptr(), &host.tm)) };
        if written == 0 {
            return 0;
        }
        view::write(buffer, &text[..written + 1]);
        written
    }

    fn asctime_r(thread: *mut Thread, tm: Ptr, buffer: Ptr) -> Ptr {
        unsafe { asctime_into(thread, tm, buffer) }
    }

    fn asctime(thread: *mut Thread, tm: Ptr) -> Ptr {
        let buffer = unsafe { static_buffer(thread, &STATIC_TEXT, 26) };
        unsafe { asctime_into(thread, tm, buffer) }
    }

    fn ctime_r(thread: *mut Thread, time: Ptr, buffer: Ptr) -> Ptr {
        let tm = unsafe { object::allocate(thread, TM_SIZE, 0) };
        unsafe { thread::track(thread, tm) };
        if unsafe { broken_down(thread, time, Ptr::for_object(tm), libc::localtime_r) }.ptr.is_null() {
            return Ptr::NULL;
        }
        unsafe { asctime_into(thread, Ptr::for_object(tm), buffer) }
    }

    fn ctime(thread: *mut Thread, time: Ptr) -> Ptr {
        let tm = unsafe { object::allocate(thread, TM_SIZE, 0) };
        unsafe { thread::track(thread, tm) };
        if unsafe { broken_down(thread, time, Ptr::for_object(tm), libc::localtime_r) }.ptr.is_null() {
            return Ptr::NULL;
        }
        let buffer = unsafe { static_buffer(thread, &STATIC_TEXT, 26) };
        unsafe { asctime_into(thread, Ptr::for_object(tm), buffer) }
    }
}

unsafe extern "C" {
    #[link_name = "timezone"]
    static HOST_TIMEZONE: c_long;
    #[link_name = "daylight"]
    static HOST_DAYLIGHT: c_int;
    #[link_name = "tzname"]
    static HOST_TZNAME: [*const c_char; 2];
}

static TIMEZONE: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static DAYLIGHT: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static TZNAME: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// The storage behind `timezone`, holding the host's value as it is now.
unsafe extern "C" fn timezone(me: *mut Thread, _: *const Origin) -> Ptr {
    let storage = unsafe { static_buffer(me, &TIMEZONE, 8) };
    view::write(storage, &unsafe { HOST_TIMEZONE }.to_le_bytes());
    storage
}

unsafe extern "C" fn daylight(me: *mut Thread, _: *const Origin) -> Ptr {
    let storage = unsafe { static_buffer(me, &DAYLIGHT, 4) };
    view::write(storage, &unsafe { HOST_DAYLIGHT }.to_le_bytes());
    storage
}

unsafe extern "C" fn tzname(me: *mut Thread, _: *const Origin) -> Ptr {
    unsafe {
        let frame = NativeFrame::push(me);
        let storage = static_buffer(me, &TZNAME, 2 * WORD_SIZE);
        for (index, &name) in (&raw const HOST_TZNAME).read().iter().enumerate() {
            let name = if name.is_null() { Ptr::NULL } else { interned(me, std::ffi::CStr::from_ptr(name).to_bytes()) };
            ptrs::store(me, storage.with_ptr(storage.ptr.add(index * WORD_SIZE)), name);
        }
        frame.pop(me);
        storage
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated_timezone", timezone as *const ()),
        ("pizlonated___timezone", timezone as *const ()),
        ("pizlonated_daylight", daylight as *const ()),
        ("pizlonated___daylight", daylight as *const ()),
        ("pizlonated_tzname", tzname as *const ()),
        ("pizlonated___tzname", tzname as *const ()),
        ("pizlonated___tzset", tzset::getter as *const ()),
        ("pizlonated_strftime_l", strftime::getter as *const ()),
        ("pizlonated___strftime_l", strftime::getter as *const ()),
    ]
}
