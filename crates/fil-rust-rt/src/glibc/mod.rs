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

mod ctype;
mod env;
mod error;
mod stdlib;
mod string;
mod time;
mod unistd;

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
    /// Where `strtok` carries on from, or null.
    pub(crate) strtok: Ptr,
}

impl Default for ThreadState {
    fn default() -> Self {
        ThreadState { errno: None, dlerror: Ptr::NULL, strtok: Ptr::NULL }
    }
}

impl ThreadState {
    /// Calls `visit` on every object here.
    pub fn for_each_object(&self, mut visit: impl FnMut(*mut Object)) {
        if let Some(errno) = self.errno {
            visit(errno.as_ptr());
        }
        visit(self.dlerror.object());
        visit(self.strtok.object());
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

/// This thread's `errno`.
pub(crate) unsafe fn errno_location(me: *mut Thread) -> *mut c_int {
    unsafe { Ptr::for_object(errno_object(me)).ptr.cast() }
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

/// A read-only copy of `bytes` and a terminator that lives for good, the same
/// one for the same bytes: for strings a libc hands out from static storage,
/// like `strerror`'s and time zone names.
pub(crate) unsafe fn interned(me: *mut Thread, bytes: &[u8]) -> Ptr {
    type Strings = Option<std::collections::HashMap<Vec<u8>, (usize, usize)>>;
    static STRINGS: std::sync::Mutex<Strings> = std::sync::Mutex::new(None);
    let found = |strings: &mut Strings| {
        strings.get_or_insert_with(Default::default).get(bytes).map(|&(ptr, lower)| Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 })
    };
    if let Some(ptr) = found(&mut *unsafe { thread::lock_entered(me, &STRINGS) }) {
        return ptr;
    }
    let copy = unsafe {
        let object = object::allocate(me, bytes.len() + 1, 0);
        crate::globals::keep(object);
        let copy = Ptr::for_object(object);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy.ptr, bytes.len());
        object::make_readonly(object);
        copy
    };
    let mut strings = unsafe { thread::lock_entered(me, &STRINGS) };
    if let Some(ptr) = found(&mut strings) {
        return ptr;
    }
    strings.as_mut().unwrap().insert(bytes.to_vec(), (copy.ptr as usize, copy.lower as usize));
    copy
}

/// Calls `visit` on every object the libc's global state holds.
pub fn for_each_root(visit: impl FnMut(*mut Object)) {
    env::for_each_root(visit);
}

/// Stub slots for every native here, keyed by symbol.
pub fn table() -> Vec<(&'static str, *const ())> {
    let mut table = errno::table();
    table.extend(string::table());
    table.extend(string::aliases());
    table.extend(ctype::table());
    table.extend(ctype::aliases());
    table.extend(stdlib::table());
    table.extend(stdlib::aliases());
    table.extend(unistd::aliases());
    table.extend(env::table());
    table.extend(env::aliases());
    table.extend(error::table());
    table.extend(time::table());
    table.extend(time::aliases());
    table
}

mod errno {
    use super::*;

crate::natives::natives! {
    fn __errno_location(thread: *mut Thread) -> Ptr {
        Ptr::for_object(unsafe { errno_object(thread) })
    }

    fn dlerror(thread: *mut Thread) -> Ptr {
        std::mem::replace(&mut unsafe { thread::extra(thread) }.libc.dlerror, Ptr::NULL)
    }
}
}
