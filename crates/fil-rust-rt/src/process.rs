//! What starts processes.

use std::ffi::{CString, c_char, c_int, c_uint};
use std::ptr::null;

use crate::abi::*;
use crate::gc;
use crate::natives::natives;
use crate::object::Access;
use crate::ptrs;
use crate::start::set_errno;
use crate::sys::{iovecs_of, plain, sys, text};
use crate::thread;

/// A null-terminated array of strings, copied so that no other thread can
/// change it under the kernel.
struct Strings {
    _strings: Vec<CString>,
    pointers: Vec<*const c_char>,
}

unsafe fn strings(thread: *mut Thread, user: Ptr) -> Strings {
    let mut entry = user;
    let mut strings = Vec::new();
    loop {
        let string = unsafe { ptrs::load(entry) };
        if string.ptr.is_null() {
            break;
        }
        unsafe {
            thread::track(thread, string.object());
            strings.push(text(string));
        }
        entry = entry.with_ptr(entry.ptr.wrapping_add(size_of::<Ptr>() / 2));
    }
    let mut pointers: Vec<*const c_char> = strings.iter().map(|string| string.as_ptr()).collect();
    pointers.push(null());
    Strings { _strings: strings, pointers }
}

unsafe fn strings_or_null(thread: *mut Thread, user: Ptr) -> Option<Strings> {
    (!user.ptr.is_null()).then(|| unsafe { strings(thread, user) })
}

natives! {
    fn zsys_fork_impl(thread: *mut Thread) -> c_int {
        let (result, error) = unsafe { gc::fork(thread) };
        if result < 0 {
            unsafe { set_errno(thread, error) };
        }
        result
    }

    fn zsys_execve(thread: *mut Thread, path: Ptr, arguments: Ptr, environment: Ptr) -> c_int {
        let path = unsafe { text(path) };
        let arguments = unsafe { strings(thread, arguments) };
        let environment = unsafe { strings_or_null(thread, environment) };
        let environment = environment.as_ref().map_or(null(), |environment| environment.pointers.as_ptr());
        unsafe { sys(thread, || libc::execve(path.as_ptr(), arguments.pointers.as_ptr(), environment)) };
        -1
    }

    fn zsys_fexecve(thread: *mut Thread, fd: c_int, arguments: Ptr, environment: Ptr) -> c_int {
        let arguments = unsafe { strings(thread, arguments) };
        let environment = unsafe { strings(thread, environment) };
        unsafe { sys(thread, || libc::fexecve(fd, arguments.pointers.as_ptr(), environment.pointers.as_ptr())) }
    }

    fn zsys_vmsplice(thread: *mut Thread, fd: c_int, user: Ptr, count: usize, flags: c_uint) -> i64 {
        // The kernel goes by the descriptor: it reads the memory if it's a
        // pipe's write end and writes it if it's the read end. On anything else
        // it touches nothing.
        let access = unsafe {
            plain(thread, || {
                let mut status: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut status) != 0 || status.st_mode & libc::S_IFMT != libc::S_IFIFO {
                    return None;
                }
                let read_end = libc::fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE == libc::O_RDONLY;
                Some(if read_end { Access::Write } else { Access::Read })
            })
        };
        let list = unsafe { iovecs_of(thread, user, count, access) };
        unsafe { sys(thread, || libc::vmsplice(fd, list.as_ptr().cast::<libc::iovec>(), count, flags) as i64) }
    }
}

