//! Starting the Fil-C libc, and what it leaves with the runtime.
//!
//! `__libc_start_main` never returns, and only a thread it or `pthread_create`
//! set up can call into the libc. So the program it runs is the thread pool:
//! see `fr_main_getter`.

use std::ffi::{c_char, c_int};
use std::ptr::null_mut;
use std::sync::Mutex;

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::object;
use crate::thread::{self, NativeFrame};
use crate::ptrs;

/// What the process was started with, as Fil-C sees it.
#[derive(Clone, Copy)]
pub struct UserEnvironment {
    pub argc: usize,
    pub argv: Ptr,
    pub environ: Ptr,
    pub auxv: Ptr,
}

struct Registered {
    environment: Option<UserEnvironment>,
    errno_handler: Ptr,
    dlerror_handler: Ptr,
}

static REGISTERED: Mutex<Registered> =
    Mutex::new(Registered { environment: None, errno_handler: Ptr::NULL, dlerror_handler: Ptr::NULL });

fn registered() -> std::sync::MutexGuard<'static, Registered> {
    REGISTERED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps this from changing, for `fork`.
pub fn hold() -> impl Sized {
    registered()
}

/// The arguments constructors get, once there are any.
pub fn user_environment() -> Option<UserEnvironment> {
    registered().environment
}

pub fn register_errno_handler(handler: Ptr) {
    let mut registered = registered();
    if !registered.errno_handler.lower.is_null() {
        safety_panic(format_args!("errno handler already registered."));
    }
    registered.errno_handler = handler;
}

pub fn register_dlerror_handler(handler: Ptr) {
    let mut registered = registered();
    if !registered.dlerror_handler.lower.is_null() {
        safety_panic(format_args!("dlerror handler already registered."));
    }
    registered.dlerror_handler = handler;
}

/// Sets the libc's errno. This runs compiled code, so it's a safepoint.
pub unsafe fn set_errno(thread: *mut Thread, value: c_int) {
    let handler = registered().errno_handler;
    if handler.lower.is_null() {
        safety_panic(format_args!("errno handler not registered when trying to set errno = {value}."));
    }
    unsafe { crate::call(thread, handler, &[Ptr { ptr: value as usize as *mut u8, lower: null_mut() }], &mut []) };
}

/// Hands the libc an error for `dlerror`.
pub unsafe fn set_dlerror(thread: *mut Thread, message: &[u8]) {
    let handler = registered().dlerror_handler;
    if handler.lower.is_null() {
        safety_panic(format_args!("dlerror handler not registered when trying to set dlerror."));
    }
    unsafe {
        let copy = c_string(thread, message);
        crate::call(thread, handler, &[copy], &mut []);
    }
}

/// Calls `visit` on every object the libc left with the runtime.
pub unsafe fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let registered = registered();
    for ptr in [registered.errno_handler, registered.dlerror_handler] {
        visit(ptr.object());
    }
    if let Some(user) = registered.environment {
        for ptr in [user.argv, user.environ, user.auxv] {
            visit(ptr.object());
        }
    }
}

/// A tracked copy of `bytes`, with a terminator.
pub(crate) unsafe fn c_string(thread: *mut Thread, bytes: &[u8]) -> Ptr {
    unsafe {
        let object = object::allocate(thread, bytes.len() + 1, 0);
        thread::track(thread, object);
        let copy = Ptr::for_object(object);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy.ptr, bytes.len());
        copy
    }
}

/// A tracked, null-terminated array of copies of `strings`.
unsafe fn string_vector(thread: *mut Thread, strings: &[&[u8]]) -> Ptr {
    unsafe {
        let object = object::allocate(thread, (strings.len() + 1) * WORD_SIZE, 0);
        thread::track(thread, object);
        let vector = Ptr::for_object(object);
        for (index, string) in strings.iter().enumerate() {
            let copy = c_string(thread, string);
            ptrs::store(thread, vector.with_ptr(vector.ptr.add(index * WORD_SIZE)), copy);
        }
        vector
    }
}

/// `AT_MINSIGSTKSZ`, the highest key this has heard of.
const MAX_AUX_KEY: u64 = 51;

/// Copies what's in the kernel's auxiliary vector. The Fil-C libc wants one, and
/// its pointers are just numbers to it.
unsafe fn auxiliary_vector(thread: *mut Thread) -> Ptr {
    let mut entries = Vec::new();
    for key in 0..=MAX_AUX_KEY {
        unsafe {
            *libc::__errno_location() = 0;
            let value = libc::getauxval(key);
            if *libc::__errno_location() == 0 {
                entries.push(key as usize);
                entries.push(value as usize);
            }
        }
    }
    entries.push(0);
    unsafe {
        let object = object::allocate(thread, entries.len() * WORD_SIZE, 0);
        thread::track(thread, object);
        let vector = Ptr::for_object(object);
        std::ptr::copy_nonoverlapping(entries.as_ptr(), vector.ptr.cast::<usize>(), entries.len());
        vector
    }
}

/// Makes this thread the Fil-C program's first, and hands it to the libc.
/// Doesn't return: the thread becomes `main`.
pub unsafe fn start_program(arguments: &[&[u8]], libc_start_main: GetterFn, main: GetterFn) -> ! {
    let thread = thread::attach_current();
    crate::signal::set_main(thread);
    unsafe {
        thread::enter(thread);
        let UserEnvironment { argv, environ, auxv, .. } = {
            let _frame = NativeFrame::push(thread);
            let argv = string_vector(thread, arguments);
            let mut environment = Vec::new();
            let mut entry = environ_pointer();
            while !(*entry).is_null() {
                environment.push(std::ffi::CStr::from_ptr(*entry).to_bytes());
                entry = entry.add(1);
            }
            let environ = string_vector(thread, &environment);
            let auxv = auxiliary_vector(thread);
            let user = UserEnvironment { argc: arguments.len(), argv, environ, auxv };
            registered().environment = Some(user);
            user
        };

        let main = main(thread, std::ptr::null());
        let libc_start_main = libc_start_main(thread, std::ptr::null());
        let argc = Ptr { ptr: arguments.len() as *mut u8, lower: null_mut() };
        crate::call(thread, libc_start_main, &[main, argc, argv, environ, auxv], &mut []);
    }
    internal_panic(format_args!("unexpected return from __libc_start_main."));
}

unsafe fn environ_pointer() -> *const *const c_char {
    unsafe extern "C" {
        static environ: *const *const c_char;
    }
    unsafe { environ }
}
