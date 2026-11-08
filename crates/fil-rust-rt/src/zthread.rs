//! Threads that Fil-C code creates: `zthread_*`, which `pthread_create` in the
//! Fil-C libc is built on.
//!
//! Compiled frames can't be unwound by the host's `pthread_exit`, whose forced
//! unwinding would run through them looking for cleanups. `zthread_exit` jumps
//! back to where the thread started instead, and it returns from there.

use std::ffi::{c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::natives::natives;
use crate::object::{self, Access};
use crate::start::set_errno;
use crate::thread::{self, NativeFrame, extra};
use crate::{globals, ptrs};

#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    ".text",
    ".hidden fr_call_escapable",
    ".type fr_call_escapable, @function",
    "fr_call_escapable:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov [rdx], rsp",
    "mov rax, rdi",
    "mov rdi, rsi",
    "call rax",
    "2:",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".hidden fr_escape",
    ".type fr_escape, @function",
    "fr_escape:",
    "mov rsp, rdi",
    "jmp 2b",
);

#[cfg(target_arch = "aarch64")]
std::arch::global_asm!(
    ".text",
    ".hidden fr_call_escapable",
    ".type fr_call_escapable, %function",
    "fr_call_escapable:",
    "stp x29, x30, [sp, #-160]!",
    "stp x19, x20, [sp, #16]",
    "stp x21, x22, [sp, #32]",
    "stp x23, x24, [sp, #48]",
    "stp x25, x26, [sp, #64]",
    "stp x27, x28, [sp, #80]",
    "stp d8, d9, [sp, #96]",
    "stp d10, d11, [sp, #112]",
    "stp d12, d13, [sp, #128]",
    "stp d14, d15, [sp, #144]",
    "mov x3, sp",
    "str x3, [x2]",
    "mov x3, x0",
    "mov x0, x1",
    "blr x3",
    "2:",
    "ldp d14, d15, [sp, #144]",
    "ldp d12, d13, [sp, #128]",
    "ldp d10, d11, [sp, #112]",
    "ldp d8, d9, [sp, #96]",
    "ldp x27, x28, [sp, #80]",
    "ldp x25, x26, [sp, #64]",
    "ldp x23, x24, [sp, #48]",
    "ldp x21, x22, [sp, #32]",
    "ldp x19, x20, [sp, #16]",
    "ldp x29, x30, [sp], #160",
    "ret",
    ".hidden fr_escape",
    ".type fr_escape, %function",
    "fr_escape:",
    "mov sp, x0",
    "b 2b",
);

unsafe extern "C" {
    /// Calls `body(argument)`, leaving in `escape` what [`fr_escape`] needs to
    /// make this call return, however deep it's gotten by then. The frames in
    /// between are compiled code and functions with nothing to drop.
    fn fr_call_escapable(body: unsafe extern "C" fn(*mut c_void), argument: *mut c_void, escape: *mut usize);
    fn fr_escape(escape: usize) -> !;
}

/// What a new thread needs from the one creating it, which stays exited until
/// `started` is set. Its native frame is what keeps these alive until then.
struct Start {
    function: Ptr,
    argument: Ptr,
    zthread: Ptr,
    tid: Ptr,
    signal_mask: libc::sigset_t,
    started: Mutex<bool>,
    changed: Condvar,
}

#[derive(Clone, Copy)]
struct Running {
    thread: *mut Thread,
    start: *const Start,
}

/// The thread is done: its frames are about to be gone, and its result is what
/// `zthread_join` will see.
unsafe fn leave(thread: *mut Thread, result: Ptr) -> ! {
    unsafe {
        (*thread).result = result;
        (*thread).top_frame = null_mut();
        let extra = extra(thread);
        extra.tracked.clear();
        let escape = extra.escape;
        thread::exit(thread);
        if escape == 0 {
            internal_panic(format_args!("zthread_exit on a thread Fil-C code didn't create"));
        }
        fr_escape(escape)
    }
}

unsafe extern "C" fn run(argument: *mut c_void) {
    let Running { thread, start } = unsafe { argument.cast::<Running>().read() };
    unsafe {
        thread::enter(thread);
        let frame = NativeFrame::push(thread);
        let start = &*start;
        let (function, argument) = (start.function, start.argument);
        thread::track(thread, function.object());
        thread::track(thread, argument.object());
        if !start.zthread.ptr.is_null() {
            ptrs::store(thread, start.zthread, Ptr::for_object(object_for_lower(thread.cast())));
        }
        if !start.tid.ptr.is_null() {
            object::check_access(start.tid, size_of::<u32>(), Access::Write);
            start.tid.ptr.cast::<u32>().write((*thread).tid);
        }
        libc::pthread_sigmask(libc::SIG_SETMASK, &start.signal_mask, null_mut());
        *start.started.lock().unwrap_or_else(|e| e.into_inner()) = true;
        // The creator may go on as soon as it can see this.
        start.changed.notify_all();

        let mut results = [Ptr::NULL];
        crate::call(thread, function, &[argument], &mut results);
        frame.pop(thread);
        leave(thread, results[0]);
    }
}

extern "C" fn thread_entry(start: *mut c_void) -> *mut c_void {
    let thread = thread::attach_new_thread();
    let mut running = Running { thread, start: start.cast() };
    unsafe {
        fr_call_escapable(run, (&raw mut running).cast(), &raw mut extra(thread).escape);
        thread::finish_current(thread);
    }
    null_mut()
}

fn check_zthread(ptr: Ptr) {
    object::check_special(ptr, SPECIAL_THREAD, "zthread");
}

/// The signals a thread comes up with: none until it knows what it is.
unsafe fn block_all_signals() -> libc::sigset_t {
    unsafe {
        let mut all = MaybeUninit::uninit();
        let mut old = MaybeUninit::uninit();
        libc::sigfillset(all.as_mut_ptr());
        libc::pthread_sigmask(libc::SIG_BLOCK, all.as_ptr(), old.as_mut_ptr());
        old.assume_init()
    }
}

natives! {
    fn zthread_self(thread: *mut Thread) -> Ptr {
        Ptr::for_object(object_for_lower(thread.cast()))
    }

    fn zthread_get_id(thread: *mut Thread, zthread: Ptr) -> u32 {
        check_zthread(zthread);
        unsafe { (*zthread.ptr.cast::<Thread>()).tid }
    }

    fn zthread_self_id(thread: *mut Thread) -> u32 {
        unsafe { (*thread).tid }
    }

    fn zthread_get_cookie(thread: *mut Thread, zthread: Ptr) -> Ptr {
        check_zthread(zthread);
        unsafe { (*zthread.ptr.cast::<Thread>()).cookie }
    }

    fn zthread_set_self_cookie(thread: *mut Thread, cookie: Ptr) -> () {
        unsafe { (*thread).cookie = cookie };
    }

    fn zthread_create2(thread: *mut Thread, function: Ptr, argument: Ptr, zthread: Ptr, tid: Ptr) -> bool {
        if !globals::ctors_started() {
            safety_panic(format_args!("cannot create threads before global constructors have started being run."));
        }
        object::check_function_call(function);
        unsafe {
            thread::exit(thread);
            let signal_mask = block_all_signals();
            let start = Start {
                function,
                argument,
                zthread,
                tid,
                signal_mask,
                started: Mutex::new(false),
                changed: Condvar::new(),
            };
            let mut attributes = MaybeUninit::uninit();
            libc::pthread_attr_init(attributes.as_mut_ptr());
            libc::pthread_attr_setdetachstate(attributes.as_mut_ptr(), libc::PTHREAD_CREATE_DETACHED);
            let mut ignored = MaybeUninit::uninit();
            let result = libc::pthread_create(
                ignored.as_mut_ptr(),
                attributes.as_ptr(),
                thread_entry,
                (&raw const start).cast_mut().cast(),
            );
            libc::pthread_attr_destroy(attributes.as_mut_ptr());
            libc::pthread_sigmask(libc::SIG_SETMASK, &signal_mask, null_mut());
            if result == 0 {
                let mut started = start.started.lock().unwrap_or_else(|e| e.into_inner());
                while !*started {
                    started = start.changed.wait(started).unwrap_or_else(|e| e.into_inner());
                }
            }
            thread::enter(thread);
            if result != 0 {
                set_errno(thread, result);
            }
            result == 0
        }
    }

    fn zthread_exit(thread: *mut Thread, result: Ptr) -> () {
        unsafe { leave(thread, result) };
    }

    fn zthread_join(thread: *mut Thread, zthread: Ptr, result: Ptr) -> bool {
        check_zthread(zthread);
        let target = zthread.ptr.cast::<Thread>();
        if unsafe { (*target).forked.load(Ordering::Relaxed) } != 0 {
            unsafe { set_errno(thread, libc::ESRCH) };
            return false;
        }
        unsafe {
            thread::exit(thread);
            thread::wait_until_finished(target);
            thread::enter(thread);
            if !result.ptr.is_null() {
                ptrs::store(thread, result, (*target).result);
            }
        }
        true
    }

    fn zthread_kill(thread: *mut Thread, zthread: Ptr, signal: c_int) -> bool {
        if crate::sys::is_internal_signal(signal) {
            unsafe { set_errno(thread, libc::ENOSYS) };
            return false;
        }
        check_zthread(zthread);
        let target = zthread.ptr.cast::<Thread>();
        unsafe {
            thread::exit(thread);
            let blocked = crate::signal::Blocked::all();
            let guard = thread::lock_lifecycle();
            let host = (*target).host;
            let error = if host != 0 { libc::pthread_kill(host as libc::pthread_t, signal) } else { libc::ESRCH };
            drop(guard);
            drop(blocked);
            thread::enter(thread);
            if error != 0 {
                set_errno(thread, error);
            }
            error == 0
        }
    }

    fn zincrement_signal_deferral_depth(thread: *mut Thread) -> () {
        unsafe { crate::signal::hold_deferred(thread) };
    }

    fn zdecrement_signal_deferral_depth(thread: *mut Thread) -> () {
        unsafe {
            if (*thread).signal_deferral_depth == 0 {
                safety_panic(format_args!("attempt to decrement the signal deferral depth below zero."));
            }
            crate::signal::allow_deferred(thread);
            thread::poll(thread);
        }
    }

    fn zget_signal_deferral_depth(thread: *mut Thread) -> u64 {
        unsafe { (*thread).signal_deferral_depth }
    }

    fn zthread_stack_limit(thread: *mut Thread, zthread: Ptr) -> Ptr {
        check_zthread(zthread);
        Ptr { ptr: unsafe { (*zthread.ptr.cast::<Thread>()).stack_low }, lower: null::<u8>().cast_mut() }
    }

    fn zthread_stack_top(thread: *mut Thread, zthread: Ptr) -> Ptr {
        check_zthread(zthread);
        Ptr { ptr: unsafe { (*zthread.ptr.cast::<Thread>()).stack_top }, lower: null::<u8>().cast_mut() }
    }
}
