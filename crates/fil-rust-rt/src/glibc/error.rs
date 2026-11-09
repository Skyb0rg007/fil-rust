//! Error and signal descriptions: the host's text, interned, so the strings
//! handed out are read-only and live for good, as glibc's static ones do.

use std::ffi::{CStr, c_int};
use std::io::Write;

use crate::natives::natives;
use crate::view;

use super::interned;

fn error_text(number: c_int) -> Vec<u8> {
    let mut buffer = [0u8; 256];
    // The GNU one, which the libc crate's `strerror_r` is on glibc.
    unsafe extern "C" {
        fn __xpg_strerror_r(number: c_int, buffer: *mut u8, size: usize) -> c_int;
    }
    if unsafe { __xpg_strerror_r(number, buffer.as_mut_ptr(), buffer.len()) } != 0 {
        return format!("Unknown error {number}").into_bytes();
    }
    CStr::from_bytes_until_nul(&buffer).unwrap().to_bytes().to_vec()
}

fn known(number: c_int) -> bool {
    !error_text(number).starts_with(b"Unknown error")
}

natives! {
    fn strerror(thread: *mut Thread, number: c_int) -> Ptr {
        unsafe { interned(thread, &error_text(number)) }
    }

    fn strerror_l(thread: *mut Thread, number: c_int, locale: Ptr) -> Ptr {
        unsafe { interned(thread, &error_text(number)) }
    }

    // GNU's: the text may be static, and the buffer goes unused.
    fn strerror_r(thread: *mut Thread, number: c_int, buffer: Ptr, size: usize) -> Ptr {
        unsafe { interned(thread, &error_text(number)) }
    }

    // POSIX's.
    fn __xpg_strerror_r(thread: *mut Thread, number: c_int, buffer: Ptr, size: usize) -> c_int {
        let mut text = error_text(number);
        let status = if known(number) { 0 } else { libc::EINVAL };
        if size == 0 {
            return libc::ERANGE;
        }
        let fits = text.len() < size;
        text.truncate(size - 1);
        text.push(0);
        view::write(buffer, &text);
        if fits { status } else { libc::ERANGE }
    }

    fn strsignal(thread: *mut Thread, signal: c_int) -> Ptr {
        let text = unsafe { libc::strsignal(signal) };
        let text = if text.is_null() { format!("Unknown signal {signal}").into_bytes() } else { unsafe { CStr::from_ptr(text) }.to_bytes().to_vec() };
        unsafe { interned(thread, &text) }
    }

    fn sigdescr_np(thread: *mut Thread, signal: c_int) -> Ptr {
        unsafe extern "C" {
            fn sigdescr_np(signal: c_int) -> *const std::ffi::c_char;
        }
        let text = unsafe { sigdescr_np(signal) };
        if text.is_null() {
            return Ptr::NULL;
        }
        unsafe { interned(thread, CStr::from_ptr(text).to_bytes()) }
    }

    fn strerrordesc_np(thread: *mut Thread, number: c_int) -> Ptr {
        if !known(number) {
            return Ptr::NULL;
        }
        unsafe { interned(thread, &error_text(number)) }
    }

    // Straight to the process's standard error, as Rust's `eprintln!` goes.
    fn perror(thread: *mut Thread, prefix: Ptr) -> () {
        let number = unsafe { *crate::glibc::errno_location(thread) };
        let mut line = Vec::new();
        if let Some(prefix) = (!prefix.ptr.is_null()).then(|| view::c_string(prefix)).filter(|prefix| !prefix.is_empty()) {
            line.extend_from_slice(&prefix);
            line.extend_from_slice(b": ");
        }
        line.extend_from_slice(&error_text(number));
        line.push(b'\n');
        unsafe { crate::sys::plain(thread, || drop(std::io::stderr().write_all(&line))) };
    }
}
