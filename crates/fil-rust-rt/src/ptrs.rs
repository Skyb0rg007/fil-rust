//! Loading and storing pointers in memory.
//!
//! A pointer in memory is its raw address, in the payload like any other word,
//! plus an entry in the object's aux array. The entry is a `lower`, or if the
//! word was ever used atomically, the address of a box (tagged with
//! `ATOMIC_BOX_BIT`) holding the whole pointer, so both words can change as one.

use std::sync::atomic::{Ordering, fence};

use crate::abi::*;
use crate::object::{Access, aux_word, check_word_access, ensure_aux};

/// A 16-byte atomic. There's no `AtomicU128`, and the compiler needs these to
/// be real 16-byte atomics, since it accesses boxes itself.
pub mod atomic16 {
    use crate::abi::Ptr;

    #[cfg(target_arch = "x86_64")]
    unsafe fn cas_raw(target: *mut u128, expected: u128, new: u128) -> u128 {
        let (mut low, mut high) = (expected as u64, (expected >> 64) as u64);
        unsafe {
            // rbx can't be named as an operand: LLVM keeps it for itself. But
            // a `reg` operand can still be given rbx, and the xchg would then
            // replace the address with the new value, so the address is in a
            // fixed register.
            core::arch::asm!(
                "xchg {new_low}, rbx",
                "lock cmpxchg16b [rdi]",
                "mov rbx, {new_low}",
                in("rdi") target,
                new_low = inout(reg) new as u64 => _,
                inout("rax") low,
                inout("rdx") high,
                in("rcx") (new >> 64) as u64,
                options(nostack),
            );
        }
        (low as u128) | ((high as u128) << 64)
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn cas_raw(target: *mut u128, expected: u128, new: u128) -> u128 {
        let (mut low, mut high): (u64, u64);
        unsafe {
            core::arch::asm!(
                "2:",
                "ldaxp {low}, {high}, [{target}]",
                "cmp {low}, {expected_low}",
                "ccmp {high}, {expected_high}, #0, eq",
                "b.ne 3f",
                "stlxp {status:w}, {new_low}, {new_high}, [{target}]",
                "cbnz {status:w}, 2b",
                "b 4f",
                "3:",
                "stlxp {status:w}, {low}, {high}, [{target}]",
                "cbnz {status:w}, 2b",
                "4:",
                target = in(reg) target,
                expected_low = in(reg) expected as u64,
                expected_high = in(reg) (expected >> 64) as u64,
                new_low = in(reg) new as u64,
                new_high = in(reg) (new >> 64) as u64,
                low = out(reg) low,
                high = out(reg) high,
                status = out(reg) _,
                options(nostack),
            );
        }
        (low as u128) | ((high as u128) << 64)
    }

    fn pack(value: Ptr) -> u128 {
        (value.ptr as usize as u128) | ((value.lower as usize as u128) << 64)
    }

    fn unpack(value: u128) -> Ptr {
        Ptr { ptr: value as usize as *mut u8, lower: (value >> 64) as usize as *mut u8 }
    }

    pub unsafe fn cas(target: *mut Ptr, expected: Ptr, new: Ptr) -> Ptr {
        unpack(unsafe { cas_raw(target.cast(), pack(expected), pack(new)) })
    }

    /// An atomic load. As a compare-and-swap, it may write back what's there,
    /// so the memory must be writable, as a box is.
    pub unsafe fn load(target: *mut Ptr) -> Ptr {
        unsafe { cas(target, Ptr::NULL, Ptr::NULL) }
    }

    pub unsafe fn store(target: *mut Ptr, value: Ptr) {
        let mut expected = Ptr::NULL;
        loop {
            let found = unsafe { cas(target, expected, value) };
            if found == expected {
                return;
            }
            expected = found;
        }
    }
}

pub fn is_box(entry: usize) -> bool {
    entry & ATOMIC_BOX_BIT != 0
}

pub fn box_of(entry: usize) -> *mut Ptr {
    (entry & !ATOMIC_BOX_BIT) as *mut Ptr
}

/// The `lower` an aux entry stands for, whichever kind it is.
pub unsafe fn extract_lower(entry: usize) -> *mut u8 {
    if is_box(entry) {
        // A plain load is atomic for the one word.
        unsafe { (*std::ptr::addr_of!((*box_of(entry)).lower).cast::<std::sync::atomic::AtomicPtr<u8>>()).load(Ordering::Relaxed) }
    } else {
        entry as *mut u8
    }
}

fn new_box(thread: *mut Thread, value: Ptr) -> usize {
    let block = unsafe { crate::heap::heap().allocate(thread, size_of::<AtomicBox>()) }.cast::<Ptr>();
    unsafe { block.write(value) };
    fence(Ordering::Release);
    block as usize | ATOMIC_BOX_BIT
}

unsafe fn entry_of<'a>(ptr: Ptr) -> Option<&'a std::sync::atomic::AtomicUsize> {
    let aux = unsafe { (*ptr.object()).aux_ptr() };
    if aux.is_null() { None } else { Some(unsafe { aux_word(aux, ptr.offset()) }) }
}

unsafe fn ensure_entry<'a>(thread: *mut Thread, ptr: Ptr) -> &'a std::sync::atomic::AtomicUsize {
    let aux = unsafe { ensure_aux(thread, ptr.object()) };
    unsafe { aux_word(aux, ptr.offset()) }
}

unsafe fn raw_word<'a>(ptr: Ptr) -> &'a std::sync::atomic::AtomicPtr<u8> {
    unsafe { &*ptr.ptr.cast() }
}

pub unsafe fn load(ptr: Ptr) -> Ptr {
    check_word_access(ptr, Access::Read);
    let entry = unsafe { entry_of(ptr) }.map_or(0, |entry| entry.load(Ordering::Relaxed));
    let lower = unsafe { extract_lower(entry) };
    Ptr { ptr: unsafe { raw_word(ptr) }.load(Ordering::Relaxed), lower }
}

pub unsafe fn store(thread: *mut Thread, ptr: Ptr, value: Ptr) {
    check_word_access(ptr, Access::Write);
    let entry = unsafe { ensure_entry(thread, ptr) };
    entry.store(value.lower as usize, Ordering::Relaxed);
    unsafe { raw_word(ptr) }.store(value.ptr, Ordering::Relaxed);
}

pub unsafe fn load_atomic(ptr: Ptr) -> Ptr {
    check_word_access(ptr, Access::Read);
    let raw = unsafe { raw_word(ptr) }.load(Ordering::Acquire);
    let entry = unsafe { entry_of(ptr) }.map_or(0, |entry| entry.load(Ordering::Acquire));
    if is_box(entry) {
        return unsafe { atomic16::load(box_of(entry)) };
    }
    Ptr { ptr: raw, lower: entry as *mut u8 }
}

pub unsafe fn store_atomic(thread: *mut Thread, ptr: Ptr, value: Ptr) {
    check_word_access(ptr, Access::Write);
    let entry = unsafe { ensure_entry(thread, ptr) };
    let current = entry.load(Ordering::Acquire);
    if is_box(current) {
        unsafe { atomic16::store(box_of(current), value) };
    } else {
        entry.store(new_box(thread, value), Ordering::SeqCst);
    }
    fence(Ordering::Release);
    unsafe { raw_word(ptr) }.store(value.ptr, Ordering::Relaxed);
}

/// The old value. The swap happened if its address equals `expected`'s.
pub unsafe fn strong_cas(thread: *mut Thread, ptr: Ptr, expected: Ptr, new: Ptr) -> Ptr {
    check_word_access(ptr, Access::Write);
    let entry = unsafe { ensure_entry(thread, ptr) };
    let old = loop {
        let current = entry.load(Ordering::Acquire);
        if is_box(current) {
            break unsafe { atomic16::cas(box_of(current), expected, new) };
        }
        let raw = unsafe { raw_word(ptr) }.load(Ordering::Acquire);
        if raw != expected.ptr {
            break Ptr { ptr: raw, lower: current as *mut u8 };
        }
        let boxed = new_box(thread, new);
        if entry.compare_exchange_weak(current, boxed, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            break Ptr { ptr: expected.ptr, lower: current as *mut u8 };
        }
    };
    if old.ptr == expected.ptr {
        unsafe { raw_word(ptr) }.store(new.ptr, Ordering::Release);
    }
    old
}

pub unsafe fn weak_cas(thread: *mut Thread, ptr: Ptr, expected: Ptr, new: Ptr) -> bool {
    check_word_access(ptr, Access::Write);
    let entry = unsafe { ensure_entry(thread, ptr) };
    loop {
        let current = entry.load(Ordering::Acquire);
        if is_box(current) {
            if unsafe { atomic16::cas(box_of(current), expected, new) } != expected {
                return false;
            }
        } else {
            if unsafe { raw_word(ptr) }.load(Ordering::Acquire) != expected.ptr {
                return false;
            }
            let boxed = new_box(thread, new);
            if entry.compare_exchange_weak(current, boxed, Ordering::SeqCst, Ordering::Relaxed).is_err() {
                continue;
            }
        }
        fence(Ordering::Release);
        unsafe { raw_word(ptr) }.store(new.ptr, Ordering::Relaxed);
        return true;
    }
}

pub unsafe fn xchg(thread: *mut Thread, ptr: Ptr, new: Ptr) -> Ptr {
    loop {
        let old = unsafe { load_atomic(ptr) };
        if unsafe { weak_cas(thread, ptr, old, new) } {
            return old;
        }
    }
}
