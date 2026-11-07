//! Threads.
//!
//! A `Thread` is `filc_thread` as compiled code sees it, with a header in front
//! so `zthread_self` can hand out a real capability to it. Compiled code gets
//! it as an argument. The few helpers that don't are the reason `current`
//! exists.

use std::alloc::{Layout, alloc_zeroed};
use std::cell::Cell;
use std::ptr::{self, null, null_mut};
use std::sync::atomic::Ordering;

use crate::abi::*;

/// How close to the end of the stack compiled code may get before it fails with
/// a stack overflow. Runtime helpers run in this space.
const STACK_SLACK: usize = 1 << 16;

/// The runtime's per-thread state, reached from `Thread::extra`.
pub struct ThreadExtra {
    /// Objects native code is holding, which stay alive until the
    /// `NativeFrame` they were tracked in is popped.
    pub tracked: Vec<*mut Object>,
    /// Bottom of the frame chain, which gives compiled code a `top_frame` to
    /// set an origin on when Rust calls it.
    pub base_frame: Box<Frame>,
    /// Objects of thread-local variables, which live as long as the thread.
    pub thread_locals: Vec<*mut Object>,
}

/// Roots for what a stretch of native code allocates or loads.
pub struct NativeFrame {
    mark: usize,
}

struct SyncOrigin(FunctionOrigin);
unsafe impl Sync for SyncOrigin {}
struct SyncFrameOrigin(Origin);
unsafe impl Sync for SyncFrameOrigin {}

static BASE_FUNCTION_ORIGIN: SyncOrigin = SyncOrigin(FunctionOrigin {
    base: OriginNode {
        function: c"<native>".as_ptr(),
        filename: c"<runtime>".as_ptr(),
        num_lowers_ish: 0,
    },
    personality_getter: null(),
    can_throw: false,
    can_catch: false,
    num_setjmps: 0,
    num_stack_auxes: 0,
});
static BASE_ORIGIN: SyncFrameOrigin = SyncFrameOrigin(Origin {
    origin_node: &BASE_FUNCTION_ORIGIN.0.base,
    line: 0,
    column: 0,
});

thread_local! {
    static CURRENT: Cell<*mut Thread> = const { Cell::new(null_mut()) };
}

/// This OS thread's `Thread`, or null.
pub fn current() -> *mut Thread {
    CURRENT.with(Cell::get)
}

/// This OS thread's `Thread`, which must exist.
pub fn require_current() -> *mut Thread {
    let thread = current();
    if thread.is_null() {
        crate::failure::internal_panic(format_args!("this thread isn't known to the runtime"));
    }
    thread
}

/// The usable low end of this thread's stack.
fn stack_low_end() -> usize {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            crate::failure::internal_panic(format_args!("cannot find this thread's stack"));
        }
        let mut address = null_mut();
        let mut size = 0;
        libc::pthread_attr_getstack(&attr, &mut address, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        address as usize
    }
}

/// Makes a `Thread` for the calling OS thread, starting out exited.
pub fn attach_current() -> *mut Thread {
    let existing = current();
    if !existing.is_null() {
        return existing;
    }
    // The header goes in the padding in front, so the thread keeps its
    // alignment.
    let layout = Layout::from_size_align(size_of::<Thread>() + 64, 64).unwrap();
    let memory = unsafe { alloc_zeroed(layout) };
    if memory.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let thread = memory.wrapping_add(64).cast::<Thread>();
    let lower = thread.cast::<u8>();
    let object = object_for_lower(lower);
    unsafe {
        // No size: nothing may read or write a thread through a capability.
        ptr::write(object, Object {
            upper: lower.into(),
            aux: aux_create(FLAG_GLOBAL | FLAG_READONLY | object_flags(SPECIAL_THREAD, 0), lower).into(),
        });
        let base_frame = Box::new(Frame {
            parent: null_mut(),
            origin: &BASE_ORIGIN.0,
            lowers: [],
        });
        let this = &mut *thread;
        this.top_frame = (&raw const *base_frame).cast_mut();
        this.extra = Box::into_raw(Box::new(ThreadExtra {
            tracked: Vec::new(),
            base_frame,
            thread_locals: Vec::new(),
        }));
        this.stack_limit = (stack_low_end() + STACK_SLACK) as *mut u8;
        this.tid = libc::gettid() as u32;
        for (class, slot) in this.slots.iter_mut().enumerate() {
            slot.class = class;
        }
    }
    CURRENT.with(|current| current.set(thread));
    thread
}

/// The thread is now running Fil-C code and must reach safepoints.
pub unsafe fn enter(thread: *mut Thread) {
    unsafe { (*thread).state.fetch_or(STATE_ENTERED, Ordering::AcqRel) };
}

/// The thread is at a safepoint until it enters again.
pub unsafe fn exit(thread: *mut Thread) {
    unsafe { (*thread).state.fetch_and(!STATE_ENTERED, Ordering::AcqRel) };
}

/// Compiled code reached a pollcheck with a state bit set. There's nothing to
/// do here until the collector can request one.
pub unsafe fn safepoint(thread: *mut Thread) {
    unsafe {
        exit(thread);
        enter(thread);
    }
}

pub unsafe fn extra<'a>(thread: *mut Thread) -> &'a mut ThreadExtra {
    unsafe { &mut *(*thread).extra }
}

impl NativeFrame {
    pub unsafe fn push(thread: *mut Thread) -> NativeFrame {
        NativeFrame { mark: unsafe { extra(thread) }.tracked.len() }
    }

    pub unsafe fn pop(self, thread: *mut Thread) {
        unsafe { extra(thread) }.tracked.truncate(self.mark);
    }
}

/// Keeps `object` alive until the current `NativeFrame` is popped.
pub unsafe fn track(thread: *mut Thread, object: *mut Object) {
    if !object.is_null() {
        unsafe { extra(thread) }.tracked.push(object);
    }
}
