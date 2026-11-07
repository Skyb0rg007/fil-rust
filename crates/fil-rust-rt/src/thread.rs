//! Threads.
//!
//! A `Thread` is `filc_thread` as compiled code sees it, with a header in front
//! so `zthread_self` can hand out a real capability to it. Compiled code gets
//! it as an argument. The few helpers that don't are the reason `current`
//! exists.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::Cell;
use std::ptr::{self, null, null_mut};
use std::sync::{Mutex, MutexGuard};
use std::sync::atomic::Ordering;

use crate::abi::*;

/// How close to the end of the stack compiled code may get before it fails with
/// a stack overflow. Runtime helpers run in this space.
const STACK_SLACK: usize = 1 << 16;
/// How much memory precedes a `Thread`: room for its header, keeping alignment.
const THREAD_PREFIX: usize = 64;

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

/// Every `Thread`, for the collector to stop.
pub struct ThreadList(pub Vec<usize>);

static THREADS: Mutex<ThreadList> = Mutex::new(ThreadList(Vec::new()));

/// Keeps threads from coming and going. The collector holds this throughout, so
/// nothing that holds it may wait for a collection or reach a safepoint.
pub fn lock_threads() -> MutexGuard<'static, ThreadList> {
    THREADS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lets a `Thread` go when its OS thread does.
struct Detach;

impl Drop for Detach {
    fn drop(&mut self) {
        let thread = CURRENT.with(|current| current.replace(null_mut()));
        if !thread.is_null() {
            unsafe { detach(thread) };
        }
    }
}

thread_local! {
    static DETACH: Detach = const { Detach };
}

/// Frees a thread. Nothing it allocated goes with it: its blocks stay in their
/// chunks, reachable or not.
unsafe fn detach(thread: *mut Thread) {
    // A collection needs the slots to be flushed, or not in the list.
    let mut threads = lock_threads();
    threads.0.retain(|&entry| entry != thread as usize);
    unsafe {
        let this = &mut *thread;
        for slot in &mut this.slots {
            crate::heap::heap().flush_slot(slot);
        }
        drop(threads);
        crate::cc::release_outline(thread);
        drop(Box::from_raw(this.extra));
        dealloc(thread.cast::<u8>().wrapping_sub(THREAD_PREFIX), thread_layout());
    }
}

fn thread_layout() -> Layout {
    Layout::from_size_align(size_of::<Thread>() + THREAD_PREFIX, 64).unwrap()
}

/// Makes a `Thread` for the calling OS thread, starting out exited.
pub fn attach_current() -> *mut Thread {
    let existing = current();
    if !existing.is_null() {
        return existing;
    }
    // The header goes in the padding in front, so the thread keeps its
    // alignment.
    let layout = thread_layout();
    let memory = unsafe { alloc_zeroed(layout) };
    if memory.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let thread = memory.wrapping_add(THREAD_PREFIX).cast::<Thread>();
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
    unsafe { crate::gc::rearm_under_stress(thread) };
    lock_threads().0.push(thread as usize);
    // If the thread is on its way out, this does nothing, and the `Thread`
    // stays around: destructors of Fil-C libraries can run that late.
    let _ = DETACH.try_with(|_| ());
    thread
}

/// The thread is now running Fil-C code and must reach safepoints.
pub unsafe fn enter(thread: *mut Thread) {
    let state = unsafe { &(*thread).state };
    loop {
        let current = state.load(Ordering::Acquire);
        if current & STATE_STOP_REQUESTED != 0 {
            unsafe { crate::gc::wait_for_resume(thread) };
        } else if state
            .compare_exchange_weak(current, current | STATE_ENTERED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

/// The thread is at a safepoint until it enters again.
pub unsafe fn exit(thread: *mut Thread) {
    let before = unsafe { (*thread).state.fetch_and(!STATE_ENTERED, Ordering::AcqRel) };
    if before & STATE_STOP_REQUESTED != 0 {
        crate::gc::note_stopped();
    }
}

/// The thread can't go on until it's dealt with these.
const SAFEPOINT_BITS: u8 = STATE_CHECK_REQUESTED | STATE_STOP_REQUESTED | STATE_DEFERRED_SIGNAL;

/// A safepoint in code that isn't compiled: Rust's calls to a pollcheck, and
/// helpers that compiled code treats as one.
pub unsafe fn poll(thread: *mut Thread) {
    if unsafe { (*thread).state.load(Ordering::Acquire) } & SAFEPOINT_BITS != 0 {
        unsafe { safepoint(thread) };
    }
}

/// A pollcheck found a state bit set.
pub unsafe fn safepoint(thread: *mut Thread) {
    let state = unsafe { &(*thread).state };
    if state.fetch_and(!STATE_CHECK_REQUESTED, Ordering::AcqRel) & STATE_CHECK_REQUESTED != 0 {
        unsafe { crate::gc::check(thread) };
    }
    if state.load(Ordering::Acquire) & STATE_STOP_REQUESTED != 0 {
        unsafe {
            exit(thread);
            enter(thread);
        }
    }
    unsafe { crate::gc::rearm_under_stress(thread) };
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
