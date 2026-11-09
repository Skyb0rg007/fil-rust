//! `mmap` and what goes with it.
//!
//! A mapping always lands in a heap block, so an address can't be reused while
//! something refers to it. Fil-C code gets the block as a capability and the
//! kernel puts the mapping on top. Unmapping puts ordinary memory back, rather
//! than leaving a hole for the next `mmap` of anyone in the process to fall into,
//! and the collector does the same for a mapping nothing refers to any more.

use std::ffi::{c_int, c_void};
use std::ptr::null_mut;

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::gc;
use crate::natives::natives;
use crate::object::{self, Access};
use crate::glibc::set_errno;
use crate::sys::{one_out, plain, sys};
use crate::thread;

const PAGE_SIZE: usize = 4096;

fn round_to_page(size: usize) -> usize {
    size.wrapping_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

fn is_page_aligned(value: usize) -> bool {
    value & (PAGE_SIZE - 1) == 0
}

fn failed() -> Ptr {
    Ptr { ptr: usize::MAX as *mut u8, lower: null_mut() }
}

unsafe fn fail(thread: *mut Thread, error: c_int) -> Ptr {
    unsafe { set_errno(thread, error) };
    failed()
}

/// Fails unless `ptr` points into a mapping that's still there.
fn check_mapped(ptr: Ptr) {
    let flags = if ptr.lower.is_null() { 0 } else { unsafe { &*ptr.object() }.flags() };
    if flags & FLAG_MMAP == 0 {
        safety_panic(format_args!("cannot perform this operation on something that was not mmapped (ptr = {:p}).", ptr.ptr));
    }
    if flags & FLAG_FREE != 0 {
        safety_panic(format_args!("cannot perform this operation on a free object (ptr = {:p}).", ptr.ptr));
    }
}

/// A new block for a mapping of `length` bytes. It's ordinary memory until the
/// mapping is made, and it's destructed like a mapping from the start, since
/// the block can't tell what happened to it.
unsafe fn allocate(thread: *mut Thread, length: usize) -> Ptr {
    unsafe {
        let object = object::allocate_aligned(thread, length, PAGE_SIZE, FLAG_MMAP);
        thread::track(thread, object);
        gc::add_destructible(object);
        let lower = lower_for_object(object);
        Ptr { ptr: lower, lower }
    }
}

/// Puts ordinary memory where a mapping was.
pub unsafe fn unmap(address: *mut u8, length: usize) {
    if length == 0 {
        return;
    }
    let result = unsafe {
        libc::mmap(
            address.cast(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if result != address.cast() {
        internal_panic(format_args!("couldn't unmap {length} bytes at {address:p}"));
    }
}

/// What the collector does for a mapping nothing refers to.
pub unsafe fn destruct(object: *mut Object) {
    let this = unsafe { &*object };
    if this.flags() & FLAG_FREE == 0 {
        let lower = lower_for_object(object);
        unsafe { unmap(lower, this.upper() as usize - lower as usize) };
    }
}

/// Whether `advice` leaves the contents of the memory as they are.
fn is_transparent(advice: c_int) -> bool {
    matches!(
        advice,
        libc::MADV_NORMAL
            | libc::MADV_RANDOM
            | libc::MADV_SEQUENTIAL
            | libc::MADV_WILLNEED
            | libc::MADV_MERGEABLE
            | libc::MADV_UNMERGEABLE
            | libc::MADV_SOFT_OFFLINE
            | libc::MADV_HUGEPAGE
            | libc::MADV_NOHUGEPAGE
            | libc::MADV_COLLAPSE
            | libc::MADV_DONTDUMP
            | libc::MADV_DODUMP
            | libc::MADV_COLD
            | libc::MADV_PAGEOUT
    )
}

fn check_advice(advice: c_int) {
    const GUARD_INSTALL: c_int = 102;
    const GUARD_REMOVE: c_int = 103;
    match advice {
        libc::MADV_DONTNEED
        | libc::MADV_REMOVE
        | libc::MADV_DOFORK
        | libc::MADV_WIPEONFORK
        | libc::MADV_POPULATE_READ
        | libc::MADV_POPULATE_WRITE
        | libc::MADV_FREE => {}
        libc::MADV_DONTFORK => safety_panic(format_args!("attempted to use MADV_DONTFORK.")),
        libc::MADV_HWPOISON => safety_panic(format_args!("attempted to use MADV_HWPOISON.")),
        GUARD_INSTALL => safety_panic(format_args!("attempted to use MADV_GUARD_INSTALL.")),
        GUARD_REMOVE => safety_panic(format_args!("attempted to use MADV_GUARD_REMOVE.")),
        _ if is_transparent(advice) => {}
        _ => safety_panic(format_args!("attempting to use unrecognized madvise advice {advice}.")),
    }
}

/// What's common to `mlock` and `munlock`. Locking memory that wasn't mapped
/// is an error rather than a failed check, as it is on Linux.
unsafe fn lock(thread: *mut Thread, address: Ptr, length: usize, call: impl FnOnce(*const c_void, usize) -> c_int) -> c_int {
    if length != 0 {
        object::check_access(address, length, Access::Read);
        if unsafe { &*address.object() }.flags() & FLAG_MMAP == 0 {
            unsafe { set_errno(thread, libc::ENOMEM) };
            return -1;
        }
    }
    unsafe { sys(thread, || call(address.ptr.cast(), length)) }
}

/// The mapping of `new_size` bytes that replaces `old`, at `new`. An `mremap`
/// that takes the old pages away would leave a hole, so this asks for a second
/// mapping of the same pages, which is possible only for shared ones, and puts
/// ordinary memory under the old ones.
unsafe fn move_mapping(old: *mut u8, old_size: usize, new: *mut u8, new_size: usize) -> *mut c_void {
    unsafe {
        let flags = libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED;
        let result = libc::mremap(old.cast(), 0, new_size, flags, new);
        if result != libc::MAP_FAILED {
            unmap(old, old_size);
        } else if *libc::__errno_location() == libc::EINVAL {
            // A private mapping. It would take a new mapping and a copy, and a
            // way of finding the file for a private file mapping.
            *libc::__errno_location() = libc::ENOSYS;
        }
        result
    }
}

/// `mremap`: `new` only counts with `MREMAP_FIXED`.
pub(crate) unsafe fn remap(thread: *mut Thread, old: Ptr, old_size: usize, new_size: usize, flags: c_int, new: Ptr) -> Ptr {
    let known = libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED | libc::MREMAP_DONTUNMAP;
    let moves = flags & libc::MREMAP_MAYMOVE != 0;
    let fixed = flags & libc::MREMAP_FIXED != 0;
    if !is_page_aligned(old.ptr as usize) || !is_page_aligned(new.ptr as usize) || flags & !known != 0 {
        return unsafe { fail(thread, libc::EINVAL) };
    }
    if flags & libc::MREMAP_DONTUNMAP != 0 {
        return unsafe { fail(thread, libc::ENOSYS) };
    }
    if (old_size == 0 && !moves) || new_size == 0 {
        return unsafe { fail(thread, libc::EINVAL) };
    }
    let old_size = round_to_page(old_size);
    let new_size = round_to_page(new_size);
    // Even a size of zero asks for a page, which has to be a mapping's.
    object::check_access(old, old_size.max(PAGE_SIZE), Access::Write);
    check_mapped(old);
    let new = if fixed {
        if !moves {
            return unsafe { fail(thread, libc::EINVAL) };
        }
        object::check_access(new, new_size, Access::Write);
        check_mapped(new);
        new
    } else if moves {
        unsafe { allocate(thread, new_size) }
    } else if new_size > old.available() {
        return unsafe { fail(thread, libc::ENOMEM) };
    } else {
        old
    };
    let result = unsafe {
        sys(thread, || {
            if moves {
                move_mapping(old.ptr, old_size, new.ptr, new_size)
            } else if new_size > old_size {
                // This only gets here after shrinking, so ordinary memory
                // follows and it fails, as it does upstream.
                libc::mremap(old.ptr.cast(), old_size, new_size, 0)
            } else {
                unmap(old.ptr.add(new_size), old_size - new_size);
                old.ptr.cast()
            }
        })
    };
    if result == libc::MAP_FAILED {
        return failed();
    }
    object::check_access(new, new_size, Access::Write);
    new
}

natives! {
    fn zsys_mmap(thread: *mut Thread, address: Ptr, length: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> Ptr {
        let length = round_to_page(length);
        if !address.ptr.is_null() {
            object::check_access(address, length, Access::Write);
            check_mapped(address);
            if flags & libc::MAP_FIXED == 0 {
                return unsafe { fail(thread, libc::EINVAL) };
            }
        } else if flags & libc::MAP_FIXED != 0 {
            return unsafe { fail(thread, libc::EINVAL) };
        }
        let shared = flags & libc::MAP_SHARED != 0;
        let private = flags & libc::MAP_PRIVATE != 0;
        if prot & libc::PROT_EXEC != 0 || shared == private {
            return unsafe { fail(thread, libc::EINVAL) };
        }
        if address.ptr.is_null() {
            address = unsafe { allocate(thread, length) };
            flags |= libc::MAP_FIXED;
        }
        let result = unsafe { sys(thread, || libc::mmap(address.ptr.cast(), length, prot, flags, fd, offset)) };
        if result == libc::MAP_FAILED {
            return failed();
        }
        // Someone may have unmapped it in the meantime.
        object::check_access(address, length, Access::Write);
        address
    }

    fn zsys_munmap(thread: *mut Thread, address: Ptr, length: usize) -> c_int {
        if length == 0 {
            return 0;
        }
        let length = round_to_page(length);
        object::check_access(address, length, Access::Write);
        check_mapped(address);
        if !is_page_aligned(address.ptr as usize) {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        if address.ptr == address.lower && length == address.available() {
            unsafe { object::free_mapping(address) };
        }
        unsafe { plain(thread, || unmap(address.ptr, length)) };
        0
    }

    fn zsys_mprotect(thread: *mut Thread, address: Ptr, length: usize, prot: c_int) -> c_int {
        if prot & libc::PROT_EXEC != 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        object::check_access(address, length, Access::Write);
        check_mapped(address);
        unsafe { sys(thread, || libc::mprotect(address.ptr.cast(), length, prot)) }
    }

    fn zsys_remap_file_pages(thread: *mut Thread, address: Ptr, length: usize, prot: c_int, offset: usize, flags: c_int) -> c_int {
        if prot & libc::PROT_EXEC != 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        object::check_access(address, length, Access::Write);
        check_mapped(address);
        unsafe { sys(thread, || libc::syscall(libc::SYS_remap_file_pages, address.ptr, length, prot, offset, flags) as c_int) }
    }

    fn zsys_madvise(thread: *mut Thread, address: Ptr, length: usize, advice: c_int) -> c_int {
        if !is_transparent(advice) {
            object::check_access(address, length, Access::Write);
            check_mapped(address);
            check_advice(advice);
        }
        unsafe { sys(thread, || libc::madvise(address.ptr.cast(), length, advice)) }
    }

    // None of this changes anything in the memory, so there's nothing to check.
    // At worst the kernel turns the address down.
    fn zsys_msync(thread: *mut Thread, address: Ptr, length: usize, flags: c_int) -> c_int {
        unsafe { sys(thread, || libc::msync(address.ptr.cast(), length, flags)) }
    }

    fn zsys_mincore(thread: *mut Thread, address: Ptr, length: usize, result: Ptr) -> c_int {
        object::check_access(result, round_to_page(length) / PAGE_SIZE, Access::Write);
        unsafe { sys(thread, || libc::mincore(address.ptr.cast(), length, result.ptr)) }
    }

    fn zsys_mlock(thread: *mut Thread, address: Ptr, length: usize) -> c_int {
        unsafe { lock(thread, address, length, |address, length| libc::mlock(address, length)) }
    }

    fn zsys_munlock(thread: *mut Thread, address: Ptr, length: usize) -> c_int {
        unsafe { lock(thread, address, length, |address, length| libc::munlock(address, length)) }
    }

    fn zsys_mremap(thread: *mut Thread, old: Ptr, old_size: usize, new_size: usize, flags: c_int, new: Ptr) -> Ptr {
        unsafe { remap(thread, old, old_size, new_size, flags, new) }
    }

    fn zsys_shmget(thread: *mut Thread, key: c_int, size: usize, flags: c_int) -> c_int {
        unsafe { sys(thread, || libc::shmget(key, size, flags)) }
    }

    fn zsys_shmctl(thread: *mut Thread, id: c_int, command: c_int, buffer: Ptr) -> c_int {
        let buffer = if buffer.ptr.is_null() { null_mut() } else { one_out::<libc::shmid_ds>(buffer) };
        unsafe { sys(thread, || libc::shmctl(id, command, buffer)) }
    }

    fn zsys_shmat(thread: *mut Thread, id: c_int, address: Ptr, flags: c_int) -> Ptr {
        if !address.ptr.is_null() {
            return unsafe { fail(thread, libc::EINVAL) };
        }
        // The size can't be relied on unless the segment stays, so this holds
        // an attachment to it for as long as it matters.
        let held = unsafe { sys(thread, || libc::shmat(id, null_mut(), libc::SHM_RDONLY)) };
        if held as isize == -1 {
            return failed();
        }
        let mut status: libc::shmid_ds = unsafe { std::mem::zeroed() };
        let mut result = failed();
        if unsafe { sys(thread, || libc::shmctl(id, libc::IPC_STAT, &mut status)) } == 0 {
            let length = round_to_page(status.shm_segsz);
            let mapping = unsafe { allocate(thread, length) };
            let attached = unsafe { sys(thread, || libc::shmat(id, mapping.ptr.cast(), flags | libc::SHM_REMAP)) };
            if attached == mapping.ptr.cast() {
                object::check_access(mapping, length, Access::Write);
                result = mapping;
            }
        }
        unsafe { plain(thread, || libc::shmdt(held)) };
        result
    }

    fn zsys_shmdt(thread: *mut Thread, address: Ptr) -> c_int {
        if address.ptr != address.lower {
            safety_panic(format_args!("attempt to shmdt a pointer not to the base of the capability (ptr = {:p}).", address.ptr));
        }
        let available = address.available();
        if available == 0 {
            safety_panic(format_args!("attempt to shmdt a pointer with zero bytes available (ptr = {:p}).", address.ptr));
        }
        object::check_access(address, available, Access::Write);
        check_mapped(address);
        unsafe {
            object::free_mapping(address);
            plain(thread, || unmap(address.ptr, available));
        }
        0
    }
}
