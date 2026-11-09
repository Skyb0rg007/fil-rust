//! `setjmp` and `longjmp`.
//!
//! Compiled code calls `filc_jmp_buf_recreate` for a buffer, then the host's
//! `_setjmp` on it. The frame keeps that buffer in a slot of its own, which is
//! how `zlongjmp` knows the destination is still on the stack. A `jmp_buf` in
//! Fil-C is only a capability for this object.

use std::ffi::c_int;

use crate::abi::*;
use crate::failure::safety_panic;
use crate::natives::natives;
use crate::object;
use crate::thread::extra;
use std::sync::atomic::{AtomicBool, Ordering};

const KIND_SETJMP: c_int = 0;
const KIND_SIGSETJMP: c_int = 2;

/// Whether `setjmp` also saves the signal mask, as `sigsetjmp` can.
static SETJMP_SAVES_SIGMASK: AtomicBool = AtomicBool::new(false);

#[repr(C)]
pub struct JmpBuf {
    /// Has to come first: compiled code passes this object to `_setjmp`. As big
    /// as any host's `jmp_buf`.
    system_buf: [u64; 64],
    did_save_sigmask: bool,
    sigmask: libc::sigset_t,
    kind: c_int,
    saved_top_frame: *mut Frame,
    saved_tracked: usize,
    saved_signal_deferral_depth: u64,
    num_lowers: usize,
    lowers: [*mut u8; 0],
}

unsafe extern "C" {
    fn _longjmp(buffer: *mut JmpBuf, value: c_int) -> !;
}

unsafe fn frame_lowers(frame: *mut Frame) -> *mut *mut u8 {
    unsafe { (&raw mut (*frame).lowers).cast() }
}

/// What the collector follows from a buffer: these stay live for as long as
/// the buffer does, since a jump puts them back.
pub unsafe fn for_each_lower(buffer: *mut JmpBuf, mut visit: impl FnMut(*mut u8)) {
    unsafe {
        let lowers = (&raw const (*buffer).lowers).cast::<*mut u8>();
        for index in 0..(*buffer).num_lowers {
            visit(*lowers.add(index));
        }
    }
}

/// The buffer for one `setjmp` of the top frame, kept in `slot`. Running it
/// again overwrites the old state, so nothing can jump back in time.
pub unsafe fn recreate(thread: *mut Thread, slot: *mut *mut JmpBuf, kind: c_int, value: c_int) -> *mut JmpBuf {
    unsafe {
        let frame = (*thread).top_frame;
        let origin = &*function_origin((*frame).origin);
        let count = origin.base.num_lowers_ish as usize;
        assert_eq!(origin.num_stack_auxes, 0, "a function that calls setjmp has no stack auxes");
        let tracked = extra(thread).tracked.len();
        let mut buffer = *slot;
        if buffer.is_null() {
            let size = std::mem::offset_of!(JmpBuf, lowers) + count * size_of::<*mut u8>();
            let object = object::allocate_special(thread, size, SPECIAL_JMP_BUF);
            buffer = lower_for_object(object).cast();
            buffer.cast::<u8>().write_bytes(0, size);
            *slot = buffer;
            (*buffer).kind = kind;
            (*buffer).saved_top_frame = frame;
            (*buffer).saved_tracked = tracked;
            (*buffer).num_lowers = count;
        } else {
            assert!((*buffer).kind == kind && (*buffer).saved_top_frame == frame);
            assert!((*buffer).saved_tracked == tracked && (*buffer).num_lowers == count);
        }
        (*buffer).saved_signal_deferral_depth = (*thread).signal_deferral_depth;
        let saved = (&raw mut (*buffer).lowers).cast::<*mut u8>();
        std::ptr::copy_nonoverlapping(frame_lowers(frame), saved, count);
        if (kind == KIND_SIGSETJMP && value != 0) || (kind == KIND_SETJMP && SETJMP_SAVES_SIGMASK.load(Ordering::Relaxed)) {
            libc::pthread_sigmask(0, std::ptr::null(), &mut (*buffer).sigmask);
            (*buffer).did_save_sigmask = true;
        }
        buffer
    }
}

/// Whether a frame still running on `thread` owns `buffer`.
unsafe fn is_on_stack(thread: *mut Thread, buffer: *mut JmpBuf) -> bool {
    unsafe {
        let mut frame = (*thread).top_frame;
        while !frame.is_null() {
            let origin = &*function_origin((*frame).origin);
            let count = origin.base.num_lowers_ish as usize;
            for index in count - origin.num_setjmps as usize..count {
                if *frame_lowers(frame).add(index) == buffer.cast() {
                    return true;
                }
            }
            frame = (*frame).parent;
        }
        false
    }
}

fn is_setjmp_lower(origin: &FunctionOrigin, index: usize) -> bool {
    index >= origin.base.num_lowers_ish as usize - origin.num_setjmps as usize
}

/// Jumps to where `buffer`, a capability for the runtime's buffer, was set.
pub(crate) unsafe fn longjmp(thread: *mut Thread, buffer: Ptr, value: c_int) -> ! {
    object::check_special(buffer, SPECIAL_JMP_BUF, "jmp_buf");
    unsafe {
        let buffer = buffer.ptr.cast::<JmpBuf>();
        if !is_on_stack(thread, buffer) {
            safety_panic(format_args!("cannot longjmp unless the setjmp destination is on the stack."));
        }
        // Nothing here has a destructor: the jump drops this frame too.
        let frame = (*buffer).saved_top_frame;
        (*thread).top_frame = frame;
        extra(thread).tracked.truncate((*buffer).saved_tracked);
        let origin = &*function_origin((*frame).origin);
        let saved = (&raw const (*buffer).lowers).cast::<*mut u8>();
        for index in 0..(*buffer).num_lowers {
            if !is_setjmp_lower(origin, index) {
                *frame_lowers(frame).add(index) = *saved.add(index);
            }
        }
        (*thread).signal_deferral_depth = (*buffer).saved_signal_deferral_depth;
        crate::signal::after_deferral_decrease(thread);
        if (*buffer).did_save_sigmask {
            libc::pthread_sigmask(libc::SIG_SETMASK, &(*buffer).sigmask, std::ptr::null_mut());
        }
        _longjmp(buffer, value)
    }
}

natives! {
    fn zlongjmp(thread: *mut Thread, buffer: Ptr, value: c_int) -> () {
        unsafe { longjmp(thread, buffer, value) }
    }

    fn zget_jmp_buf_impl_frame(thread: *mut Thread, buffer: Ptr) -> Ptr {
        let _ = thread;
        object::check_special(buffer, SPECIAL_JMP_BUF, "jmp_buf");
        let frame = unsafe { (*buffer.ptr.cast::<JmpBuf>()).saved_top_frame };
        Ptr { ptr: frame.cast(), lower: std::ptr::null_mut() }
    }

    fn zmake_setjmp_save_sigmask(thread: *mut Thread, save: bool) -> () {
        let _ = thread;
        SETJMP_SAVES_SIGMASK.store(save, Ordering::Relaxed);
    }
}
