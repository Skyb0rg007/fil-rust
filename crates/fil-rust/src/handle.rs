//! Keeping Fil-C objects alive beyond a scope, and Rust-owned Fil-C buffers.

use fil_rust_rt::shim as rt;
use std::marker::PhantomData;
use std::sync::Mutex;

use fil_rust_sys::RawPtr;

use crate::ptr::{FilPtr, Opaque};
use crate::runtime::{Inner, lock_anywhere};
use crate::{Error, Fil, Result, error};

struct HandleState {
    next: usize,
    free: Vec<usize>,
}

/// Slots of the shim's root table, which the garbage collector treats as
/// live.
pub(crate) struct HandleTable {
    state: Mutex<HandleState>,
    /// Slots released by `Drop` (possibly on a non-Fil-C thread), cleared by
    /// the next Fil-C thread that touches the table.
    pending: Mutex<Vec<usize>>,
}

impl HandleTable {
    pub(crate) fn new() -> HandleTable {
        HandleTable {
            state: Mutex::new(HandleState { next: 0, free: Vec::new() }),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn insert(&self, fil: &Fil<'_>, value: RawPtr) -> Result<usize> {
        self.flush_pending(fil);
        let mut state = fil.lock(&self.state);
        let index = match state.free.pop() {
            Some(index) => index,
            None => {
                let capacity = unsafe { rt::fr_root_capacity() };
                if state.next >= capacity {
                    return Err(Error::HandleTableFull);
                }
                state.next += 1;
                state.next - 1
            }
        };
        // Holding the lock serializes chunk creation in the shim.
        if let Err(e) = error::check(unsafe { rt::fr_root_set(fil.thread, index, value) }) {
            state.free.push(index);
            return Err(e);
        }
        Ok(index)
    }

    pub(crate) fn get(&self, fil: &Fil<'_>, index: usize) -> RawPtr {
        let mut out = RawPtr::NULL;
        let status = unsafe { rt::fr_root_get(fil.thread, index, &mut out) };
        debug_assert_eq!(status, 0);
        out
    }

    pub(crate) fn release(&self, index: usize) {
        // In a forked child the lock may be held forever by a thread that
        // didn't survive the fork; the child can't use the runtime anyway.
        if crate::runtime::in_forked_child() {
            return;
        }
        lock_anywhere(&self.pending).push(index);
    }

    pub(crate) fn flush_pending(&self, fil: &Fil<'_>) {
        let indices = std::mem::take(&mut *fil.lock(&self.pending));
        if indices.is_empty() {
            return;
        }
        let mut state = fil.lock(&self.state);
        for index in indices {
            unsafe { rt::fr_root_set(fil.thread, index, RawPtr::NULL) };
            state.free.push(index);
        }
    }
}

/// A strong reference to a Fil-C object that outlives the scope it came
/// from. It is `Send` and `Sync`; turn it back into a pointer with
/// [`FilHandle::get`] on any Fil-C thread.
///
/// A handle keeps the object from being garbage collected, but Fil-C code can
/// still `free` it; pointers obtained afterwards then report
/// [`Error::Freed`].
pub struct FilHandle<T: ?Sized = Opaque> {
    inner: &'static Inner,
    index: usize,
    _marker: PhantomData<*const T>,
}

// SAFETY: a handle is an index into the runtime's root table, usable only
// through a `Fil`.
unsafe impl<T: ?Sized> Send for FilHandle<T> {}
unsafe impl<T: ?Sized> Sync for FilHandle<T> {}

impl<T: ?Sized> std::fmt::Debug for FilHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FilHandle(#{})", self.index)
    }
}

impl<T: ?Sized> FilHandle<T> {
    /// The pointer, in the scope of `fil`.
    pub fn get<'f>(&self, fil: &Fil<'f>) -> FilPtr<'f, T> {
        FilPtr::from_raw(self.inner.handles.get(fil, self.index))
    }

    /// Another handle to the same object.
    pub fn clone_in(&self, fil: &Fil<'_>) -> Result<FilHandle<T>> {
        fil.root(self.get(fil))
    }
}

impl<T: ?Sized> Drop for FilHandle<T> {
    fn drop(&mut self) {
        self.inner.handles.release(self.index);
    }
}

impl<'f> Fil<'f> {
    /// Roots `ptr` so it stays alive (and usable from other scopes and
    /// threads) until the handle is dropped.
    pub fn root<T: ?Sized>(&self, ptr: FilPtr<'f, T>) -> Result<FilHandle<T>> {
        let index = self.inner.handles.insert(self, ptr.raw())?;
        Ok(FilHandle { inner: self.inner, index, _marker: PhantomData })
    }

    /// Allocates a zeroed [`FilBuf`] of `len` bytes.
    pub fn buf(&self, len: usize) -> Result<FilBuf> {
        let raw = self.alloc_raw(len.max(1), 16)?;
        let index = self.inner.handles.insert(self, raw)?;
        Ok(FilBuf { inner: self.inner, index, data: raw.ptr.cast(), len })
    }

    /// Allocates a [`FilBuf`] holding a copy of `bytes`.
    pub fn buf_from(&self, bytes: &[u8]) -> Result<FilBuf> {
        let mut buf = self.buf(bytes.len())?;
        buf.copy_from_slice(bytes);
        Ok(buf)
    }
}

/// A buffer of Fil-C memory owned exclusively by Rust.
///
/// Because no Fil-C code can reach it, Rust may access it directly as a
/// `&[u8]` / `&mut [u8]`, from any thread, without a `Fil`. When it's ready,
/// [`FilBuf::share`] gives it to Fil-C as a pointer; that consumes the
/// buffer, since Fil-C code may then keep, modify or free it.
pub struct FilBuf {
    inner: &'static Inner,
    index: usize,
    data: *mut u8,
    len: usize,
}

// SAFETY: the buffer is uniquely owned (like a `Vec<u8>`) and kept alive by
// its root; the garbage collector never moves objects or touches payload
// bytes that hold no pointers.
unsafe impl Send for FilBuf {}
unsafe impl Sync for FilBuf {}

impl FilBuf {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Hands the buffer to Fil-C.
    pub fn share<'f>(self, fil: &Fil<'f>) -> FilPtr<'f, u8> {
        // Loading from the root tracks the object in the current scope, so it
        // stays alive after the root is released below.
        let raw = self.inner.handles.get(fil, self.index);
        FilPtr::from_raw(raw)
    }
}

impl std::ops::Deref for FilBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `data` points to at least `len` bytes of a live, rooted
        // object that only this `FilBuf` can access.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

impl std::ops::DerefMut for FilBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` gives exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.len) }
    }
}

impl Drop for FilBuf {
    fn drop(&mut self) {
        self.inner.handles.release(self.index);
    }
}

impl std::fmt::Debug for FilBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilBuf").field("len", &self.len).finish()
    }
}
