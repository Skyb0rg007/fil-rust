//! The calling convention: arguments and return values travel through per-thread
//! buffers, payload and aux side by side, and the size is passed along so both
//! ends can check it.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr;

use crate::abi::*;
use crate::failure::safety_panic;
use crate::object::{self, ensure_aux};
use crate::ptrs::extract_lower;

fn outline_layout(size: usize) -> Layout {
    Layout::from_size_align(size, CC_ALIGNMENT).unwrap()
}

pub unsafe fn ensure_outline(thread: *mut Thread, size: usize) {
    let this = unsafe { &mut *thread };
    if size <= this.cc_outline_size {
        return;
    }
    let size = (size + CC_ALIGNMENT - 1) & !(CC_ALIGNMENT - 1);
    unsafe {
        release_outline(thread);
        this.cc_outline = alloc_zeroed(outline_layout(size));
        this.cc_outline_aux = alloc_zeroed(outline_layout(size));
    }
    if this.cc_outline.is_null() || this.cc_outline_aux.is_null() {
        std::alloc::handle_alloc_error(outline_layout(size));
    }
    this.cc_outline_size = size;
}

pub unsafe fn release_outline(thread: *mut Thread) {
    let this = unsafe { &mut *thread };
    if this.cc_outline_size != 0 {
        unsafe {
            dealloc(this.cc_outline, outline_layout(this.cc_outline_size));
            dealloc(this.cc_outline_aux, outline_layout(this.cc_outline_size));
        }
        this.cc_outline_size = 0;
    }
}

pub unsafe fn ensure_total(thread: *mut Thread, size: usize) {
    if size > CC_INLINE_SIZE {
        unsafe { ensure_outline(thread, size - CC_INLINE_SIZE) };
    }
}

pub unsafe fn slot(thread: *mut Thread, offset: usize) -> *mut u64 {
    let this = thread;
    if offset < CC_INLINE_SIZE {
        unsafe { (&raw mut (*this).cc_inline).cast::<u8>().add(offset).cast() }
    } else {
        unsafe { (*this).cc_outline.add(offset - CC_INLINE_SIZE).cast() }
    }
}

pub unsafe fn aux_slot(thread: *mut Thread, offset: usize) -> *mut u64 {
    let this = thread;
    if offset < CC_INLINE_SIZE {
        unsafe { (&raw mut (*this).cc_inline_aux).cast::<u8>().add(offset).cast() }
    } else {
        unsafe { (*this).cc_outline_aux.add(offset - CC_INLINE_SIZE).cast() }
    }
}

/// Copies the buffers to a new read-only object. Doesn't reach a safepoint: the
/// pointers are visible to nothing else until the caller records the result.
pub unsafe fn promote_to_heap(thread: *mut Thread, size: usize) -> Ptr {
    if size == 0 {
        return Ptr::NULL;
    }
    unsafe {
        let object = object::allocate(thread, size, FLAG_READONLY);
        let result = Ptr::for_object(object);
        let mut offset = 0;
        while offset < size {
            *result.ptr.add(offset).cast::<u64>() = *slot(thread, offset);
            offset += WORD_SIZE;
        }
        let mut offset = 0;
        while offset < size {
            let lower = *aux_slot(thread, offset);
            if lower != 0 {
                let aux = ensure_aux(thread, object);
                object::aux_word(aux, offset).store(lower as usize, std::sync::atomic::Ordering::Relaxed);
            }
            offset += WORD_SIZE;
        }
        result
    }
}

/// Copies an object to the buffers, and returns how many bytes that was.
pub unsafe fn demote_from_heap(thread: *mut Thread, ptr_: Ptr) -> usize {
    if ptr_.ptr.is_null() {
        return 0;
    }
    if ptr_.lower.is_null() {
        safety_panic(format_args!("cannot access ptr with null object (ptr = {:p}).", ptr_.ptr));
    }
    let object = unsafe { &*ptr_.object() };
    if object.flags() & FLAG_FREE != 0 {
        safety_panic(format_args!("cannot access free object (ptr = {:p}).", ptr_.ptr));
    }
    if object.is_special() {
        safety_panic(format_args!("cannot access special object (ptr = {:p}).", ptr_.ptr));
    }
    let misaligned = ptr_.ptr as usize % WORD_SIZE;
    if misaligned != 0 {
        safety_panic(format_args!(
            "alignment requirement of {WORD_SIZE} bytes not met; in this case ptr % {WORD_SIZE} = {misaligned} (ptr = {:p}).",
            ptr_.ptr
        ));
    }
    let upper = object.upper() as usize;
    let address = ptr_.ptr as usize;
    if address < ptr_.lower as usize || address >= upper {
        return 0;
    }
    let available = upper - address;
    unsafe {
        ensure_total(thread, available);
        let aux = object.aux_ptr();
        let base = ptr_.offset();
        let mut offset = 0;
        while offset < available {
            *slot(thread, offset) = *ptr_.ptr.add(offset).cast::<u64>();
            let lower = if aux.is_null() {
                ptr::null_mut()
            } else {
                let entry = object::aux_word(aux, base + offset).load(std::sync::atomic::Ordering::Relaxed);
                extract_lower(entry)
            };
            *aux_slot(thread, offset) = lower as u64;
            offset += WORD_SIZE;
        }
    }
    available
}
