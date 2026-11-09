//! Fil-C memory as natives see it.
//!
//! Other Fil-C threads may change memory a native is reading, and nothing the
//! native does may become unsafe because of it: what they write can change the
//! answer, never what the native touches. So an access is checked once against
//! the object's bounds and flags, and then every byte is read with a relaxed
//! atomic load, which is a plain load that a race can't make undefined. Scans
//! for a terminator stop at the object's end, whatever its contents. The
//! object stays alive meanwhile: natives track their pointer arguments.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::abi::*;
use crate::failure::safety_panic;
use crate::object::{self, Access, check_access};
use crate::thread;

fn byte(address: *const u8) -> &'static AtomicU8 {
    unsafe { &*address.cast::<AtomicU8>() }
}

/// The bytes from a pointer to the end of its object, checked for reading.
#[derive(Clone, Copy)]
pub struct Bytes {
    base: Ptr,
    len: usize,
}

impl Bytes {
    /// Fails unless the byte at `ptr` can be read.
    pub fn readable(ptr: Ptr) -> Bytes {
        check_access(ptr, 1, Access::Read);
        Bytes { base: ptr, len: unsafe { (*ptr.object()).upper() } as usize - ptr.ptr as usize }
    }

    /// The bytes at `ptr` up to `limit`, or to the end of the object if that's
    /// first. Nothing has to be readable if `limit` is 0.
    pub fn readable_up_to(ptr: Ptr, limit: usize) -> Bytes {
        if limit == 0 {
            return Bytes { base: ptr, len: 0 };
        }
        let all = Bytes::readable(ptr);
        Bytes { base: ptr, len: all.len.min(limit) }
    }

    /// Fails unless `len` bytes at `ptr` can be accessed as `access` says.
    pub fn exactly(ptr: Ptr, len: usize, access: Access) -> Bytes {
        check_access(ptr, len, access);
        Bytes { base: ptr, len }
    }

    pub fn len(self) -> usize {
        self.len
    }

    /// The byte at `index`. Past the end, this fails as compiled code would
    /// have reading there.
    pub fn get(self, index: usize) -> u8 {
        if index >= self.len {
            check_access(self.at(index), 1, Access::Read);
            safety_panic(format_args!("cannot read past the end of a {} byte range.", self.len));
        }
        byte(self.base.ptr.wrapping_add(index)).load(Ordering::Relaxed)
    }

    /// A pointer to `index` bytes in, with the same capability.
    pub fn at(self, index: usize) -> Ptr {
        self.base.with_ptr(self.base.ptr.wrapping_add(index))
    }

    /// Where `wanted` first is, if it's here.
    pub fn find(self, wanted: u8) -> Option<usize> {
        (0..self.len).find(|&index| self.get(index) == wanted)
    }

    /// The length of the string here, which must end before the object does.
    pub fn c_str_len(self) -> usize {
        match self.find(0) {
            Some(length) => length,
            None => safety_panic(format_args!("string is not null-terminated (ptr = {:p}).", self.base.ptr)),
        }
    }

    /// The first `count` bytes.
    pub fn to_vec(self, count: usize) -> Vec<u8> {
        (0..count).map(|index| self.get(index)).collect()
    }
}

/// A copy of the string at `ptr`, without its terminator.
pub fn c_string(ptr: Ptr) -> Vec<u8> {
    let bytes = Bytes::readable(ptr);
    bytes.to_vec(bytes.c_str_len())
}

/// Writes `bytes` at `dst`, which must have room. Pointers they overwrite lose
/// their capabilities, as with any byte store.
pub fn write(dst: Ptr, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    check_access(dst, bytes.len(), Access::Write);
    for (index, &value) in bytes.iter().enumerate() {
        byte(dst.ptr.wrapping_add(index)).store(value, Ordering::Relaxed);
    }
    unsafe { crate::memops::forget_pointers(dst, bytes.len()) };
}

/// A new object holding `bytes` and a terminator, tracked by this native.
pub unsafe fn new_c_string(me: *mut Thread, bytes: &[u8]) -> Ptr {
    unsafe {
        let object = object::allocate(me, bytes.len() + 1, 0);
        thread::track(me, object);
        let copy = Ptr::for_object(object);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy.ptr, bytes.len());
        copy
    }
}
