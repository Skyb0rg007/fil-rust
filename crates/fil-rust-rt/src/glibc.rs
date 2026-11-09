//! The glibc flavor's libc, as natives over the host libc and Rust's std.
//!
//! A compiled library imports libc functions and variables as `pizlonated_`
//! getters from `libc.so.6666` and its siblings. `fil-rust-sys` builds
//! stand-ins under those sonames with a slot for every export, and the slots
//! that have a native here get it. The rest fail by name: a function when it's
//! called, a variable when its getter runs.

use std::ffi::c_int;

use crate::abi::*;
use crate::object;
use crate::thread;

/// What the libc keeps per thread. It lives in the thread's `ThreadExtra`,
/// which the collector marks, never in a `thread_local!`: registering a
/// thread-local's destructor takes the loader lock, which a constructor may
/// hold while it waits for a collection.
pub struct ThreadState {
    /// The thread's `errno`, made the first time it's needed. It never moves:
    /// `__errno_location` is `const`, so compiled code may keep its address.
    errno: Option<std::ptr::NonNull<Object>>,
    /// What `dlerror` returns next, or null.
    dlerror: Ptr,
}

impl Default for ThreadState {
    fn default() -> Self {
        ThreadState { errno: None, dlerror: Ptr::NULL }
    }
}

impl ThreadState {
    /// Calls `visit` on every object here.
    pub fn for_each_object(&self, mut visit: impl FnMut(*mut Object)) {
        if let Some(errno) = self.errno {
            visit(errno.as_ptr());
        }
        visit(self.dlerror.object());
    }
}

unsafe fn errno_object(me: *mut Thread) -> *mut Object {
    if let Some(errno) = unsafe { thread::extra(me) }.libc.errno {
        return errno.as_ptr();
    }
    let object = unsafe { object::allocate(me, size_of::<c_int>(), 0) };
    unsafe { thread::extra(me) }.libc.errno = std::ptr::NonNull::new(object);
    object
}

/// Sets this thread's `errno`.
pub unsafe fn set_errno(me: *mut Thread, value: c_int) {
    unsafe { Ptr::for_object(errno_object(me)).ptr.cast::<c_int>().write(value) };
}

/// Hands `dlerror` its next message.
pub unsafe fn set_dlerror(me: *mut Thread, message: &[u8]) {
    let copy = unsafe { crate::start::c_string(me, message) };
    unsafe { thread::extra(me) }.libc.dlerror = copy;
}

crate::natives::natives! {
    fn __errno_location(thread: *mut Thread) -> Ptr {
        Ptr::for_object(unsafe { errno_object(thread) })
    }

    fn dlerror(thread: *mut Thread) -> Ptr {
        std::mem::replace(&mut unsafe { thread::extra(thread) }.libc.dlerror, Ptr::NULL)
    }
}
