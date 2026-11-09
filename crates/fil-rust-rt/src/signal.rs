//! Signals.
//!
//! A handler is compiled code, so it can't run just anywhere. If a signal lands
//! on a thread that's running Fil-C code, it goes on a queue, and a state bit
//! sends the thread to a safepoint. If the thread is exited, the handler runs
//! there and then, having entered: all that's interrupted is a system call or
//! Rust code that isn't touching the heap.
//!
//! That only works if a thread that's exited never holds a lock a handler could
//! want. So the few that do have signals blocked: see [`Blocked`].
//!
//! A process-directed signal can land on a Rust thread that has never seen
//! Fil-C. The main Fil-C thread takes it instead, as it can always run a
//! handler. Faults aren't in this: Fil-C doesn't let programs handle them, so
//! Rust's stack overflow handler stays in place.
//!
//! Sending the signal itself on to the main thread would leave it waiting there
//! whenever that thread has it blocked for a moment, as in a handler for it or
//! a collection, and by the time it got through the program could have reset it
//! to `SIG_DFL` and been killed. So a forwarded signal is written down where it
//! landed, and the main thread is only woken, with [`WAKE`], to run its handler
//! from there. Unless the program has the signal blocked on the main thread:
//! then it's sent on as it is, to wait as a blocked signal should, for
//! `sigwait` or for the program to unblock it.

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::abi::*;
use crate::failure::internal_panic;
use crate::natives::natives;
use crate::object::{self, Access};
use crate::start::set_errno;
use crate::sys::{is_internal_signal, one_in, plain, sys, sys_result, user_sigset};
use crate::thread::{self, NativeFrame, extra};
use crate::ptrs;

const MAX_SIGNAL: c_int = 64;
const INLINE_QUEUE_SIZE: usize = 16;
const CHUNK_SIZE: usize = 256;

/// What `sigaction` was given, for the handler to find.
#[repr(C)]
pub struct Handler {
    pub function: Ptr,
    mask: libc::sigset_t,
    flags: c_int,
}

static TABLE: [AtomicPtr<Handler>; MAX_SIGNAL as usize + 1] = [const { AtomicPtr::new(null_mut()) }; MAX_SIGNAL as usize + 1];

/// Where signals go that land on threads without a `Thread`.
static MAIN_TID: AtomicI32 = AtomicI32::new(0);
static MAIN_THREAD: AtomicPtr<Thread> = AtomicPtr::new(null_mut());

/// The main thread's signal mask, as the program last set it: bit `n - 1` for
/// signal `n`.
static MAIN_MASK: AtomicU64 = AtomicU64::new(0);

/// Wakes the main thread for a signal that was forwarded. It's musl's
/// `SIGSYNCCALL`, which programs can't send, handle or block. The host's glibc
/// doesn't use it, but it's what glibc gives as `SIGRTMIN`, so Rust code in the
/// same process mustn't use that.
pub const WAKE: c_int = 34;

/// Whether [`WAKE`] restarts system calls, which follows the handler of the
/// signal it was last sent for. 2 until it's been set.
static WAKE_RESTARTS: AtomicU32 = AtomicU32::new(2);

/// Signals written down for the main thread: how many, and the last one's
/// `siginfo`, which whoever holds `busy` can touch.
struct Forwarded {
    count: AtomicUsize,
    busy: AtomicBool,
    has_info: AtomicBool,
    info: UnsafeCell<MaybeUninit<libc::siginfo_t>>,
}

unsafe impl Sync for Forwarded {}

static FORWARDED: [Forwarded; MAX_SIGNAL as usize + 1] = [const {
    Forwarded {
        count: AtomicUsize::new(0),
        busy: AtomicBool::new(false),
        has_info: AtomicBool::new(false),
        info: UnsafeCell::new(MaybeUninit::uninit()),
    }
}; MAX_SIGNAL as usize + 1];

static ANY_FORWARDED: AtomicUsize = AtomicUsize::new(0);

fn mask_bits(set: &libc::sigset_t) -> u64 {
    (1..=MAX_SIGNAL).filter(|&signal| unsafe { libc::sigismember(set, signal) } == 1).fold(0, |bits, signal| bits | 1 << (signal - 1))
}

/// Whether a wait can see `EINTR`, which the libc wants to know: a handler has
/// to have asked for that.
pub static INTERRUPTING_HANDLERS: AtomicBool = AtomicBool::new(false);

/// Signals held back from this thread, until the guard goes.
pub struct Blocked {
    old: libc::sigset_t,
}

impl Blocked {
    /// For a thread that's exited, while it holds a lock a handler could want.
    pub fn all() -> Blocked {
        unsafe {
            let mut all = MaybeUninit::<libc::sigset_t>::uninit();
            let mut old = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigfillset(all.as_mut_ptr());
            libc::pthread_sigmask(libc::SIG_BLOCK, all.as_ptr(), old.as_mut_ptr());
            Blocked { old: old.assume_init() }
        }
    }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, null_mut()) };
    }
}

#[repr(C)]
struct Chunk {
    next: *mut Chunk,
    infos: [MaybeUninit<libc::siginfo_t>; CHUNK_SIZE],
}

/// Signals waiting for this thread to get to a safepoint. Only touched with
/// signals blocked, as a handler can be interrupted by another signal. There's
/// no bound: a real-time signal has to arrive however many there are, so past
/// what's inline it maps memory, which a signal handler may do.
pub struct Queue {
    count: usize,
    inline: [MaybeUninit<libc::siginfo_t>; INLINE_QUEUE_SIZE],
    first: *mut Chunk,
    last: *mut Chunk,
    held_back: bool,
    delivered: u64,
}

impl Default for Queue {
    fn default() -> Queue {
        Queue {
            count: 0,
            inline: [const { MaybeUninit::uninit() }; INLINE_QUEUE_SIZE],
            first: null_mut(),
            last: null_mut(),
            held_back: false,
            delivered: 0,
        }
    }
}

/// Makes the calling thread, which is `thread`, the one signals go to when they
/// land on threads without a `Thread`. Anything forwarded so far is forgotten:
/// this is a new process, or one that just forked.
pub fn set_main(thread: *mut Thread) {
    unsafe {
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr());
        MAIN_MASK.store(mask_bits(mask.assume_init_ref()), Ordering::Release);
    }
    for slot in &FORWARDED {
        slot.count.store(0, Ordering::Relaxed);
    }
    ANY_FORWARDED.store(0, Ordering::Release);
    MAIN_THREAD.store(thread, Ordering::Release);
    MAIN_TID.store(unsafe { libc::gettid() }, Ordering::Release);
    set_wake_restarts(true);
}

/// Whether a handler has ever been installed.
static ANY_HANDLER: AtomicBool = AtomicBool::new(false);

/// With no libc main thread, as in the glibc flavor, signals that land on
/// threads without a `Thread` go to one that has a handler installed or
/// attaches after one was, while it lives. Until one does, they queue.
pub unsafe fn adopt(thread: *mut Thread) {
    if !ANY_HANDLER.load(Ordering::Acquire) || !MAIN_THREAD.load(Ordering::Acquire).is_null() {
        return;
    }
    if MAIN_THREAD.compare_exchange(null_mut(), thread, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    unsafe {
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr());
        MAIN_MASK.store(mask_bits(mask.assume_init_ref()), Ordering::Release);
        MAIN_TID.store(libc::gettid(), Ordering::Release);
        set_wake_restarts(true);
        // What queued meanwhile runs at its next safepoint.
        if ANY_FORWARDED.load(Ordering::Acquire) != 0 {
            (*thread).state.fetch_or(STATE_DEFERRED_SIGNAL, Ordering::AcqRel);
        }
    }
}

/// `thread` is going: forwarded signals go to another attached thread from
/// now on, if there is one, which is woken for any waiting. Its mask isn't
/// known here; it holds back what it blocks itself.
pub unsafe fn forget(thread: *mut Thread) {
    if MAIN_THREAD.load(Ordering::Acquire) != thread {
        return;
    }
    let threads = thread::lock_threads();
    let heir = threads.0.iter().map(|&entry| entry as *mut Thread).find(|&other| other != thread);
    if MAIN_THREAD.compare_exchange(thread, heir.unwrap_or(null_mut()), Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    match heir {
        Some(heir) => unsafe {
            MAIN_MASK.store(0, Ordering::Release);
            let tid = (*heir).tid as c_int;
            MAIN_TID.store(tid, Ordering::Release);
            if ANY_FORWARDED.load(Ordering::Acquire) != 0 {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, WAKE);
            }
        },
        None => MAIN_TID.store(0, Ordering::Release),
    }
}

/// Has [`WAKE`] restart system calls it interrupts, or not. Async-signal-safe.
fn set_wake_restarts(restarts: bool) {
    if WAKE_RESTARTS.swap(restarts as u32, Ordering::AcqRel) == restarts as u32 {
        return;
    }
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | if restarts { libc::SA_RESTART } else { 0 };
        libc::sigaction(WAKE, &action, null_mut());
    }
}

/// The program changed the main thread's mask, if that's `thread`.
pub unsafe fn note_mask(thread: *mut Thread) {
    if MAIN_THREAD.load(Ordering::Acquire) != thread {
        return;
    }
    unsafe {
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr());
        MAIN_MASK.store(mask_bits(mask.assume_init_ref()), Ordering::Release);
    }
}

/// Handlers this thread has run, so that a wait can tell being woken for one
/// from being woken for nothing.
pub unsafe fn delivered(thread: *mut Thread) -> u64 {
    unsafe { extra(thread).signals.delivered }
}

/// Sends what was written down this thread's way: those it doesn't block run
/// at its next safepoint, which may be now.
pub unsafe fn deliver_unblocked_soon(thread: *mut Thread) {
    if ANY_FORWARDED.load(Ordering::Acquire) != 0 {
        unsafe {
            (*thread).state.fetch_or(STATE_DEFERRED_SIGNAL, Ordering::AcqRel);
            thread::poll(thread);
        }
    }
}

/// Handlers are reachable through this table, whoever installed them.
pub unsafe fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    for entry in &TABLE {
        let handler = entry.load(Ordering::Acquire);
        if !handler.is_null() {
            visit(object_for_lower(handler.cast()));
        }
    }
}

/// True for the signals a Fil-C program has no business touching: those the
/// host libc keeps for itself.
fn is_unsafe_for_handlers(signal: c_int) -> bool {
    matches!(signal, libc::SIGILL | libc::SIGTRAP | libc::SIGBUS | libc::SIGSEGV | libc::SIGFPE) || is_internal_signal(signal)
}

unsafe fn defer(thread: *mut Thread, info: *const libc::siginfo_t) {
    let _blocked = Blocked::all();
    unsafe {
        let queue = &mut extra(thread).signals;
        let index = queue.count;
        queue.count += 1;
        if index < INLINE_QUEUE_SIZE {
            queue.inline[index].write(*info);
            return;
        }
        let index = (index - INLINE_QUEUE_SIZE) % CHUNK_SIZE;
        if index == 0 {
            let chunk = libc::mmap(
                null_mut(),
                size_of::<Chunk>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if chunk == libc::MAP_FAILED {
                internal_panic(format_args!("out of memory holding on to a signal"));
            }
            let chunk = chunk.cast::<Chunk>();
            if queue.last.is_null() {
                queue.first = chunk;
            } else {
                (*queue.last).next = chunk;
            }
            queue.last = chunk;
        }
        (&mut (*queue.last).infos)[index].write(*info);
    }
}

/// Runs the handler for `info` on this thread, which is entered, at whatever
/// counts as a safepoint here.
unsafe fn deliver(thread: *mut Thread, info: *const libc::siginfo_t) {
    unsafe {
        let signal = (*info).si_signo;
        let handler = TABLE[signal as usize].load(Ordering::Acquire);
        // Its disposition can have changed since the signal was queued.
        if handler.is_null() {
            return;
        }
        extra(thread).signals.delivered += 1;
        let native = NativeFrame::push(thread);
        // Nothing can unwind out of a handler, into what it interrupted.
        let mut frame = Frame { parent: (*thread).top_frame, origin: thread::base_origin(), lowers: [] };
        (*thread).top_frame = &mut frame;
        let function = (*handler).function;
        thread::track(thread, function.object());
        let object = object::allocate(thread, size_of::<libc::siginfo_t>(), 0);
        thread::track(thread, object);
        let copy = Ptr::for_object(object);
        std::ptr::copy_nonoverlapping(info.cast::<u8>(), copy.ptr, size_of::<libc::siginfo_t>());
        let number = Ptr { ptr: signal as usize as *mut u8, lower: null_mut() };
        crate::call(thread, function, &[number, copy, Ptr::NULL], &mut []);
        (*thread).top_frame = frame.parent;
        native.pop(thread);
    }
}

unsafe fn deliver_masked(thread: *mut Thread, info: *const libc::siginfo_t) {
    unsafe {
        let handler = TABLE[(*info).si_signo as usize].load(Ordering::Acquire);
        if handler.is_null() {
            return;
        }
        let mut old = MaybeUninit::<libc::sigset_t>::uninit();
        libc::pthread_sigmask(libc::SIG_BLOCK, &(*handler).mask, old.as_mut_ptr());
        deliver(thread, info);
        libc::pthread_sigmask(libc::SIG_SETMASK, old.as_ptr(), null_mut());
    }
}

unsafe fn consume(thread: *mut Thread) {
    unsafe {
        let (count, inline, mut chunk) = {
            let _blocked = Blocked::all();
            let queue = &mut extra(thread).signals;
            let taken = (queue.count, queue.inline, queue.first);
            queue.count = 0;
            queue.first = null_mut();
            queue.last = null_mut();
            taken
        };
        for info in &inline[..count.min(INLINE_QUEUE_SIZE)] {
            deliver_masked(thread, info.as_ptr());
        }
        let mut remaining = count.saturating_sub(INLINE_QUEUE_SIZE);
        while !chunk.is_null() {
            let here = remaining.min(CHUNK_SIZE);
            for index in 0..here {
                deliver_masked(thread, (&(*chunk).infos)[index].as_ptr());
            }
            remaining -= here;
            let next = (*chunk).next;
            libc::munmap(chunk.cast(), size_of::<Chunk>());
            chunk = next;
        }
        deliver_forwarded(thread);
    }
}

/// Runs handlers for what was forwarded, of the signals this thread doesn't
/// block now. A signal that isn't real-time runs once however many came, as
/// the kernel would have merged them.
unsafe fn deliver_forwarded(thread: *mut Thread) {
    if ANY_FORWARDED.load(Ordering::Acquire) == 0 {
        return;
    }
    unsafe {
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr());
        let mask = mask.assume_init();
        for signal in 1..=MAX_SIGNAL {
            let slot = &FORWARDED[signal as usize];
            if slot.count.load(Ordering::Acquire) == 0 || libc::sigismember(&mask, signal) == 1 {
                continue;
            }
            let count = slot.count.swap(0, Ordering::AcqRel);
            if count == 0 {
                continue;
            }
            ANY_FORWARDED.fetch_sub(count, Ordering::AcqRel);
            let mut info: libc::siginfo_t = std::mem::zeroed();
            {
                let _blocked = Blocked::all();
                while slot.busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                    std::hint::spin_loop();
                }
                if slot.has_info.load(Ordering::Relaxed) {
                    info = (*slot.info.get()).assume_init_read();
                }
                slot.busy.store(false, Ordering::Release);
            }
            info.si_signo = signal;
            let times = if signal < 32 { 1 } else { count };
            for _ in 0..times {
                deliver_masked(thread, &info);
            }
        }
    }
}

/// Runs the handlers that are waiting, on an entered thread.
pub unsafe fn handle_deferred(thread: *mut Thread) {
    unsafe {
        if (*thread).state.fetch_and(!STATE_DEFERRED_SIGNAL, Ordering::AcqRel) & STATE_DEFERRED_SIGNAL != 0 {
            consume(thread);
        }
    }
}

/// No handler runs on this thread until the matching [`allow_deferred`].
pub unsafe fn hold_deferred(thread: *mut Thread) {
    unsafe {
        (*thread).signal_deferral_depth += 1;
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
        if (*thread).state.fetch_and(!STATE_DEFERRED_SIGNAL, Ordering::AcqRel) & STATE_DEFERRED_SIGNAL != 0 {
            extra(thread).signals.held_back = true;
        }
    }
}

/// Lets what got held back through. It runs at the next safepoint.
pub unsafe fn after_deferral_decrease(thread: *mut Thread) {
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    unsafe {
        let signals = &mut extra(thread).signals;
        if (*thread).signal_deferral_depth == 0 && signals.held_back {
            signals.held_back = false;
            (*thread).state.fetch_or(STATE_DEFERRED_SIGNAL, Ordering::AcqRel);
        }
    }
}

pub unsafe fn allow_deferred(thread: *mut Thread) {
    unsafe {
        (*thread).signal_deferral_depth -= 1;
        after_deferral_decrease(thread);
    }
}

/// Hands a signal on to the main thread, as one that isn't Fil-C's can't run
/// its handler: see the top of this file.
unsafe fn forward(signal: c_int, info: *mut libc::siginfo_t) {
    let target = MAIN_TID.load(Ordering::Acquire);
    if target != 0 && MAIN_MASK.load(Ordering::Acquire) & (1 << (signal - 1)) != 0 {
        unsafe { send_on(target, signal, info) };
        return;
    }
    let slot = &FORWARDED[signal as usize];
    if !info.is_null() && slot.busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        unsafe { (*slot.info.get()).write(*info) };
        slot.has_info.store(true, Ordering::Relaxed);
        slot.busy.store(false, Ordering::Release);
    }
    slot.count.fetch_add(1, Ordering::AcqRel);
    ANY_FORWARDED.fetch_add(1, Ordering::AcqRel);
    let handler = TABLE[signal as usize].load(Ordering::Acquire);
    if target == 0 {
        // Queued for whichever thread adopts them.
        return;
    }
    if !handler.is_null() {
        set_wake_restarts(unsafe { (*handler).flags } & libc::SA_RESTART != 0);
    }
    unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), target, WAKE) };
}

/// Sends the signal itself to `target`. The kernel only lets a thread queue a
/// siginfo that claims to be from user space to another, so others go without
/// theirs, as `tgkill`.
unsafe fn send_on(target: c_int, signal: c_int, info: *mut libc::siginfo_t) {
    unsafe {
        let pid = libc::getpid();
        if info.is_null()
            || (*info).si_code >= 0
            || libc::syscall(libc::SYS_rt_tgsigqueueinfo, pid, target, signal, info) != 0
        {
            libc::syscall(libc::SYS_tgkill, pid, target, signal);
        }
    }
}

unsafe extern "C" fn on_signal(signal: c_int, info: *mut libc::siginfo_t, _: *mut c_void) {
    unsafe {
        let error = *libc::__errno_location();
        let thread = thread::current();
        if signal == WAKE {
            if !thread.is_null() {
                wake(thread);
            }
        } else if thread.is_null() {
            forward(signal, info);
        } else {
            let marker = 0u8;
            if ((&raw const marker) as usize) < (*thread).stack_limit as usize {
                crate::failure::safety_panic(format_args!("stack overflow."));
            }
            let state = (*thread).state.load(Ordering::Acquire);
            let held = (*thread).signal_deferral_depth != 0;
            if state & STATE_ENTERED != 0 || held {
                defer(thread, info);
                if held {
                    extra(thread).signals.held_back = true;
                } else {
                    (*thread).state.fetch_or(STATE_DEFERRED_SIGNAL, Ordering::AcqRel);
                }
            } else {
                // It may be in `thread::exit`, just past the point where a
                // collection could go on without it, and yet to say so.
                if state & STATE_STOP_REQUESTED != 0 {
                    crate::gc::note_stopped();
                }
                thread::enter(thread);
                deliver(thread, info);
                thread::exit(thread);
            }
        }
        *libc::__errno_location() = error;
    }
}

/// [`WAKE`] came: what was forwarded runs here, as if it had been sent to this
/// thread, which is the main one.
unsafe fn wake(thread: *mut Thread) {
    unsafe {
        let state = (*thread).state.load(Ordering::Acquire);
        if (*thread).signal_deferral_depth != 0 {
            extra(thread).signals.held_back = true;
        } else if state & STATE_ENTERED != 0 {
            (*thread).state.fetch_or(STATE_DEFERRED_SIGNAL, Ordering::AcqRel);
        } else {
            if state & STATE_STOP_REQUESTED != 0 {
                crate::gc::note_stopped();
            }
            thread::enter(thread);
            deliver_forwarded(thread);
            thread::exit(thread);
        }
    }
}

fn is_special_handler(handler: usize) -> bool {
    handler == libc::SIG_DFL || handler == libc::SIG_IGN
}

/// Makes the `sigaction` call the kernel should see, and the handler for it.
unsafe fn from_user(thread: *mut Thread, signal: c_int, act: Ptr) -> Result<(libc::sigaction, *mut Handler), c_int> {
    unsafe {
        let user = one_in::<libc::sigaction>(act);
        let mask_field = (&raw const (*user).sa_mask).cast_mut().cast::<u8>();
        let mut host: libc::sigaction = std::mem::zeroed();
        host.sa_mask = user_sigset(act.with_ptr(mask_field));
        host.sa_flags = (*user).sa_flags;
        let field = (&raw const (*user).sa_sigaction).cast_mut().cast::<u8>();
        let function = ptrs::load(act.with_ptr(field));
        let given = function.ptr as usize;
        if is_special_handler(given) {
            host.sa_sigaction = given;
            return Ok((host, null_mut()));
        }
        if is_unsafe_for_handlers(signal) {
            return Err(libc::ENOSYS);
        }
        object::check_function_call(function);
        if host.sa_flags & libc::SA_RESTART == 0 {
            INTERRUPTING_HANDLERS.store(true, Ordering::Relaxed);
        }
        let object = object::allocate_special(thread, size_of::<Handler>(), SPECIAL_SIGNAL_HANDLER);
        thread::track(thread, object);
        let handler = lower_for_object(object).cast::<Handler>();
        handler.write(Handler { function, mask: host.sa_mask, flags: host.sa_flags });
        host.sa_sigaction = on_signal as *const () as usize;
        host.sa_flags |= libc::SA_SIGINFO;
        Ok((host, handler))
    }
}

/// Tells the program what `sigaction` found.
unsafe fn to_user(thread: *mut Thread, signal: c_int, user: Ptr, host: &libc::sigaction) {
    object::check_access(user, size_of::<libc::sigaction>(), Access::Write);
    unsafe {
        let old = TABLE[signal as usize].load(Ordering::Acquire);
        let fields = user.ptr.cast::<libc::sigaction>();
        let field = (&raw mut (*fields).sa_sigaction).cast::<u8>();
        let function = if host.sa_sigaction == on_signal as *const () as usize && !old.is_null() {
            (*old).function
        } else {
            Ptr { ptr: host.sa_sigaction as *mut u8, lower: null_mut() }
        };
        ptrs::store(thread, user.with_ptr(field), function);
        (*fields).sa_flags = host.sa_flags;
        if !old.is_null() {
            (*fields).sa_flags = (host.sa_flags & !libc::SA_SIGINFO) | ((*old).flags & libc::SA_SIGINFO);
        }
        (*fields).sa_mask = host.sa_mask;
    }
}

/// `sigaction`, with the program's `struct sigaction`s, which are the host's.
pub(crate) unsafe fn sigaction(thread: *mut Thread, signal: c_int, act: Ptr, old_act: Ptr) -> c_int {
    if !(0..=MAX_SIGNAL).contains(&signal) {
        unsafe { set_errno(thread, libc::EINVAL) };
        return -1;
    }
    unsafe {
        // Handlers for what's already here have to come first: once this
        // is done, the table says something else.
        thread::poll(thread);
        let wanted = if act.ptr.is_null() {
            None
        } else {
            match from_user(thread, signal, act) {
                Ok(wanted) => Some(wanted),
                Err(error) => {
                    set_errno(thread, error);
                    return -1;
                }
            }
        };
        hold_deferred(thread);
        let mut old = std::mem::zeroed::<libc::sigaction>();
        let new = wanted.as_ref().map_or(std::ptr::null(), |(host, _)| &raw const *host);
        let result = sys(thread, || libc::sigaction(signal, new, &mut old));
        if result == 0 {
            if !old_act.ptr.is_null() {
                to_user(thread, signal, old_act, &old);
            }
            if let Some((_, handler)) = wanted {
                TABLE[signal as usize].store(handler, Ordering::Release);
                if !handler.is_null() {
                    ANY_HANDLER.store(true, Ordering::Release);
                    adopt(thread);
                }
            }
        }
        allow_deferred(thread);
        result
    }
}

natives! {
    fn zsys_sigaction(thread: *mut Thread, signal: c_int, act: Ptr, old_act: Ptr) -> c_int {
        unsafe { sigaction(thread, signal, act, old_act) }
    }

    fn zsys_sigaltstack(thread: *mut Thread, stack: Ptr, old: Ptr) -> c_int {
        internal_panic(format_args!("sigaltstack not supported."));
    }

    fn zsys_sigsuspend(thread: *mut Thread, mask: Ptr) -> c_int {
        unsafe {
            let mask = user_sigset(mask);
            // Being woken for a forwarded signal this mask blocks isn't the
            // end of the wait.
            loop {
                let before = delivered(thread);
                let result = sys(thread, || libc::sigsuspend(&mask));
                if result != -1 || delivered(thread) != before || ANY_FORWARDED.load(Ordering::Acquire) == 0 {
                    return result;
                }
            }
        }
    }

    fn zsys_sigwait(thread: *mut Thread, set: Ptr, signal: Ptr) -> c_int {
        unsafe {
            let set = user_sigset(set);
            let mut found = 0;
            let result = plain(thread, || libc::sigwait(&set, &mut found));
            if result == 0 {
                crate::sys::one_out::<c_int>(signal).write(found);
            }
            result
        }
    }

    fn zsys_sigtimedwait(thread: *mut Thread, set: Ptr, info: Ptr, timeout: Ptr) -> c_int {
        unsafe {
            let set = user_sigset(set);
            let info = crate::sys::maybe_out::<libc::siginfo_t>(info);
            let timeout = crate::sys::maybe_in::<libc::timespec>(timeout);
            sys(thread, || libc::sigtimedwait(&set, info, timeout))
        }
    }

    fn zsys_sigqueue(thread: *mut Thread, process: c_int, signal: c_int, value: Ptr) -> c_int {
        if is_internal_signal(signal) {
            unsafe { set_errno(thread, libc::ENOSYS) };
            return -1;
        }
        let value = libc::sigval { sival_ptr: value.ptr.cast() };
        unsafe { sys(thread, || libc::sigqueue(process, signal, value)) }
    }

    fn zsys_signalfd(thread: *mut Thread, fd: c_int, mask: Ptr, flags: c_int) -> c_int {
        unsafe {
            let mut set = user_sigset(mask);
            for signal in 1..=MAX_SIGNAL {
                if is_unsafe_for_handlers(signal) {
                    libc::sigdelset(&mut set, signal);
                }
            }
            sys(thread, || libc::signalfd(fd, &set, flags))
        }
    }

    fn zsys_raise_without_exit(thread: *mut Thread, signal: c_int) -> c_int {
        if is_internal_signal(signal) {
            unsafe { set_errno(thread, libc::ENOSYS) };
            return -1;
        }
        unsafe { sys_result(thread, libc::raise(signal)) }
    }

    fn zis_unsafe_signal_for_kill(thread: *mut Thread, signal: c_int) -> bool {
        is_internal_signal(signal)
    }

    fn zis_unsafe_signal_for_handlers(thread: *mut Thread, signal: c_int) -> bool {
        is_unsafe_for_handlers(signal)
    }

    fn znum_deferred_signals(thread: *mut Thread) -> usize {
        unsafe { extra(thread).signals.count }
    }
}
