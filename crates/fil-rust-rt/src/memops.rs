//! Copying and filling memory, which has to carry capabilities along.
//!
//! Each of these is about two arrays, the payload and the aux. Bytes copy as
//! bytes. An aux entry only stays valid if its whole word is copied to a word
//! at the same phase: partly overwritten pointer words lose their capability.

use std::ptr;
use std::sync::atomic::Ordering;

use crate::abi::*;
use crate::failure::safety_panic;
use crate::object::{Access, aux_word, check_named_access, ensure_aux};
use crate::ptrs::extract_lower;

const WORD_MASK: usize = WORD_SIZE - 1;

fn round_down(value: usize) -> usize {
    value & !WORD_MASK
}

fn round_up(value: usize) -> usize {
    (value + WORD_MASK) & !WORD_MASK
}

unsafe fn nuke_range(aux: *mut u8, start: usize, end: usize) {
    let mut offset = start;
    while offset < end {
        unsafe { aux_word(aux, offset) }.store(0, Ordering::Relaxed);
        offset += WORD_SIZE;
    }
}

unsafe fn nuke_edge(aux: *mut u8, offset: usize) {
    if offset & WORD_MASK != 0 {
        unsafe { aux_word(aux, round_down(offset)) }.store(0, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Memory {
    Heap,
    Stack,
}

/// Brings an aux range up to date after `count` payload bytes were copied.
/// `dst_object` is what to make an aux for if `dst_aux` is null.
#[allow(clippy::too_many_arguments)]
unsafe fn copy_aux(
    thread: *mut Thread,
    dst_aux: *mut u8,
    src_aux: *mut u8,
    dst_start: usize,
    src_start: usize,
    count: usize,
    dst_object: *mut Object,
    src_memory: Memory,
) {
    let dst_end = dst_start + count;
    let in_phase = !src_aux.is_null() && dst_start & WORD_MASK == src_start & WORD_MASK;
    if !in_phase {
        if !dst_aux.is_null() {
            unsafe { nuke_range(dst_aux, round_down(dst_start), round_up(dst_end)) };
        }
        return;
    }
    let first = round_up(dst_start);
    let last = round_down(dst_end);
    let words = last.saturating_sub(first) / WORD_SIZE;
    let src_first = src_start + (first - dst_start);
    let load_lower = |index: usize| unsafe {
        let entry = aux_word(src_aux, src_first + index * WORD_SIZE).load(Ordering::Relaxed);
        match src_memory {
            Memory::Heap => extract_lower(entry),
            Memory::Stack => entry as *mut u8,
        }
    };
    if dst_aux.is_null() {
        // Nothing to clear, and no aux until a pointer turns up to need one.
        let mut aux: *mut u8 = ptr::null_mut();
        for index in 0..words {
            let lower = load_lower(index);
            if lower.is_null() {
                continue;
            }
            if aux.is_null() {
                aux = unsafe { ensure_aux(thread, dst_object) };
            }
            unsafe { aux_word(aux, first + index * WORD_SIZE) }.store(lower as usize, Ordering::Relaxed);
        }
        return;
    }
    let dst_word = |index: usize| unsafe { aux_word(dst_aux, first + index * WORD_SIZE) };
    // Source and destination are the same array when they overlap.
    if (dst_aux as usize).wrapping_add(first) <= (src_aux as usize).wrapping_add(src_first) {
        for index in 0..words {
            dst_word(index).store(load_lower(index) as usize, Ordering::Relaxed);
        }
    } else {
        for index in (0..words).rev() {
            dst_word(index).store(load_lower(index) as usize, Ordering::Relaxed);
        }
    }
    // After the copy: these words may be part of its source.
    unsafe {
        nuke_edge(dst_aux, dst_start);
        nuke_edge(dst_aux, dst_end);
    }
}

/// Copies between objects. The caller has checked the access.
pub unsafe fn move_checked(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) {
    if count == 0 {
        return;
    }
    unsafe {
        ptr::copy(src.ptr, dst.ptr, count);
        let dst_aux = (*dst.object()).aux_ptr();
        let src_aux = (*src.object()).aux_ptr();
        copy_aux(thread, dst_aux, src_aux, dst.offset(), src.offset(), count, dst.object(), Memory::Heap);
    }
}

pub unsafe fn move_memory(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) {
    check_named_access(dst, count, Access::Write, "destination");
    check_named_access(src, count, Access::Read, "source");
    unsafe { move_checked(thread, dst, src, count) };
}

/// Drops the capabilities of the pointers in the words `count` bytes at `ptr_`
/// touch, after something wrote bytes there.
pub unsafe fn forget_pointers(ptr_: Ptr, count: usize) {
    let aux = unsafe { (*ptr_.object()).aux_ptr() };
    if !aux.is_null() && count != 0 {
        let offset = ptr_.offset();
        unsafe { nuke_range(aux, round_down(offset), round_up(offset + count)) };
    }
}

pub unsafe fn set_memory(ptr_: Ptr, value: u32, count: usize) {
    if count == 0 {
        return;
    }
    check_named_access(ptr_, count, Access::Write, "");
    unsafe {
        ptr::write_bytes(ptr_.ptr, value as u8, count);
        let aux = (*ptr_.object()).aux_ptr();
        if !aux.is_null() {
            let offset = ptr_.offset();
            nuke_range(aux, round_down(offset), round_up(offset + count));
        }
    }
}

/// A local's aux pointer may be unaligned like its payload pointer is. The
/// bookkeeping above works in terms of a word-aligned base and an offset.
fn stack_aux_base(aux: *mut u8) -> (*mut u8, usize) {
    (round_down(aux as usize) as *mut u8, aux as usize & WORD_MASK)
}

pub unsafe fn stack_to_heap_checked(thread: *mut Thread, dst: Ptr, payload: *mut u8, aux: *mut u8, count: usize) {
    if count == 0 {
        return;
    }
    let (src_aux, src_start) = stack_aux_base(aux);
    unsafe {
        ptr::copy_nonoverlapping(payload, dst.ptr, count);
        let dst_aux = (*dst.object()).aux_ptr();
        copy_aux(thread, dst_aux, src_aux, dst.offset(), src_start, count, dst.object(), Memory::Stack);
    }
}

pub unsafe fn heap_to_stack_checked(payload: *mut u8, aux: *mut u8, src: Ptr, count: usize) {
    if count == 0 {
        return;
    }
    let (dst_aux, dst_start) = stack_aux_base(aux);
    unsafe {
        ptr::copy_nonoverlapping(src.ptr, payload, count);
        let src_aux = (*src.object()).aux_ptr();
        copy_aux(ptr::null_mut(), dst_aux, src_aux, dst_start, src.offset(), count, ptr::null_mut(), Memory::Heap);
    }
}

pub unsafe fn stack_to_stack_checked(
    dst_payload: *mut u8,
    dst_aux: *mut u8,
    src_payload: *mut u8,
    src_aux: *mut u8,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let (dst_base, dst_start) = stack_aux_base(dst_aux);
    let (src_base, src_start) = stack_aux_base(src_aux);
    unsafe {
        ptr::copy(src_payload, dst_payload, count);
        copy_aux(ptr::null_mut(), dst_base, src_base, dst_start, src_start, count, ptr::null_mut(), Memory::Stack);
    }
}

/// Fails unless `size` bytes fit between `aux` and the end of `stack_aux`.
pub unsafe fn check_stack(aux: *mut u8, stack_aux: *mut StackAux, size: usize, access: Access, name: &str) {
    let verb = match access {
        Access::Read => "read",
        Access::Write => "write",
    };
    let begin = unsafe { std::ptr::addr_of!((*stack_aux).lowers) } as usize;
    let end = begin + unsafe { (*stack_aux).num_lowers } * WORD_SIZE;
    let address = aux as usize;
    if address < begin {
        safety_panic(format_args!("cannot {verb} {name} pointer with ptr < lower"));
    }
    if address >= end {
        safety_panic(format_args!("cannot {verb} {name} pointer with ptr >= upper"));
    }
    if size > end - address {
        let available = end - address;
        safety_panic(format_args!("cannot {verb} {size} bytes when upper - ptr = {available}"));
    }
}

/// `filc_finish_memmove_small`: compiled code copied the payload itself, and
/// has `lowers` to store at `dst`. Returns the aux and where in it they went.
pub unsafe fn finish_small(thread: *mut Thread, dst: Ptr, lowers: &[*mut u8]) -> Ptr {
    let aux = unsafe { ensure_aux(thread, dst.object()) };
    let base = round_down(dst.offset());
    for (index, lower) in lowers.iter().enumerate() {
        unsafe { aux_word(aux, base + index * WORD_SIZE) }.store(*lower as usize, Ordering::Relaxed);
    }
    Ptr { ptr: aux, lower: aux.wrapping_add(base) }
}
