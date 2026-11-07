//! What compiled code calls: every function here is reached through a slot of
//! the stub `libpizlo.so`, with the signature the compiler declares for it. A
//! flight pointer argument is two word arguments, `ptr` and then `lower`, since
//! the compiler flattens the struct, and may well split across the registers.
//! A returned one comes back in two registers, which `Ptr` gives us.
//!
//! Nothing here is allowed to start a collection, other than the functions the
//! compiled caller treats as a safepoint (see `gc`).

use std::ffi::{CStr, c_char};
use std::sync::atomic::Ordering;

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::globals::ConstantRelocation;
use crate::object::{self, Access};
use crate::{cc, globals, memops, ptrs, thread};

fn flight(ptr: *mut u8, lower: *mut u8) -> Ptr {
    Ptr { ptr, lower }
}

/// Records where the current frame is, for reports.
pub unsafe fn set_origin(thread: *mut Thread, origin: *const Origin) {
    unsafe {
        if !origin.is_null() && !(*thread).top_frame.is_null() {
            (*(*thread).top_frame).origin = origin;
        }
    }
}

fn fix_origin(origin: *const Origin) {
    let thread = thread::current();
    if !thread.is_null() {
        unsafe { set_origin(thread, origin) };
    }
}

fn describe(ptr: Ptr) -> String {
    if ptr.lower.is_null() {
        return format!("{:p},<null>", ptr.ptr);
    }
    let object = unsafe { &*ptr.object() };
    format!("{:p},{:p},{:p}", ptr.ptr, ptr.lower, object.upper())
}

#[repr(C)]
pub struct AccessCheckOrigin {
    size: u32,
    alignment: u8,
    alignment_offset: u8,
    needs_write: bool,
    scheduled_origin: *const Origin,
}

fn expected_text(check: &AccessCheckOrigin) -> String {
    let mut text = String::from("    expected");
    if check.size != 0 {
        let writable = if check.needs_write { "writable " } else { "" };
        text += &format!(" {} {writable}bytes", check.size);
    } else if check.needs_write {
        text += " writable capability";
    } else {
        text += " valid capability";
    }
    if check.alignment > 1 {
        text += &format!(" with ptr aligned to {} bytes", check.alignment);
        if check.alignment_offset != 0 {
            text += &format!(" at offset {}", check.alignment_offset);
        }
    }
    text.push('.');
    text
}

fn access_text(check: &AccessCheckOrigin) -> &'static str {
    match (check.size != 0, check.needs_write) {
        (true, true) => "write",
        (true, false) => "read",
        (false, _) => "access",
    }
}

unsafe extern "C" fn filc_optimized_access_check_fail(
    ptr: *mut u8,
    lower: *mut u8,
    check: *const AccessCheckOrigin,
) -> ! {
    let check = unsafe { &*check };
    let flight = flight(ptr, lower);
    fix_origin(check.scheduled_origin);
    let verb = access_text(check);
    let size = check.size as usize;
    let reason = if lower.is_null() {
        format!("cannot {verb} pointer with null object.")
    } else {
        let object = unsafe { &*flight.object() };
        let flags = object.flags();
        let upper = object.upper() as usize;
        let address = ptr as usize;
        if flags & FLAG_FREE != 0 {
            format!("cannot {verb} pointer to free object.")
        } else if flags & FLAGS_SPECIAL_MASK != 0 {
            format!("cannot {verb} pointer to special object.")
        } else if size != 0 && address < lower as usize {
            format!("cannot {verb} pointer with ptr < lower.")
        } else if size != 0 && address >= upper {
            format!("cannot {verb} pointer with ptr >= upper.")
        } else if size as isize > upper.wrapping_sub(address) as isize {
            format!("cannot {verb} {size} bytes when upper - ptr = {}.", upper.wrapping_sub(address))
        } else if check.needs_write && flags & FLAG_READONLY != 0 {
            "cannot write to read-only object.".to_string()
        } else {
            format!("alignment requirement of {} bytes not met.", check.alignment)
        }
    };
    safety_panic(format_args!("{reason}\n    pointer: {}\n{}", describe(flight), expected_text(check)));
}

#[repr(C)]
pub struct AlignmentContradictionOrigin {
    alignments: *const [u8; 2],
    scheduled_origin: *const Origin,
}

unsafe fn alignment_contradiction(pointer: String, check: *const AlignmentContradictionOrigin) -> ! {
    let check = unsafe { &*check };
    fix_origin(check.scheduled_origin);
    let mut report = format!("alignment contradiction.\n    pointer: {pointer}\nrequired alignments:");
    let mut cursor = check.alignments;
    unsafe {
        while (*cursor)[0] != 0 {
            report += &format!("\n    align to {} at offset {}.", (*cursor)[0], (*cursor)[1]);
            cursor = cursor.add(1);
        }
    }
    safety_panic(format_args!("{report}"));
}

unsafe extern "C" fn filc_optimized_alignment_contradiction(
    ptr: *mut u8,
    lower: *mut u8,
    check: *const AlignmentContradictionOrigin,
) -> ! {
    unsafe { alignment_contradiction(describe(flight(ptr, lower)), check) }
}

unsafe extern "C" fn filc_optimized_stack_alignment_contradiction(
    offset: isize,
    size: usize,
    check: *const AlignmentContradictionOrigin,
) -> ! {
    unsafe { alignment_contradiction(format!("stack_optimized(offset={offset},size={size})"), check) }
}

unsafe extern "C" fn filc_optimized_stack_access_check_fail(
    offset: isize,
    size: usize,
    check: *const AccessCheckOrigin,
) -> ! {
    let check = unsafe { &*check };
    fix_origin(check.scheduled_origin);
    let verb = access_text(check);
    let wanted = check.size as usize;
    let reason = if wanted != 0 && offset < 0 {
        format!("cannot {verb} pointer with ptr < lower.")
    } else if wanted != 0 && offset as usize >= size {
        format!("cannot {verb} pointer with ptr >= upper.")
    } else if wanted > size - offset as usize {
        format!("cannot {verb} {wanted} bytes when upper - ptr = {}.", size - offset as usize)
    } else {
        format!("alignment requirement of {} bytes not met.", check.alignment)
    };
    safety_panic(format_args!(
        "{reason}\n    pointer: stack_optimized(offset={offset},size={size})\n{}",
        expected_text(check)
    ));
}

fn masked_failure(ptr: Ptr, mask: u64, size: usize, access: &str, origin: *const Origin) -> ! {
    fix_origin(origin);
    safety_panic(format_args!(
        "masked {access} not in bounds (even accounting for the mask).\n    pointer: {}\n    mask: {mask:#x}\n    vector size in bytes: {size}",
        describe(ptr)
    ));
}

unsafe extern "C" fn filc_masked_write_check_fail(p: *mut u8, l: *mut u8, mask: u64, size: usize, o: *const Origin) -> ! {
    masked_failure(flight(p, l), mask, size, "write", o)
}

unsafe extern "C" fn filc_masked_read_check_fail(p: *mut u8, l: *mut u8, mask: u64, size: usize, o: *const Origin) -> ! {
    masked_failure(flight(p, l), mask, size, "read", o)
}

fn compress_failure(ptr: Ptr, mask: u64, element: usize, vector: usize, access: &str, origin: *const Origin) -> ! {
    fix_origin(origin);
    let bits = mask.count_ones();
    safety_panic(format_args!(
        "{access} not in bounds (even accounting for the mask).\n    pointer: {}\n    mask: {mask:#x} ({bits} bits)\n    vector size in bytes: {vector}\n    element size in bytes: {element}\n    access size in bytes: {}",
        describe(ptr),
        element * bits as usize
    ));
}

unsafe extern "C" fn filc_compress_write_check_fail(p: *mut u8, l: *mut u8, m: u64, e: usize, v: usize, o: *const Origin) -> ! {
    compress_failure(flight(p, l), m, e, v, "compress store", o)
}

unsafe extern "C" fn filc_expand_read_check_fail(p: *mut u8, l: *mut u8, m: u64, e: usize, v: usize, o: *const Origin) -> ! {
    compress_failure(flight(p, l), m, e, v, "expand load", o)
}

unsafe extern "C" fn filc_check_function_call_fail(ptr: *mut u8, lower: *mut u8) -> ! {
    object::check_function_call(flight(ptr, lower));
    internal_panic(format_args!("function call check failed, but the call is fine"));
}

unsafe extern "C" fn filc_check_closure_fail(lower: *mut u8, origin: *const Origin) -> ! {
    fix_origin(origin);
    safety_panic(format_args!("object {:p} is not a closure.", object_for_lower(lower)));
}

unsafe extern "C" fn filc_comdat_link_fail(name: *const c_char, signature: u64) -> ! {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    safety_panic(format_args!("linking comdat function {name} with signature {signature} failed."));
}

unsafe extern "C" fn filc_cc_args_check_failure(actual: usize, expected: usize, origin: *const Origin) -> ! {
    fix_origin(origin);
    safety_panic(format_args!("argument size mismatch (actual = {actual}, expected = {expected})."));
}

unsafe extern "C" fn filc_cc_rets_check_failure(actual: usize, expected: usize, origin: *const Origin) -> ! {
    fix_origin(origin);
    safety_panic(format_args!("return size mismatch (actual = {actual}, expected = {expected})."));
}

unsafe extern "C" fn filc_stack_overflow_failure() -> ! {
    safety_panic(format_args!("stack overflow."));
}

unsafe extern "C" fn filc_error(reason: *const c_char, origin: *const Origin) -> ! {
    fix_origin(origin);
    let reason = unsafe { CStr::from_ptr(reason) }.to_string_lossy();
    safety_panic(format_args!("{reason}"));
}

unsafe extern "C" fn filc_pollcheck_slow(thread: *mut Thread, origin: *const Origin) {
    unsafe { set_origin(thread, origin) };
    unsafe { crate::thread::safepoint(thread) };
}

unsafe extern "C" fn filc_enter(thread: *mut Thread) {
    unsafe { thread::enter(thread) };
}

unsafe extern "C" fn filc_exit(thread: *mut Thread) {
    unsafe { thread::exit(thread) };
}

/// The barrier belongs to a concurrent collector. This collector stops the
/// world, and `filc_current_marking_state` stays 0, so no caller gets here.
unsafe extern "C" fn filc_store_barrier_for_lower_slow(_: *mut Thread, _: *mut u8) {
    internal_panic(format_args!("store barrier called with no marking in progress"));
}

unsafe extern "C" fn filc_store_ptr_atomic_outline(t: *mut Thread, p: *mut u8, l: *mut u8, vp: *mut u8, vl: *mut u8) {
    unsafe { ptrs::store_atomic(t, flight(p, l), flight(vp, vl)) };
}

unsafe extern "C" fn filc_load_ptr_atomic_with_manual_tracking_outline(p: *mut u8, l: *mut u8) -> Ptr {
    unsafe { ptrs::load_atomic(flight(p, l)) }
}

unsafe extern "C" fn filc_object_ensure_aux_ptr_outline(t: *mut Thread, object: *mut Object) -> *mut u8 {
    unsafe { object::ensure_aux(t, object) }
}

unsafe extern "C" fn filc_thread_ensure_cc_outline_buffer_slow(t: *mut Thread, size: usize) {
    unsafe { cc::ensure_outline(t, size) };
}

fn offset_ptr(ptr: Ptr, offset: isize) -> Ptr {
    ptr.with_ptr(ptr.ptr.wrapping_offset(offset))
}

unsafe extern "C" fn filc_strong_cas_ptr_with_manual_tracking(
    t: *mut Thread,
    p: *mut u8,
    l: *mut u8,
    offset: isize,
    ep: *mut u8,
    el: *mut u8,
    np: *mut u8,
    nl: *mut u8,
) -> Ptr {
    unsafe { ptrs::strong_cas(t, offset_ptr(flight(p, l), offset), flight(ep, el), flight(np, nl)) }
}

unsafe extern "C" fn filc_xchg_ptr_with_manual_tracking(
    t: *mut Thread,
    p: *mut u8,
    l: *mut u8,
    offset: isize,
    np: *mut u8,
    nl: *mut u8,
) -> Ptr {
    unsafe { ptrs::xchg(t, offset_ptr(flight(p, l), offset), flight(np, nl)) }
}

unsafe extern "C" fn filc_get_next_ptr_bytes_for_va_arg(p: *mut u8, l: *mut u8, size: usize, alignment: usize) -> Ptr {
    let list = flight(p, l);
    unsafe {
        let value = ptrs::load(list);
        let alignment = alignment.max(WORD_SIZE);
        let address = (value.ptr as usize + alignment - 1) & !(alignment - 1);
        let result = value.with_ptr(address as *mut u8);
        object::check_access(result, size, Access::Read);
        // Just the address, not the whole pointer: the capability is the same
        // if the `va_list` is used properly, and nothing here can allocate.
        object::check_word_access(list, Access::Write);
        *list.ptr.cast::<usize>() = address + size;
        let aux = if value.lower.is_null() { std::ptr::null_mut() } else { (*value.object()).aux_ptr() };
        let entry = if aux.is_null() { std::ptr::null_mut() } else { aux.wrapping_add(result.offset()) };
        Ptr { ptr: result.ptr, lower: entry }
    }
}

unsafe extern "C" fn filc_allocate(t: *mut Thread, size: usize) -> *mut Object {
    unsafe {
        thread::poll(t);
        object::allocate(t, size, 0)
    }
}

unsafe extern "C" fn filc_allocate_without_exiting(t: *mut Thread, size: usize) -> *mut Object {
    unsafe { object::allocate(t, size, 0) }
}

unsafe extern "C" fn filc_allocate_with_alignment(t: *mut Thread, size: usize, alignment: usize) -> *mut Object {
    unsafe {
        thread::poll(t);
        object::allocate_aligned(t, size, alignment, 0)
    }
}

unsafe extern "C" fn filc_allocate_with_alignment_without_exiting(t: *mut Thread, size: usize, alignment: usize) -> *mut Object {
    unsafe { object::allocate_aligned(t, size, alignment, 0) }
}

unsafe extern "C" fn verse_local_allocator_allocate(slot: *mut AllocatorSlot) -> *mut u8 {
    crate::heap::heap().allocate_from_slot(unsafe { &mut *slot })
}

unsafe extern "C" fn filc_allocate_thread_local(t: *mut Thread, size: usize, alignment: usize) -> *mut Object {
    unsafe {
        let object = object::allocate_aligned(t, size, alignment, 0);
        thread::extra(t).thread_locals.push(object);
        object
    }
}

unsafe extern "C" fn filc_allocate_thread_local_with_ptrs(t: *mut Thread, size: usize, alignment: usize) -> *mut Object {
    unsafe {
        let object = filc_allocate_thread_local(t, size, alignment);
        object::ensure_aux(t, object);
        object
    }
}

unsafe extern "C" fn filc_free_with_checks(p: *mut u8, l: *mut u8) {
    unsafe { object::free(flight(p, l)) };
}

unsafe extern "C" fn filc_log_allocate(_: *mut Thread, _: *const Origin, size: usize, alignment: usize) {
    eprintln!("filc: allocating {size} bytes aligned to {alignment}");
}

unsafe extern "C" fn filc_memset(t: *mut Thread, p: *mut u8, l: *mut u8, value: u32, count: usize, o: *const Origin) {
    unsafe {
        set_origin(t, o);
        memops::set_memory(flight(p, l), value, count);
    }
}

unsafe extern "C" fn filc_memmove(
    t: *mut Thread,
    dp: *mut u8,
    dl: *mut u8,
    sp: *mut u8,
    sl: *mut u8,
    count: usize,
    o: *const Origin,
) {
    unsafe {
        set_origin(t, o);
        memops::move_memory(t, flight(dp, dl), flight(sp, sl), count);
    }
}

unsafe extern "C" fn filc_memmove_already_checked(
    t: *mut Thread,
    dp: *mut u8,
    dl: *mut u8,
    sp: *mut u8,
    sl: *mut u8,
    count: usize,
    _: *const Origin,
) {
    unsafe { memops::move_checked(t, flight(dp, dl), flight(sp, sl), count) };
}

unsafe extern "C" fn filc_memmove_already_checked_small(
    t: *mut Thread,
    dp: *mut u8,
    dl: *mut u8,
    sp: *mut u8,
    sl: *mut u8,
    count: usize,
    _: *const Origin,
) {
    unsafe { memops::move_checked(t, flight(dp, dl), flight(sp, sl), count) };
}

unsafe extern "C" fn filc_finish_memmove_small_1(t: *mut Thread, p: *mut u8, l: *mut u8, a: *mut u8) -> Ptr {
    unsafe { memops::finish_small(t, flight(p, l), &[a]) }
}

unsafe extern "C" fn filc_finish_memmove_small_2(t: *mut Thread, p: *mut u8, l: *mut u8, a: *mut u8, b: *mut u8) -> Ptr {
    unsafe { memops::finish_small(t, flight(p, l), &[a, b]) }
}

unsafe extern "C" fn filc_finish_memmove_small_3(
    t: *mut Thread,
    p: *mut u8,
    l: *mut u8,
    a: *mut u8,
    b: *mut u8,
    c: *mut u8,
) -> Ptr {
    unsafe { memops::finish_small(t, flight(p, l), &[a, b, c]) }
}

unsafe extern "C" fn filc_finish_memmove_small_4(
    t: *mut Thread,
    p: *mut u8,
    l: *mut u8,
    a: *mut u8,
    b: *mut u8,
    c: *mut u8,
    d: *mut u8,
) -> Ptr {
    unsafe { memops::finish_small(t, flight(p, l), &[a, b, c, d]) }
}

unsafe extern "C" fn filc_finish_memmove_small_5(
    t: *mut Thread,
    p: *mut u8,
    l: *mut u8,
    a: *mut u8,
    b: *mut u8,
    c: *mut u8,
    d: *mut u8,
    e: *mut u8,
) -> Ptr {
    unsafe { memops::finish_small(t, flight(p, l), &[a, b, c, d, e]) }
}

unsafe extern "C" fn filc_memmove_already_checked_stack_to_heap(
    t: *mut Thread,
    dp: *mut u8,
    dl: *mut u8,
    payload: *mut u8,
    aux: *mut u8,
    size: usize,
    _: *const Origin,
) {
    unsafe { memops::stack_to_heap_checked(t, flight(dp, dl), payload, aux, size) };
}

unsafe extern "C" fn filc_memmove_stack_to_heap(
    t: *mut Thread,
    dp: *mut u8,
    dl: *mut u8,
    payload: *mut u8,
    aux: *mut u8,
    stack_aux: *mut StackAux,
    size: usize,
    o: *const Origin,
) {
    if size == 0 {
        return;
    }
    unsafe {
        set_origin(t, o);
        let dst = flight(dp, dl);
        object::check_named_access(dst, size, Access::Write, "destination");
        memops::check_stack(aux, stack_aux, size, Access::Read, "source");
        memops::stack_to_heap_checked(t, dst, payload, aux, size);
    }
}

unsafe extern "C" fn filc_memmove_already_checked_heap_to_stack(
    _: *mut Thread,
    payload: *mut u8,
    aux: *mut u8,
    sp: *mut u8,
    sl: *mut u8,
    size: usize,
) {
    unsafe { memops::heap_to_stack_checked(payload, aux, flight(sp, sl), size) };
}

unsafe extern "C" fn filc_memmove_heap_to_stack(
    t: *mut Thread,
    payload: *mut u8,
    aux: *mut u8,
    stack_aux: *mut StackAux,
    sp: *mut u8,
    sl: *mut u8,
    size: usize,
    o: *const Origin,
) {
    if size == 0 {
        return;
    }
    unsafe {
        set_origin(t, o);
        let src = flight(sp, sl);
        memops::check_stack(aux, stack_aux, size, Access::Write, "destination");
        object::check_named_access(src, size, Access::Read, "source");
        memops::heap_to_stack_checked(payload, aux, src, size);
    }
}

unsafe extern "C" fn filc_memmove_already_checked_stack(
    _: *mut Thread,
    dst_payload: *mut u8,
    dst_aux: *mut u8,
    src_payload: *mut u8,
    src_aux: *mut u8,
    size: usize,
) {
    unsafe { memops::stack_to_stack_checked(dst_payload, dst_aux, src_payload, src_aux, size) };
}

unsafe extern "C" fn filc_memmove_stack(
    t: *mut Thread,
    dst_payload: *mut u8,
    dst_aux: *mut u8,
    dst_stack_aux: *mut StackAux,
    src_payload: *mut u8,
    src_aux: *mut u8,
    src_stack_aux: *mut StackAux,
    size: usize,
    o: *const Origin,
) {
    if size == 0 {
        return;
    }
    unsafe {
        set_origin(t, o);
        memops::check_stack(dst_aux, dst_stack_aux, size, Access::Write, "destination");
        memops::check_stack(src_aux, src_stack_aux, size, Access::Read, "source");
        memops::stack_to_stack_checked(dst_payload, dst_aux, src_payload, src_aux, size);
    }
}

unsafe extern "C" fn filc_promote_already_checked_stack_to_heap_without_exiting(
    t: *mut Thread,
    payload: *mut u8,
    aux: *mut u8,
    size: usize,
) -> Ptr {
    if size == 0 {
        return Ptr::NULL;
    }
    unsafe {
        let result = Ptr::for_object(object::allocate(t, size, 0));
        memops::stack_to_heap_checked(t, result, payload, aux, size);
        result
    }
}

unsafe extern "C" fn filc_promote_already_checked_stack_to_heap_with_alignment_without_exiting(
    t: *mut Thread,
    payload: *mut u8,
    aux: *mut u8,
    size: usize,
    alignment: usize,
) -> Ptr {
    if size == 0 {
        return Ptr::NULL;
    }
    unsafe {
        let result = Ptr::for_object(object::allocate_aligned(t, size, alignment, 0));
        memops::stack_to_heap_checked(t, result, payload, aux, size);
        result
    }
}

unsafe extern "C" fn filc_demote_word_aligned_already_checked_heap_to_stack_without_exiting(
    p: *mut u8,
    l: *mut u8,
    payload: *mut u8,
    aux: *mut u8,
    size: usize,
) {
    unsafe { memops::heap_to_stack_checked(payload, aux, flight(p, l), size) };
}

unsafe extern "C" fn filc_demote_already_checked_heap_to_stack_without_exiting(
    p: *mut u8,
    l: *mut u8,
    payload: *mut u8,
    aux: *mut u8,
    size: usize,
) {
    unsafe { memops::heap_to_stack_checked(payload, aux, flight(p, l), size) };
}

unsafe extern "C" fn filc_promote_args_to_heap(t: *mut Thread, size: usize) -> Ptr {
    unsafe { cc::promote_to_heap(t, size) }
}

unsafe extern "C" fn filc_prepare_to_return_with_data(t: *mut Thread, p: *mut u8, l: *mut u8, o: *const Origin) -> usize {
    unsafe {
        set_origin(t, o);
        cc::demote_from_heap(t, flight(p, l))
    }
}

unsafe extern "C" fn filc_global_initialization_start(
    t: *mut Thread,
    origin: *const Origin,
    slot: *mut Ptr,
    object: *mut Object,
) -> bool {
    unsafe { globals::start(t, origin, slot, object) }
}

unsafe extern "C" fn filc_global_initialization_end(_: *mut Thread) {
    globals::end();
}

unsafe extern "C" fn filc_call_ifunc(t: *mut Thread, origin: *const Origin, slot: *mut Ptr, p: *mut u8, l: *mut u8) -> Ptr {
    unsafe { globals::call_ifunc(t, origin, slot, flight(p, l)) }
}

unsafe extern "C" fn filc_execute_constant_relocations(
    t: *mut Thread,
    constant: *mut Object,
    relocations: *const ConstantRelocation,
    count: usize,
) {
    unsafe { globals::execute_relocations(t, constant, relocations, count) };
}

unsafe extern "C" fn filc_defer_or_run_global_ctor(p: *mut u8, l: *mut u8) {
    unsafe { globals::defer_or_run_ctor(flight(p, l)) };
}

unsafe extern "C" fn filc_run_global_dtor(p: *mut u8, l: *mut u8) {
    unsafe { globals::run_dtor(flight(p, l)) };
}

macro_rules! helpers {
    ($($name:ident),* $(,)?) => {
        /// Every helper with an implementation so far. A slot with no entry
        /// here reports its name if something calls it.
        pub fn table() -> Vec<(&'static str, *const ())> {
            vec![$((stringify!($name), $name as *const ())),*]
        }
    };
}

helpers! {
    filc_allocate,
    filc_allocate_thread_local,
    filc_allocate_thread_local_with_ptrs,
    filc_allocate_with_alignment,
    filc_allocate_with_alignment_without_exiting,
    filc_allocate_without_exiting,
    filc_call_ifunc,
    filc_cc_args_check_failure,
    filc_cc_rets_check_failure,
    filc_check_closure_fail,
    filc_check_function_call_fail,
    filc_comdat_link_fail,
    filc_compress_write_check_fail,
    filc_defer_or_run_global_ctor,
    filc_demote_already_checked_heap_to_stack_without_exiting,
    filc_demote_word_aligned_already_checked_heap_to_stack_without_exiting,
    filc_enter,
    filc_error,
    filc_execute_constant_relocations,
    filc_exit,
    filc_expand_read_check_fail,
    filc_finish_memmove_small_1,
    filc_finish_memmove_small_2,
    filc_finish_memmove_small_3,
    filc_finish_memmove_small_4,
    filc_finish_memmove_small_5,
    filc_free_with_checks,
    filc_get_next_ptr_bytes_for_va_arg,
    filc_global_initialization_end,
    filc_global_initialization_start,
    filc_load_ptr_atomic_with_manual_tracking_outline,
    filc_log_allocate,
    filc_masked_read_check_fail,
    filc_masked_write_check_fail,
    filc_memmove,
    filc_memmove_already_checked,
    filc_memmove_already_checked_heap_to_stack,
    filc_memmove_already_checked_small,
    filc_memmove_already_checked_stack,
    filc_memmove_already_checked_stack_to_heap,
    filc_memmove_heap_to_stack,
    filc_memmove_stack,
    filc_memmove_stack_to_heap,
    filc_memset,
    filc_object_ensure_aux_ptr_outline,
    filc_optimized_access_check_fail,
    filc_optimized_alignment_contradiction,
    filc_optimized_stack_access_check_fail,
    filc_optimized_stack_alignment_contradiction,
    filc_pollcheck_slow,
    filc_prepare_to_return_with_data,
    filc_promote_already_checked_stack_to_heap_with_alignment_without_exiting,
    filc_promote_already_checked_stack_to_heap_without_exiting,
    filc_promote_args_to_heap,
    filc_run_global_dtor,
    filc_stack_overflow_failure,
    filc_store_barrier_for_lower_slow,
    filc_store_ptr_atomic_outline,
    filc_strong_cas_ptr_with_manual_tracking,
    filc_thread_ensure_cc_outline_buffer_slow,
    filc_xchg_ptr_with_manual_tracking,
    verse_local_allocator_allocate,
}

pub unsafe fn is_entered(thread: *mut Thread) -> bool {
    unsafe { (*thread).state.load(Ordering::Relaxed) & STATE_ENTERED != 0 }
}
