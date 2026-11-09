//! `<dirent.h>`. A `DIR` is opaque to programs, so it's a GC object holding
//! the entry `readdir` last returned, in glibc's `struct dirent` layout; the
//! host's `DIR *` is found by the object's identity. `closedir` frees the
//! object, so a later use, or a use of the last entry, traps.

use std::collections::HashMap;
use std::ffi::{CString, c_int, c_long};
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::natives;
use crate::object::{self, Access};
use crate::glibc::set_errno;
use crate::sys::plain;
use crate::{thread, view};

/// Where the entry starts in the object.
const ENTRY: usize = 16;
/// `struct dirent`: `d_ino`, `d_off`, `d_reclen`, `d_type`, then 256 bytes
/// of `d_name`, padded to 280.
const ENTRY_SIZE: usize = 280;
const NAME: usize = 19;

/// Open directories' host `DIR *`s by their object's lower bound. Roots;
/// locked only entered, never across an allocation or a call.
static DIRS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

pub fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let dirs = DIRS.lock().unwrap_or_else(|e| e.into_inner());
    for &lower in dirs.iter().flat_map(|dirs| dirs.keys()) {
        visit(object_for_lower(lower as *mut u8));
    }
}

/// The host stream `dir` is, which has to be a `DIR` this libc opened and
/// hasn't closed.
fn host(dir: Ptr) -> *mut libc::DIR {
    if dir.ptr.is_null() {
        crate::failure::safety_panic(format_args!("cannot use a null DIR."));
    }
    object::check_access(dir, ENTRY + ENTRY_SIZE, Access::Read);
    let dirs = DIRS.lock().unwrap_or_else(|e| e.into_inner());
    match dirs.as_ref().and_then(|dirs| dirs.get(&(dir.lower as usize))).filter(|_| dir.ptr == dir.lower) {
        Some(&host) => host as *mut libc::DIR,
        None => crate::failure::safety_panic(format_args!("not a DIR this libc opened (ptr = {:p}).", dir.ptr)),
    }
}

/// A host directory stream as a new `DIR`, or null with the host's `errno`.
unsafe fn open(me: *mut Thread, call: impl FnOnce() -> *mut libc::DIR) -> Ptr {
    let (host, error) = unsafe {
        plain(me, || {
            *libc::__errno_location() = 0;
            let host = call();
            (host, *libc::__errno_location())
        })
    };
    if host.is_null() {
        unsafe { set_errno(me, error) };
        return Ptr::NULL;
    }
    unsafe {
        let object = object::allocate(me, ENTRY + ENTRY_SIZE, 0);
        let dir = Ptr::for_object(object);
        DIRS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(dir.lower as usize, host as usize);
        thread::track(me, object);
        dir
    }
}

/// The next entry, copied into `entry`, or `None` at the end or on an error,
/// with the host's `errno` for an error.
unsafe fn next(me: *mut Thread, dir: Ptr, entry: Ptr) -> Result<bool, c_int> {
    let host = host(dir);
    let read = unsafe {
        plain(me, || {
            *libc::__errno_location() = 0;
            let found = libc::readdir64(host);
            if found.is_null() {
                return Err(*libc::__errno_location());
            }
            let found = &*found;
            let name = std::ffi::CStr::from_ptr(found.d_name.as_ptr()).to_bytes();
            let mut bytes = Vec::with_capacity(NAME + name.len() + 1);
            bytes.extend_from_slice(&found.d_ino.to_le_bytes());
            bytes.extend_from_slice(&found.d_off.to_le_bytes());
            bytes.extend_from_slice(&(ENTRY_SIZE as u16).to_le_bytes());
            bytes.push(found.d_type);
            bytes.extend_from_slice(name);
            bytes.push(0);
            Ok(bytes)
        })
    };
    match read {
        Ok(bytes) => {
            view::write(entry, &bytes);
            Ok(true)
        }
        Err(0) => Ok(false),
        Err(error) => Err(error),
    }
}

natives! {
    fn opendir(thread: *mut Thread, path: Ptr) -> Ptr {
        let path = unsafe { CString::from_vec_unchecked(view::c_string(path)) };
        unsafe { open(thread, || libc::opendir(path.as_ptr())) }
    }

    fn fdopendir(thread: *mut Thread, fd: c_int) -> Ptr {
        unsafe { open(thread, || libc::fdopendir(fd)) }
    }

    fn closedir(thread: *mut Thread, dir: Ptr) -> c_int {
        let host = host(dir);
        DIRS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap().remove(&(dir.lower as usize));
        let result = unsafe { crate::sys::sys(thread, || libc::closedir(host)) };
        unsafe { object::free(dir) };
        result
    }

    fn readdir(thread: *mut Thread, dir: Ptr) -> Ptr {
        let entry = dir.with_ptr(dir.ptr.wrapping_add(ENTRY));
        match unsafe { next(thread, dir, entry) } {
            Ok(true) => entry,
            Ok(false) => Ptr::NULL,
            Err(error) => {
                unsafe { set_errno(thread, error) };
                Ptr::NULL
            }
        }
    }

    fn readdir_r(thread: *mut Thread, dir: Ptr, entry: Ptr, result: Ptr) -> c_int {
        object::check_access(entry, ENTRY_SIZE, Access::Write);
        object::check_word_access(result, Access::Write);
        match unsafe { next(thread, dir, entry) } {
            Ok(found) => {
                unsafe { crate::ptrs::store(thread, result, if found { entry } else { Ptr::NULL }) };
                0
            }
            Err(error) => error,
        }
    }

    fn rewinddir(thread: *mut Thread, dir: Ptr) -> () {
        let host = host(dir);
        unsafe { plain(thread, || libc::rewinddir(host)) };
    }

    fn dirfd(thread: *mut Thread, dir: Ptr) -> c_int {
        let host = host(dir);
        unsafe { crate::sys::sys(thread, || libc::dirfd(host)) }
    }

    fn telldir(thread: *mut Thread, dir: Ptr) -> c_long {
        let host = host(dir);
        unsafe { crate::sys::sys(thread, || libc::telldir(host)) }
    }

    fn seekdir(thread: *mut Thread, dir: Ptr, position: c_long) -> () {
        let host = host(dir);
        unsafe { plain(thread, || libc::seekdir(host, position)) };
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated_readdir64", readdir::getter as *const ()),
        ("pizlonated_readdir64_r", readdir_r::getter as *const ()),
        ("pizlonated___opendir", opendir::getter as *const ()),
        ("pizlonated___closedir", closedir::getter as *const ()),
        ("pizlonated___readdir", readdir::getter as *const ()),
        ("pizlonated___readdir64", readdir::getter as *const ()),
    ]
}
