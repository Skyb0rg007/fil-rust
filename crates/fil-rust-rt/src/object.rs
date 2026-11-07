//! The object model: making objects, freeing them, and checking accesses.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::abi::*;
use crate::failure::safety_panic;
use crate::heap::{MAX_ALIGNMENT, heap};

fn round_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

/// Object payloads are always a whole number of words of 16.
fn check_size(size: usize) -> usize {
    if size > AUX_PTR_MASK {
        safety_panic(format_args!("attempt to allocate object that is too big (size = {size})."));
    }
    round_up(size, MIN_ALIGN)
}

unsafe fn initialize(object: *mut Object, size: usize, flags: usize) {
    let lower = lower_for_object(object);
    unsafe {
        (*object).upper.store(lower.wrapping_add(size), Ordering::Relaxed);
        (*object).aux.store(aux_create(flags, null_mut()), Ordering::Release);
    }
}

/// Allocates a zeroed object. Doesn't reach a safepoint.
pub unsafe fn allocate(thread: *mut Thread, size: usize, flags: usize) -> *mut Object {
    let size = check_size(size);
    let block = unsafe { heap().allocate(thread, size + OBJECT_SIZE) };
    let object = block.cast::<Object>();
    unsafe { initialize(object, size, flags) };
    object
}

/// Allocates a zeroed object whose payload is aligned to `alignment`.
pub unsafe fn allocate_aligned(thread: *mut Thread, size: usize, alignment: usize, flags: usize) -> *mut Object {
    if alignment <= MIN_ALIGN {
        return unsafe { allocate(thread, size, flags) };
    }
    if alignment > MAX_ALIGNMENT {
        safety_panic(format_args!("alignment {alignment} too big (max alignment is {MAX_ALIGNMENT})."));
    }
    let alignment = alignment.next_power_of_two();
    let size = check_size(size);
    let block = unsafe { heap().allocate(thread, size + OBJECT_SIZE + alignment) };
    // The collector finds the block from any address in it, so unlike libpizlo
    // this needs no header at the start of the allocation.
    let lower = round_up(block as usize + OBJECT_SIZE, alignment) as *mut u8;
    let object = object_for_lower(lower);
    let flags = flags | object_flags(0, alignment.trailing_zeros() as usize);
    unsafe { initialize(object, size, flags) };
    object
}

/// Frees an object: all further accesses fail, and the memory comes back when a
/// collection finds nothing refers to it. The header has to stay as it is
/// until then, because compiled code relies on `upper == lower` for bounds
/// checks and on the flag for the rest.
pub unsafe fn free(ptr: Ptr) {
    if ptr.ptr.is_null() {
        return;
    }
    let object = ptr.object();
    if object.is_null() {
        safety_panic(format_args!("cannot free ptr with no object (ptr = {:p}).", ptr.ptr));
    }
    let object = unsafe { &*object };
    let flags = object.flags();
    if flags & FLAGS_SPECIAL_MASK != 0 {
        safety_panic(format_args!("cannot free special object (ptr = {:p}).", ptr.ptr));
    }
    if flags & FLAG_GLOBAL != 0 {
        safety_panic(format_args!("cannot free global object (ptr = {:p}).", ptr.ptr));
    }
    if flags & FLAG_MMAP != 0 {
        safety_panic(format_args!("cannot free mmap object (ptr = {:p}).", ptr.ptr));
    }
    if ptr.ptr != ptr.lower {
        safety_panic(format_args!("cannot free ptr with ptr != lower (ptr = {:p}).", ptr.ptr));
    }
    object.upper.store(ptr.lower, Ordering::Relaxed);
    loop {
        let aux = object.aux.load(Ordering::Relaxed);
        if aux_flags(aux) & FLAG_FREE != 0 {
            safety_panic(format_args!("cannot free already free object (ptr = {:p}).", ptr.ptr));
        }
        let freed = aux | (FLAG_FREE << AUX_FLAGS_SHIFT);
        if object.aux.compare_exchange_weak(aux, freed, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return;
        }
    }
}

/// The object's aux array, created if it doesn't have one yet. Doesn't reach a
/// safepoint.
pub unsafe fn ensure_aux(thread: *mut Thread, object: *mut Object) -> *mut u8 {
    let object = unsafe { &*object };
    let existing = object.aux_ptr();
    if !existing.is_null() {
        return existing;
    }
    let size = object.size();
    if size == 0 {
        // Compiled code can get here with a stale check, for an object that
        // was freed since. Its size says nothing, and it mustn't get an aux.
        safety_panic(format_args!("attempt to create aux for object with zero size."));
    }
    let new_aux = unsafe { heap().allocate(thread, size) };
    loop {
        let aux = object.aux.load(Ordering::Acquire);
        if !aux_ptr(aux).is_null() {
            // Lost a race: the block is unreachable, and the next collection
            // takes it back.
            return aux_ptr(aux);
        }
        if aux_flags(aux) & FLAG_FREE != 0 {
            safety_panic(format_args!("attempt to create aux for free object."));
        }
        let updated = aux_create(aux_flags(aux), new_aux);
        if object.aux.compare_exchange_weak(aux, updated, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return new_aux;
        }
    }
}

/// An aux word, as an atomic.
pub unsafe fn aux_word<'a>(aux: *mut u8, offset: usize) -> &'a AtomicUsize {
    unsafe { &*aux.wrapping_add(offset).cast::<AtomicUsize>() }
}

/// Which way an access goes, for checks and for reporting them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    fn verb(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
        }
    }
}

/// What's wrong with an access, if anything.
pub fn access_error(ptr: Ptr, size: usize, access: Access, name: &str) -> Option<String> {
    let verb = format!("{} {name}", access.verb());
    let verb = verb.trim_end();
    let address = ptr.ptr as usize;
    let lower = ptr.lower as usize;
    if ptr.lower.is_null() {
        return Some(format!("cannot {verb} pointer with null object."));
    }
    let object = unsafe { &*ptr.object() };
    let flags = object.flags();
    if flags & FLAG_FREE != 0 {
        return Some(format!("cannot {verb} pointer to free object."));
    }
    if flags & FLAGS_SPECIAL_MASK != 0 {
        return Some(format!("cannot {verb} pointer to special object."));
    }
    if address < lower {
        return Some(format!("cannot {verb} pointer with ptr < lower."));
    }
    let upper = object.upper() as usize;
    if address >= upper {
        return Some(format!("cannot {verb} pointer with ptr >= upper."));
    }
    if size > upper - address {
        return Some(format!("cannot {verb} {size} bytes when upper - ptr = {}.", upper - address));
    }
    if access == Access::Write && flags & FLAG_READONLY != 0 {
        return Some("cannot write to read-only object.".to_string());
    }
    None
}

/// Fails unless `size` bytes at `ptr` can be accessed. Unlike the checks
/// compiled code does inline, this one is complete on its own: it looks at the
/// free flag as well as the bounds, and a zero size is fine for any pointer.
#[inline]
pub fn check_access(ptr: Ptr, size: usize, access: Access) {
    check_named_access(ptr, size, access, "");
}

/// [`check_access`], calling the pointer `name` in the report.
#[inline]
pub fn check_named_access(ptr: Ptr, size: usize, access: Access, name: &str) {
    if size == 0 {
        return;
    }
    if let Some(message) = access_error(ptr, size, access, name) {
        safety_panic(format_args!("{message}"));
    }
}

/// Fails unless a pointer can be read or written at `ptr`.
pub fn check_word_access(ptr: Ptr, access: Access) {
    check_access(ptr, WORD_SIZE, access);
    let remainder = ptr.ptr as usize % WORD_SIZE;
    if remainder != 0 {
        safety_panic(format_args!(
            "alignment requirement of {WORD_SIZE} bytes not met; in this case ptr % {WORD_SIZE} = {remainder}."
        ));
    }
}

/// Fails unless `ptr` can be called.
pub fn check_function_call(ptr: Ptr) {
    if ptr.lower.is_null() {
        safety_panic(format_args!("cannot access special pointer with null object."));
    }
    let object = unsafe { &*ptr.object() };
    if object.flags() & FLAG_FREE != 0 {
        safety_panic(format_args!("cannot access special pointer to free object."));
    }
    if object.special_type() != SPECIAL_FUNCTION {
        safety_panic(format_args!("cannot call pointer that isn't a function."));
    }
    if ptr.ptr != object.aux_ptr() {
        safety_panic(format_args!("cannot call pointer that isn't the start of a function."));
    }
}
