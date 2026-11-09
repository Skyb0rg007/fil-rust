//! Signals: sets, `signal` over the runtime's `sigaction`, and the jumps out
//! of handlers. `setjmp` itself is compiled in; only `longjmp` is a call.

use std::ffi::c_int;
use std::io::Write;

use crate::abi::*;
use crate::natives::natives;
use crate::object::{self, Access};
use crate::glibc::set_errno;
use crate::view::{self, Bytes};
use crate::{ptrs, thread};

const SIGSET_SIZE: usize = size_of::<libc::sigset_t>();

/// What `sigaction`'s `sa_handler` gives back for an error.
const SIG_ERR: Ptr = Ptr { ptr: usize::MAX as *mut u8, lower: std::ptr::null_mut() };

fn read_set(set: Ptr) -> libc::sigset_t {
    let bytes = Bytes::exactly(set, SIGSET_SIZE, Access::Read);
    let mut host = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let raw = (&raw mut host).cast::<u8>();
    for index in 0..SIGSET_SIZE {
        unsafe { raw.add(index).write(bytes.get(index)) };
    }
    host
}

fn write_set(set: Ptr, host: &libc::sigset_t) {
    view::write(set, unsafe { std::slice::from_raw_parts((&raw const *host).cast::<u8>(), SIGSET_SIZE) });
}

/// Runs a host set function on a copy of the program's set, and copies the
/// result back.
fn on_set(set: Ptr, change: impl FnOnce(&mut libc::sigset_t) -> c_int) -> c_int {
    object::check_access(set, SIGSET_SIZE, Access::Write);
    let mut host = read_set(set);
    let result = change(&mut host);
    write_set(set, &host);
    result
}

/// A set call's result, with `errno` for a -1.
fn set_result(thread: *mut Thread, result: c_int) -> c_int {
    if result < 0 {
        unsafe { set_errno(thread, libc::EINVAL) };
    }
    result
}

/// Installs `handler` for `signal` with `flags`, giving back the old one.
unsafe fn install(me: *mut Thread, signal: c_int, handler: Ptr, flags: c_int) -> Ptr {
    unsafe {
        let size = size_of::<libc::sigaction>();
        let act = object::allocate(me, size, 0);
        thread::track(me, act);
        let old = object::allocate(me, size, 0);
        thread::track(me, old);
        let (act, old) = (Ptr::for_object(act), Ptr::for_object(old));
        let action = act.ptr.cast::<libc::sigaction>();
        ptrs::store(me, act.with_ptr((&raw mut (*action).sa_sigaction).cast()), handler);
        (*action).sa_flags = flags;
        if crate::signal::sigaction(me, signal, act, old) != 0 {
            return SIG_ERR;
        }
        ptrs::load(old.with_ptr((&raw mut (*old.ptr.cast::<libc::sigaction>()).sa_sigaction).cast()))
    }
}

natives! {
    // The compiled `setjmp` keeps a capability for the runtime's buffer at the
    // start of glibc's `jmp_buf`.
    fn longjmp(thread: *mut Thread, buffer: Ptr, value: c_int) -> () {
        let buffer = unsafe { ptrs::load(buffer) };
        unsafe { crate::jump::longjmp(thread, buffer, value) }
    }

    fn sigemptyset(thread: *mut Thread, set: Ptr) -> c_int {
        on_set(set, |set| unsafe { libc::sigemptyset(set) })
    }

    fn sigfillset(thread: *mut Thread, set: Ptr) -> c_int {
        on_set(set, |set| unsafe { libc::sigfillset(set) })
    }

    fn sigaddset(thread: *mut Thread, set: Ptr, signal: c_int) -> c_int {
        let result = on_set(set, |set| unsafe { libc::sigaddset(set, signal) });
        set_result(thread, result)
    }

    fn sigdelset(thread: *mut Thread, set: Ptr, signal: c_int) -> c_int {
        let result = on_set(set, |set| unsafe { libc::sigdelset(set, signal) });
        set_result(thread, result)
    }

    fn sigismember(thread: *mut Thread, set: Ptr, signal: c_int) -> c_int {
        let set = read_set(set);
        set_result(thread, unsafe { libc::sigismember(&set, signal) })
    }

    fn sigisemptyset(thread: *mut Thread, set: Ptr) -> c_int {
        let set = read_set(set);
        unsafe extern "C" {
            fn sigisemptyset(set: *const libc::sigset_t) -> c_int;
        }
        unsafe { sigisemptyset(&set) }
    }

    fn sigandset(thread: *mut Thread, set: Ptr, left: Ptr, right: Ptr) -> c_int {
        let (left, right) = (read_set(left), read_set(right));
        unsafe extern "C" {
            fn sigandset(set: *mut libc::sigset_t, left: *const libc::sigset_t, right: *const libc::sigset_t) -> c_int;
        }
        on_set(set, |set| unsafe { sigandset(set, &left, &right) })
    }

    fn sigorset(thread: *mut Thread, set: Ptr, left: Ptr, right: Ptr) -> c_int {
        let (left, right) = (read_set(left), read_set(right));
        unsafe extern "C" {
            fn sigorset(set: *mut libc::sigset_t, left: *const libc::sigset_t, right: *const libc::sigset_t) -> c_int;
        }
        on_set(set, |set| unsafe { sigorset(set, &left, &right) })
    }

    // BSD's, which glibc's `signal` is: handlers stay, and calls restart.
    fn signal(thread: *mut Thread, signal: c_int, handler: Ptr) -> Ptr {
        unsafe { install(thread, signal, handler, libc::SA_RESTART) }
    }

    fn sysv_signal(thread: *mut Thread, signal: c_int, handler: Ptr) -> Ptr {
        unsafe { install(thread, signal, handler, libc::SA_RESETHAND | libc::SA_NODEFER) }
    }

    // Above the runtime's wake-up signal, which glibc gives as SIGRTMIN.
    fn __libc_current_sigrtmin(thread: *mut Thread) -> c_int {
        crate::signal::WAKE + 1
    }

    fn __libc_current_sigrtmax(thread: *mut Thread) -> c_int {
        libc::SIGRTMAX()
    }

    fn psignal(thread: *mut Thread, signal: c_int, prefix: Ptr) -> () {
        let text = unsafe { libc::strsignal(signal) };
        let mut line = Vec::new();
        if let Some(prefix) = (!prefix.ptr.is_null()).then(|| view::c_string(prefix)).filter(|prefix| !prefix.is_empty()) {
            line.extend_from_slice(&prefix);
            line.extend_from_slice(b": ");
        }
        if !text.is_null() {
            line.extend_from_slice(unsafe { std::ffi::CStr::from_ptr(text) }.to_bytes());
        }
        line.push(b'\n');
        unsafe { crate::sys::plain(thread, || drop(std::io::stderr().write_all(&line))) };
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    let zlongjmp = longjmp::getter as *const ();
    let mut aliases = vec![
        ("pizlonated___sigaddset", sigaddset::getter as *const ()),
        ("pizlonated___sigdelset", sigdelset::getter as *const ()),
        ("pizlonated___sigismember", sigismember::getter as *const ()),
        ("pizlonated_bsd_signal", signal::getter as *const ()),
        ("pizlonated___sysv_signal", sysv_signal::getter as *const ()),
        ("pizlonated_ssignal", signal::getter as *const ()),
    ];
    for name in ["pizlonated__longjmp", "pizlonated_siglongjmp", "pizlonated___longjmp_chk"] {
        aliases.push((name, zlongjmp));
    }
    aliases
}
