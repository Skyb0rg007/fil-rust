//! `<pthread.h>` and `<semaphore.h>`. A `pthread_t` is the thread's object,
//! a capability. Locks are futexes in the objects glibc's headers size, so
//! static initializers (all zero, or a kind for the recursive one) work: each
//! operation checks the object, then works on its words atomically. A thread
//! that has to wait exits first, so the collector never waits on it, and comes
//! back between waits, so its signal handlers run.
//!
//! Key destructors run when a thread Fil-C created returns or calls
//! `pthread_exit`; on threads Rust attached, they don't run, and the values
//! go with the thread.

use std::ffi::c_int;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::abi::*;
use crate::natives::{Thrown, natives};
use crate::object::{self, Access};
use crate::glibc::set_errno;
use crate::sys::{one_in, plain};
use crate::{thread, view, zthread};

use super::callbacks::call_back;

const MUTEX_SIZE: usize = 40;
const COND_SIZE: usize = 48;
const RWLOCK_SIZE: usize = 56;
const ATTR_SIZE: usize = 56;
const BARRIER_SIZE: usize = 32;
const SEM_SIZE: usize = 32;

// glibc's, which the libc crate doesn't have.
const CANCEL_ENABLE: c_int = 0;
const CANCEL_DEFERRED: c_int = 0;
const SCOPE_SYSTEM: c_int = 0;
const SCOPE_PROCESS: c_int = 1;

const NORMAL: u32 = 0;
const RECURSIVE: u32 = 1;
const ERRORCHECK: u32 = 2;

/// The word at `offset` in the `size`-byte object at `at`, which has to be
/// writable and aligned.
fn word(at: Ptr, size: usize, offset: usize) -> &'static AtomicU32 {
    object::check_access(at, size, Access::Write);
    if !(at.ptr as usize).is_multiple_of(4) {
        crate::failure::safety_panic(format_args!("a lock that isn't aligned (ptr = {:p}).", at.ptr));
    }
    unsafe { &*at.ptr.add(offset).cast::<AtomicU32>() }
}

/// An absolute time on `clock`, or none.
#[derive(Clone, Copy)]
struct Deadline {
    clock: libc::clockid_t,
    time: libc::timespec,
}

fn deadline(clock: libc::clockid_t, time: Ptr) -> Result<Deadline, c_int> {
    let time = unsafe { one_in::<libc::timespec>(time).read() };
    if !(0..1_000_000_000).contains(&time.tv_nsec) {
        return Err(libc::EINVAL);
    }
    if clock != libc::CLOCK_REALTIME && clock != libc::CLOCK_MONOTONIC {
        return Err(libc::EINVAL);
    }
    Ok(Deadline { clock, time })
}

/// Waits while `word` holds `expected`, exited, until woken, a signal, or the
/// deadline. Returns 0, or `ETIMEDOUT`.
unsafe fn wait(me: *mut Thread, word: &AtomicU32, expected: u32, deadline: Option<Deadline>) -> c_int {
    let address = word as *const AtomicU32 as usize;
    let error = unsafe {
        plain(me, || {
            let mut operation = libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG;
            let time = match &deadline {
                Some(deadline) => {
                    if deadline.clock == libc::CLOCK_REALTIME {
                        operation |= libc::FUTEX_CLOCK_REALTIME;
                    }
                    &deadline.time as *const libc::timespec
                }
                None => std::ptr::null(),
            };
            let result = libc::syscall(libc::SYS_futex, address, operation, expected, time, 0usize, u32::MAX);
            if result < 0 { *libc::__errno_location() } else { 0 }
        })
    };
    if error == libc::ETIMEDOUT { libc::ETIMEDOUT } else { 0 }
}

fn wake(word: &AtomicU32, count: i32) {
    unsafe { libc::syscall(libc::SYS_futex, word as *const AtomicU32, libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, count) };
}

fn tid(me: *mut Thread) -> u32 {
    unsafe { (*me).tid }
}

/// glibc's `pthread_mutex_t`: the lock word (0 free, 1 held, 2 held with
/// waiters), the recursion count, the owner's id and the kind.
struct Mutex_ {
    lock: &'static AtomicU32,
    count: &'static AtomicU32,
    owner: &'static AtomicU32,
    kind: u32,
}

fn mutex(at: Ptr) -> Mutex_ {
    Mutex_ {
        lock: word(at, MUTEX_SIZE, 0),
        count: word(at, MUTEX_SIZE, 4),
        owner: word(at, MUTEX_SIZE, 8),
        kind: word(at, MUTEX_SIZE, 16).load(Ordering::Relaxed) & 3,
    }
}

/// Locks `at`, or tries to, waiting until `deadline` at most. Returns 0 or
/// an error number.
unsafe fn lock(me: *mut Thread, at: Ptr, try_only: bool, deadline: Option<Deadline>) -> c_int {
    let mutex = mutex(at);
    let id = tid(me);
    if mutex.owner.load(Ordering::Relaxed) == id {
        match mutex.kind {
            RECURSIVE => {
                let count = mutex.count.load(Ordering::Relaxed);
                if count == u32::MAX {
                    return libc::EAGAIN;
                }
                mutex.count.store(count + 1, Ordering::Relaxed);
                return 0;
            }
            ERRORCHECK if !try_only => return libc::EDEADLK,
            _ => {}
        }
    }
    if mutex.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        if try_only {
            return libc::EBUSY;
        }
        unsafe { thread::track(me, at.object()) };
        while mutex.lock.swap(2, Ordering::Acquire) != 0 {
            if unsafe { wait(me, mutex.lock, 2, deadline) } == libc::ETIMEDOUT {
                return libc::ETIMEDOUT;
            }
        }
    }
    mutex.owner.store(id, Ordering::Relaxed);
    if mutex.kind == RECURSIVE {
        mutex.count.store(1, Ordering::Relaxed);
    }
    0
}

/// Unlocks `at`. A recursive or error-checking mutex has to be the caller's.
unsafe fn unlock(me: *mut Thread, at: Ptr) -> c_int {
    let mutex = mutex(at);
    if mutex.kind != NORMAL && mutex.owner.load(Ordering::Relaxed) != tid(me) {
        return libc::EPERM;
    }
    if mutex.kind == RECURSIVE {
        let count = mutex.count.load(Ordering::Relaxed);
        if count > 1 {
            mutex.count.store(count - 1, Ordering::Relaxed);
            return 0;
        }
        mutex.count.store(0, Ordering::Relaxed);
    }
    mutex.owner.store(0, Ordering::Relaxed);
    if mutex.lock.swap(0, Ordering::Release) == 2 {
        wake(mutex.lock, 1);
    }
    0
}

/// glibc's `pthread_cond_t`: a sequence bumped by every signal, and here,
/// the clock its waits go by.
unsafe fn cond_wait(me: *mut Thread, cond: Ptr, mutex: Ptr, deadline: Option<Deadline>) -> c_int {
    let sequence = word(cond, COND_SIZE, 0);
    let seen = sequence.load(Ordering::Acquire);
    let error = unsafe { unlock(me, mutex) };
    if error != 0 {
        return error;
    }
    unsafe { thread::track(me, cond.object()) };
    let waited = unsafe { wait(me, sequence, seen, deadline) };
    let error = unsafe { lock(me, mutex, false, None) };
    if error != 0 { error } else { waited }
}

fn cond_clock(cond: Ptr) -> libc::clockid_t {
    word(cond, COND_SIZE, 8).load(Ordering::Relaxed) as libc::clockid_t
}

/// A `pthread_rwlock_t`: readers holding it, or `u32::MAX` for a writer, and
/// a sequence bumped by every unlock.
unsafe fn read_lock(me: *mut Thread, at: Ptr, try_only: bool, deadline: Option<Deadline>) -> c_int {
    let (state, sequence) = (word(at, RWLOCK_SIZE, 0), word(at, RWLOCK_SIZE, 4));
    loop {
        let seen = sequence.load(Ordering::Acquire);
        let held = state.load(Ordering::Relaxed);
        if held < u32::MAX - 1 {
            if state.compare_exchange(held, held + 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return 0;
            }
            continue;
        }
        if held == u32::MAX - 1 {
            return libc::EAGAIN;
        }
        if try_only {
            return libc::EBUSY;
        }
        unsafe { thread::track(me, at.object()) };
        if unsafe { wait(me, sequence, seen, deadline) } == libc::ETIMEDOUT {
            return libc::ETIMEDOUT;
        }
    }
}

unsafe fn write_lock(me: *mut Thread, at: Ptr, try_only: bool, deadline: Option<Deadline>) -> c_int {
    let (state, sequence) = (word(at, RWLOCK_SIZE, 0), word(at, RWLOCK_SIZE, 4));
    loop {
        let seen = sequence.load(Ordering::Acquire);
        if state.compare_exchange(0, u32::MAX, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return 0;
        }
        if try_only {
            return libc::EBUSY;
        }
        unsafe { thread::track(me, at.object()) };
        if unsafe { wait(me, sequence, seen, deadline) } == libc::ETIMEDOUT {
            return libc::ETIMEDOUT;
        }
    }
}

/// A semaphore's count, with its waiters' wait.
unsafe fn sem_take(me: *mut Thread, at: Ptr, try_only: bool, deadline: Option<Deadline>) -> c_int {
    let value = word(at, SEM_SIZE, 0);
    loop {
        let count = value.load(Ordering::Relaxed);
        if count > 0 {
            if value.compare_exchange(count, count - 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return 0;
            }
            continue;
        }
        if try_only {
            unsafe { set_errno(me, libc::EAGAIN) };
            return -1;
        }
        unsafe { thread::track(me, at.object()) };
        if unsafe { wait(me, value, 0, deadline) } == libc::ETIMEDOUT {
            unsafe { set_errno(me, libc::ETIMEDOUT) };
            return -1;
        }
    }
}

fn sem_result(me: *mut Thread, error: c_int) -> c_int {
    if error == 0 {
        0
    } else {
        unsafe { set_errno(me, error) };
        -1
    }
}

/// Keys: whether each is in use, its destructor, and a generation that
/// changes when it's deleted, so values set before don't come back.
#[derive(Clone, Copy)]
struct Key {
    used: bool,
    destructor: (usize, usize),
    generation: u64,
}

const KEYS_MAX: usize = 1024;
const DESTRUCTOR_ITERATIONS: usize = 4;

/// Roots; locked only entered, never across a call or an allocation.
static KEYS: Mutex<Vec<Key>> = Mutex::new(Vec::new());

/// Registered `pthread_atfork` handlers, oldest first: prepare, parent and
/// child. Roots, locked the same way.
static AT_FORK: Mutex<Vec<[(usize, usize); 3]>> = Mutex::new(Vec::new());

fn pair(ptr: Ptr) -> (usize, usize) {
    (ptr.ptr as usize, ptr.lower as usize)
}

fn unpair((ptr, lower): (usize, usize)) -> Ptr {
    Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 }
}

pub fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    for key in KEYS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if key.destructor.1 != 0 {
            visit(object_for_lower(key.destructor.1 as *mut u8));
        }
    }
    for handlers in AT_FORK.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        for handler in handlers {
            if handler.1 != 0 {
                visit(object_for_lower(handler.1 as *mut u8));
            }
        }
    }
}

/// Runs this thread's key destructors, as it ends: each value that isn't
/// null is cleared and handed to its key's destructor, again while
/// destructors set more, up to glibc's limit.
pub(crate) unsafe fn run_key_destructors(me: *mut Thread) {
    for _ in 0..DESTRUCTOR_ITERATIONS {
        let keys = KEYS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut ran = false;
        for (index, key) in keys.iter().enumerate() {
            if !key.used || key.destructor.1 == 0 {
                continue;
            }
            let values = &mut unsafe { thread::extra(me) }.libc.keys;
            let Some(&(generation, value)) = values.get(index) else { continue };
            if generation != key.generation || value.ptr.is_null() {
                continue;
            }
            values[index].1 = Ptr::NULL;
            unsafe { thread::track(me, value.object()) };
            if unsafe { call_back(me, unpair(key.destructor), &[value]) }.is_none() {
                crate::failure::safety_panic(format_args!("an exception escaped a key destructor."));
            }
            ran = true;
        }
        if !ran {
            break;
        }
    }
}

/// Runs the `pthread_atfork` handlers for before a fork.
pub(crate) unsafe fn before_fork(me: *mut Thread) {
    let handlers = AT_FORK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for handler in handlers.iter().rev() {
        unsafe { run_fork_handler(me, handler[0]) };
    }
}

/// Runs the handlers for after it, in the parent or the child.
pub(crate) unsafe fn after_fork(me: *mut Thread, child: bool) {
    let handlers = AT_FORK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for handler in &handlers {
        unsafe { run_fork_handler(me, handler[if child { 2 } else { 1 }]) };
    }
}

unsafe fn run_fork_handler(me: *mut Thread, handler: (usize, usize)) {
    if handler.0 != 0 && unsafe { call_back(me, unpair(handler), &[]) }.is_none() {
        crate::failure::safety_panic(format_args!("an exception escaped a fork handler."));
    }
}

fn at_fork(prepare: Ptr, parent: Ptr, child: Ptr) -> c_int {
    for handler in [prepare, parent, child] {
        if !handler.ptr.is_null() {
            object::check_function_call(handler);
        }
    }
    AT_FORK.lock().unwrap_or_else(|e| e.into_inner()).push([pair(prepare), pair(parent), pair(child)]);
    0
}

fn read_u32(at: Ptr) -> u32 {
    let bytes = view::Bytes::exactly(at, 4, Access::Read);
    u32::from_le_bytes(std::array::from_fn(|index| bytes.get(index)))
}

fn write_u32(at: Ptr, value: u32) {
    view::write(at, &value.to_le_bytes());
}

fn attr_field(attr: Ptr, offset: usize) -> Ptr {
    object::check_access(attr, ATTR_SIZE, Access::Write);
    attr.with_ptr(attr.ptr.wrapping_add(offset))
}

fn read_u64(at: Ptr) -> u64 {
    let bytes = view::Bytes::exactly(at, 8, Access::Read);
    u64::from_le_bytes(std::array::from_fn(|index| bytes.get(index)))
}

/// What `pthread_attr_t` holds here: the detach state, the stack size and
/// the guard size, scope, inheritance and policy.
const ATTR_DETACH: usize = 0;
const ATTR_STACK: usize = 8;
const ATTR_GUARD: usize = 16;
const ATTR_SCOPE: usize = 24;
const ATTR_INHERIT: usize = 28;
const ATTR_POLICY: usize = 32;

fn attr_init(attr: Ptr, stack: u64) {
    view::write(attr_field(attr, 0), &[0; ATTR_SIZE]);
    view::write(attr_field(attr, ATTR_STACK), &stack.to_le_bytes());
    view::write(attr_field(attr, ATTR_GUARD), &(page_size() as u64).to_le_bytes());
}

fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Stores `value` at `out` as a C `int`.
fn give_int(out: Ptr, value: c_int) -> c_int {
    write_u32(out, value as u32);
    0
}

natives! {
    fn pthread_create(thread: *mut Thread, out: Ptr, attr: Ptr, function: Ptr, argument: Ptr) -> c_int {
        object::check_word_access(out, Access::Write);
        let stack = if attr.ptr.is_null() { 0 } else { read_u64(attr_field(attr, ATTR_STACK)) as usize };
        unsafe { zthread::create(thread, function, argument, out, Ptr::NULL, stack) }
    }

    fn pthread_join(thread: *mut Thread, target: Ptr, result: Ptr) -> c_int {
        unsafe { zthread::join(thread, target, result) }
    }

    fn pthread_detach(thread: *mut Thread, target: Ptr) -> c_int {
        zthread::check_zthread(target);
        0
    }

    fn pthread_self(thread: *mut Thread) -> Ptr {
        Ptr::for_object(object_for_lower(thread.cast()))
    }

    fn pthread_equal(thread: *mut Thread, a: Ptr, b: Ptr) -> c_int {
        (a.ptr == b.ptr) as c_int
    }

    fn pthread_exit(thread: *mut Thread, result: Ptr) -> () {
        if !unsafe { zthread::can_leave(thread) } {
            crate::failure::safety_panic(format_args!("pthread_exit on a thread Fil-C code didn't create."));
        }
        unsafe {
            run_key_destructors(thread);
            zthread::leave(thread, result)
        }
    }

    fn pthread_kill(thread: *mut Thread, target: Ptr, signal: c_int) -> c_int {
        unsafe { zthread::kill(thread, target, signal) }
    }

    fn pthread_sigmask(thread: *mut Thread, how: c_int, set: Ptr, old: Ptr) -> c_int {
        let set = (!set.ptr.is_null()).then(|| unsafe { crate::sys::user_sigset(set) });
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let result = unsafe { plain(thread, || libc::pthread_sigmask(how, set.as_ref().map_or(std::ptr::null(), |set| set), &mut previous)) };
        if result != 0 {
            return result;
        }
        if !old.ptr.is_null() {
            unsafe { crate::sys::one_out::<libc::sigset_t>(old).write(previous) };
        }
        unsafe {
            crate::signal::note_mask(thread);
            crate::signal::deliver_unblocked_soon(thread);
        }
        0
    }

    fn pthread_setname_np(thread: *mut Thread, target: Ptr, name: Ptr) -> c_int {
        zthread::check_zthread(target);
        let name = view::c_string(name);
        if name.len() > 15 {
            return libc::ERANGE;
        }
        let name = std::ffi::CString::new(name).unwrap();
        let target = target.ptr.cast::<Thread>();
        unsafe {
            plain(thread, || {
                let _guard = thread::lock_lifecycle();
                match (*target).host {
                    0 => libc::ESRCH,
                    host => libc::pthread_setname_np(host as libc::pthread_t, name.as_ptr()),
                }
            })
        }
    }

    fn pthread_getname_np(thread: *mut Thread, target: Ptr, buffer: Ptr, size: usize) -> c_int {
        zthread::check_zthread(target);
        let target = target.ptr.cast::<Thread>();
        let mut name = [0 as std::ffi::c_char; 16];
        let result = unsafe {
            plain(thread, || {
                let _guard = thread::lock_lifecycle();
                match (*target).host {
                    0 => libc::ESRCH,
                    host => libc::pthread_getname_np(host as libc::pthread_t, name.as_mut_ptr(), name.len()),
                }
            })
        };
        if result != 0 {
            return result;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes_with_nul();
        if name.len() > size {
            return libc::ERANGE;
        }
        view::write(buffer, name);
        0
    }

    fn pthread_setcancelstate(thread: *mut Thread, state: c_int, old: Ptr) -> c_int {
        if !old.ptr.is_null() {
            give_int(old, CANCEL_ENABLE);
        }
        0
    }

    fn pthread_setcanceltype(thread: *mut Thread, kind: c_int, old: Ptr) -> c_int {
        if !old.ptr.is_null() {
            give_int(old, CANCEL_DEFERRED);
        }
        0
    }

    fn pthread_testcancel(thread: *mut Thread) -> () {}

    fn pthread_yield(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::sched_yield()) }
    }

    fn pthread_attr_init(thread: *mut Thread, attr: Ptr) -> c_int {
        attr_init(attr, 0);
        0
    }

    fn pthread_attr_destroy(thread: *mut Thread, attr: Ptr) -> c_int {
        let _ = attr_field(attr, 0);
        0
    }

    fn pthread_getattr_np(thread: *mut Thread, target: Ptr, attr: Ptr) -> c_int {
        zthread::check_zthread(target);
        let target = target.ptr.cast::<Thread>();
        let size = unsafe { (*target).stack_top as usize - (*target).stack_low as usize };
        attr_init(attr, size as u64);
        0
    }

    fn pthread_attr_setdetachstate(thread: *mut Thread, attr: Ptr, state: c_int) -> c_int {
        if state != libc::PTHREAD_CREATE_JOINABLE && state != libc::PTHREAD_CREATE_DETACHED {
            return libc::EINVAL;
        }
        give_int(attr_field(attr, ATTR_DETACH), state)
    }

    fn pthread_attr_getdetachstate(thread: *mut Thread, attr: Ptr, state: Ptr) -> c_int {
        give_int(state, read_u32(attr_field(attr, ATTR_DETACH)) as c_int)
    }

    fn pthread_attr_setstacksize(thread: *mut Thread, attr: Ptr, size: usize) -> c_int {
        if size < libc::PTHREAD_STACK_MIN {
            return libc::EINVAL;
        }
        view::write(attr_field(attr, ATTR_STACK), &(size as u64).to_le_bytes());
        0
    }

    fn pthread_attr_getstacksize(thread: *mut Thread, attr: Ptr, size: Ptr) -> c_int {
        let mut stack = read_u64(attr_field(attr, ATTR_STACK));
        if stack == 0 {
            let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
            stack = if unsafe { libc::getrlimit(libc::RLIMIT_STACK, &mut limit) } == 0 && limit.rlim_cur != libc::RLIM_INFINITY { limit.rlim_cur } else { 8 << 20 };
        }
        view::write(size, &stack.to_le_bytes());
        0
    }

    fn pthread_attr_setguardsize(thread: *mut Thread, attr: Ptr, size: usize) -> c_int {
        view::write(attr_field(attr, ATTR_GUARD), &(size as u64).to_le_bytes());
        0
    }

    fn pthread_attr_getguardsize(thread: *mut Thread, attr: Ptr, size: Ptr) -> c_int {
        let guard = read_u64(attr_field(attr, ATTR_GUARD));
        view::write(size, &guard.to_le_bytes());
        0
    }

    fn pthread_attr_setscope(thread: *mut Thread, attr: Ptr, scope: c_int) -> c_int {
        match scope {
            SCOPE_SYSTEM => give_int(attr_field(attr, ATTR_SCOPE), scope),
            SCOPE_PROCESS => libc::ENOTSUP,
            _ => libc::EINVAL,
        }
    }

    fn pthread_attr_getscope(thread: *mut Thread, attr: Ptr, scope: Ptr) -> c_int {
        let _ = attr_field(attr, ATTR_SCOPE);
        give_int(scope, SCOPE_SYSTEM)
    }

    fn pthread_attr_setinheritsched(thread: *mut Thread, attr: Ptr, inherit: c_int) -> c_int {
        give_int(attr_field(attr, ATTR_INHERIT), inherit)
    }

    fn pthread_attr_getinheritsched(thread: *mut Thread, attr: Ptr, inherit: Ptr) -> c_int {
        give_int(inherit, read_u32(attr_field(attr, ATTR_INHERIT)) as c_int)
    }

    fn pthread_attr_setschedpolicy(thread: *mut Thread, attr: Ptr, policy: c_int) -> c_int {
        give_int(attr_field(attr, ATTR_POLICY), policy)
    }

    fn pthread_attr_getschedpolicy(thread: *mut Thread, attr: Ptr, policy: Ptr) -> c_int {
        give_int(policy, read_u32(attr_field(attr, ATTR_POLICY)) as c_int)
    }

    fn pthread_mutex_init(thread: *mut Thread, at: Ptr, attr: Ptr) -> c_int {
        let kind = if attr.ptr.is_null() { NORMAL } else { read_u32(attr) & 3 };
        object::check_access(at, MUTEX_SIZE, Access::Write);
        view::write(at, &[0; MUTEX_SIZE]);
        word(at, MUTEX_SIZE, 16).store(kind, Ordering::Relaxed);
        0
    }

    fn pthread_mutex_destroy(thread: *mut Thread, at: Ptr) -> c_int {
        if mutex(at).lock.load(Ordering::Relaxed) != 0 { libc::EBUSY } else { 0 }
    }

    fn pthread_mutex_lock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { lock(thread, at, false, None) }
    }

    fn pthread_mutex_trylock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { lock(thread, at, true, None) }
    }

    fn pthread_mutex_timedlock(thread: *mut Thread, at: Ptr, time: Ptr) -> c_int {
        match deadline(libc::CLOCK_REALTIME, time) {
            Ok(deadline) => unsafe { lock(thread, at, false, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_mutex_clocklock(thread: *mut Thread, at: Ptr, clock: c_int, time: Ptr) -> c_int {
        match deadline(clock, time) {
            Ok(deadline) => unsafe { lock(thread, at, false, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_mutex_unlock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { unlock(thread, at) }
    }

    fn pthread_mutexattr_init(thread: *mut Thread, attr: Ptr) -> c_int {
        write_u32(attr, NORMAL);
        0
    }

    fn pthread_mutexattr_destroy(thread: *mut Thread, attr: Ptr) -> c_int {
        let _ = read_u32(attr);
        0
    }

    fn pthread_mutexattr_settype(thread: *mut Thread, attr: Ptr, kind: c_int) -> c_int {
        if !(0..=3).contains(&kind) {
            return libc::EINVAL;
        }
        write_u32(attr, kind as u32);
        0
    }

    fn pthread_mutexattr_gettype(thread: *mut Thread, attr: Ptr, kind: Ptr) -> c_int {
        give_int(kind, (read_u32(attr) & 3) as c_int)
    }

    fn pthread_mutexattr_setpshared(thread: *mut Thread, attr: Ptr, shared: c_int) -> c_int {
        let _ = read_u32(attr);
        if shared == libc::PTHREAD_PROCESS_PRIVATE { 0 } else { libc::ENOTSUP }
    }

    fn pthread_mutexattr_getpshared(thread: *mut Thread, attr: Ptr, shared: Ptr) -> c_int {
        let _ = read_u32(attr);
        give_int(shared, libc::PTHREAD_PROCESS_PRIVATE)
    }

    fn pthread_mutexattr_setrobust(thread: *mut Thread, attr: Ptr, robust: c_int) -> c_int {
        let _ = read_u32(attr);
        if robust == 0 { 0 } else { libc::ENOTSUP }
    }

    fn pthread_mutexattr_setprotocol(thread: *mut Thread, attr: Ptr, protocol: c_int) -> c_int {
        let _ = read_u32(attr);
        if protocol == 0 { 0 } else { libc::ENOTSUP }
    }

    fn pthread_cond_init(thread: *mut Thread, cond: Ptr, attr: Ptr) -> c_int {
        let clock = if attr.ptr.is_null() { libc::CLOCK_REALTIME as u32 } else { read_u32(attr) };
        object::check_access(cond, COND_SIZE, Access::Write);
        view::write(cond, &[0; COND_SIZE]);
        word(cond, COND_SIZE, 8).store(clock, Ordering::Relaxed);
        0
    }

    fn pthread_cond_destroy(thread: *mut Thread, cond: Ptr) -> c_int {
        let _ = word(cond, COND_SIZE, 0);
        0
    }

    fn pthread_cond_wait(thread: *mut Thread, cond: Ptr, mutex: Ptr) -> c_int {
        unsafe { cond_wait(thread, cond, mutex, None) }
    }

    fn pthread_cond_timedwait(thread: *mut Thread, cond: Ptr, mutex: Ptr, time: Ptr) -> c_int {
        match deadline(cond_clock(cond), time) {
            Ok(deadline) => unsafe { cond_wait(thread, cond, mutex, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_cond_clockwait(thread: *mut Thread, cond: Ptr, mutex: Ptr, clock: c_int, time: Ptr) -> c_int {
        match deadline(clock, time) {
            Ok(deadline) => unsafe { cond_wait(thread, cond, mutex, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_cond_signal(thread: *mut Thread, cond: Ptr) -> c_int {
        let sequence = word(cond, COND_SIZE, 0);
        sequence.fetch_add(1, Ordering::Release);
        wake(sequence, 1);
        0
    }

    fn pthread_cond_broadcast(thread: *mut Thread, cond: Ptr) -> c_int {
        let sequence = word(cond, COND_SIZE, 0);
        sequence.fetch_add(1, Ordering::Release);
        wake(sequence, i32::MAX);
        0
    }

    fn pthread_condattr_init(thread: *mut Thread, attr: Ptr) -> c_int {
        write_u32(attr, libc::CLOCK_REALTIME as u32);
        0
    }

    fn pthread_condattr_destroy(thread: *mut Thread, attr: Ptr) -> c_int {
        let _ = read_u32(attr);
        0
    }

    fn pthread_condattr_setclock(thread: *mut Thread, attr: Ptr, clock: c_int) -> c_int {
        if clock != libc::CLOCK_REALTIME && clock != libc::CLOCK_MONOTONIC {
            return libc::EINVAL;
        }
        write_u32(attr, clock as u32);
        0
    }

    fn pthread_condattr_getclock(thread: *mut Thread, attr: Ptr, clock: Ptr) -> c_int {
        give_int(clock, read_u32(attr) as c_int)
    }

    fn pthread_condattr_setpshared(thread: *mut Thread, attr: Ptr, shared: c_int) -> c_int {
        let _ = read_u32(attr);
        if shared == libc::PTHREAD_PROCESS_PRIVATE { 0 } else { libc::ENOTSUP }
    }

    fn pthread_rwlock_init(thread: *mut Thread, at: Ptr, attr: Ptr) -> c_int {
        object::check_access(at, RWLOCK_SIZE, Access::Write);
        view::write(at, &[0; RWLOCK_SIZE]);
        0
    }

    fn pthread_rwlock_destroy(thread: *mut Thread, at: Ptr) -> c_int {
        if word(at, RWLOCK_SIZE, 0).load(Ordering::Relaxed) != 0 { libc::EBUSY } else { 0 }
    }

    fn pthread_rwlock_rdlock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { read_lock(thread, at, false, None) }
    }

    fn pthread_rwlock_tryrdlock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { read_lock(thread, at, true, None) }
    }

    fn pthread_rwlock_timedrdlock(thread: *mut Thread, at: Ptr, time: Ptr) -> c_int {
        match deadline(libc::CLOCK_REALTIME, time) {
            Ok(deadline) => unsafe { read_lock(thread, at, false, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_rwlock_wrlock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { write_lock(thread, at, false, None) }
    }

    fn pthread_rwlock_trywrlock(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { write_lock(thread, at, true, None) }
    }

    fn pthread_rwlock_timedwrlock(thread: *mut Thread, at: Ptr, time: Ptr) -> c_int {
        match deadline(libc::CLOCK_REALTIME, time) {
            Ok(deadline) => unsafe { write_lock(thread, at, false, Some(deadline)) },
            Err(error) => error,
        }
    }

    fn pthread_rwlock_unlock(thread: *mut Thread, at: Ptr) -> c_int {
        let (state, sequence) = (word(at, RWLOCK_SIZE, 0), word(at, RWLOCK_SIZE, 4));
        match state.load(Ordering::Relaxed) {
            0 => return libc::EPERM,
            u32::MAX => state.store(0, Ordering::Release),
            _ => {
                state.fetch_sub(1, Ordering::Release);
            }
        }
        sequence.fetch_add(1, Ordering::Release);
        wake(sequence, i32::MAX);
        0
    }

    fn pthread_rwlockattr_init(thread: *mut Thread, attr: Ptr) -> c_int {
        view::write(attr, &[0; 8]);
        0
    }

    fn pthread_rwlockattr_destroy(thread: *mut Thread, attr: Ptr) -> c_int {
        let _ = read_u32(attr);
        0
    }

    // 0 to start, 1 while the first caller runs it, 2 once it's run. An
    // exception puts it back to 0 and goes on through.
    fn pthread_once(thread: *mut Thread, once: Ptr, function: Ptr) -> Thrown<c_int> {
        let state = word(once, 4, 0);
        loop {
            match state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire) {
                Ok(_) => {
                    let ran = unsafe { call_back(thread, function, &[]) }.is_some();
                    state.store(if ran { 2 } else { 0 }, Ordering::Release);
                    wake(state, i32::MAX);
                    return if ran { Thrown::Value(0) } else { Thrown::Exception };
                }
                Err(2) => return Thrown::Value(0),
                Err(_) => {
                    unsafe { thread::track(thread, once.object()) };
                    unsafe { wait(thread, state, 1, None) };
                }
            }
        }
    }

    fn pthread_key_create(thread: *mut Thread, out: Ptr, destructor: Ptr) -> c_int {
        if !destructor.ptr.is_null() {
            object::check_function_call(destructor);
        }
        object::check_access(out, 4, Access::Write);
        let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
        let index = match keys.iter().position(|key| !key.used) {
            Some(index) => index,
            None if keys.len() < KEYS_MAX => {
                keys.push(Key { used: false, destructor: (0, 0), generation: 0 });
                keys.len() - 1
            }
            None => return libc::EAGAIN,
        };
        keys[index].used = true;
        keys[index].destructor = pair(destructor);
        drop(keys);
        write_u32(out, index as u32);
        0
    }

    fn pthread_key_delete(thread: *mut Thread, key: u32) -> c_int {
        let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
        match keys.get_mut(key as usize).filter(|key| key.used) {
            Some(key) => {
                *key = Key { used: false, destructor: (0, 0), generation: key.generation + 1 };
                0
            }
            None => libc::EINVAL,
        }
    }

    fn pthread_getspecific(thread: *mut Thread, key: u32) -> Ptr {
        let generation = match KEYS.lock().unwrap_or_else(|e| e.into_inner()).get(key as usize) {
            Some(key) if key.used => key.generation,
            _ => return Ptr::NULL,
        };
        match unsafe { thread::extra(thread) }.libc.keys.get(key as usize) {
            Some(&(theirs, value)) if theirs == generation => value,
            _ => Ptr::NULL,
        }
    }

    fn pthread_setspecific(thread: *mut Thread, key: u32, value: Ptr) -> c_int {
        let generation = match KEYS.lock().unwrap_or_else(|e| e.into_inner()).get(key as usize) {
            Some(key) if key.used => key.generation,
            _ => return libc::EINVAL,
        };
        let values = &mut unsafe { thread::extra(thread) }.libc.keys;
        if values.len() <= key as usize {
            values.resize(key as usize + 1, (0, Ptr::NULL));
        }
        values[key as usize] = (generation, value);
        0
    }

    fn pthread_atfork(thread: *mut Thread, prepare: Ptr, parent: Ptr, child: Ptr) -> c_int {
        at_fork(prepare, parent, child)
    }

    fn __register_atfork(thread: *mut Thread, prepare: Ptr, parent: Ptr, child: Ptr, dso: Ptr) -> c_int {
        at_fork(prepare, parent, child)
    }

    fn pthread_spin_init(thread: *mut Thread, at: Ptr, shared: c_int) -> c_int {
        word(at, 4, 0).store(0, Ordering::Relaxed);
        0
    }

    fn pthread_spin_destroy(thread: *mut Thread, at: Ptr) -> c_int {
        let _ = word(at, 4, 0);
        0
    }

    fn pthread_spin_lock(thread: *mut Thread, at: Ptr) -> c_int {
        let lock = word(at, 4, 0);
        let mut spins = 0u32;
        while lock.compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            if spins.is_multiple_of(128) {
                unsafe { plain(thread, || libc::sched_yield()) };
            } else {
                std::hint::spin_loop();
            }
        }
        0
    }

    fn pthread_spin_trylock(thread: *mut Thread, at: Ptr) -> c_int {
        if word(at, 4, 0).compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() { 0 } else { libc::EBUSY }
    }

    fn pthread_spin_unlock(thread: *mut Thread, at: Ptr) -> c_int {
        word(at, 4, 0).store(0, Ordering::Release);
        0
    }

    // How many threads it takes, how many have come, and a generation that
    // moves on as the last one comes.
    fn pthread_barrier_init(thread: *mut Thread, at: Ptr, attr: Ptr, count: u32) -> c_int {
        if count == 0 {
            return libc::EINVAL;
        }
        object::check_access(at, BARRIER_SIZE, Access::Write);
        view::write(at, &[0; BARRIER_SIZE]);
        word(at, BARRIER_SIZE, 0).store(count, Ordering::Relaxed);
        0
    }

    fn pthread_barrier_destroy(thread: *mut Thread, at: Ptr) -> c_int {
        let _ = word(at, BARRIER_SIZE, 0);
        0
    }

    fn pthread_barrier_wait(thread: *mut Thread, at: Ptr) -> c_int {
        let (count, arrived, generation) = (word(at, BARRIER_SIZE, 0), word(at, BARRIER_SIZE, 4), word(at, BARRIER_SIZE, 8));
        let seen = generation.load(Ordering::Acquire);
        if arrived.fetch_add(1, Ordering::AcqRel) + 1 >= count.load(Ordering::Relaxed) {
            arrived.store(0, Ordering::Relaxed);
            generation.fetch_add(1, Ordering::Release);
            wake(generation, i32::MAX);
            return libc::PTHREAD_BARRIER_SERIAL_THREAD;
        }
        unsafe { thread::track(thread, at.object()) };
        while generation.load(Ordering::Acquire) == seen {
            unsafe { wait(thread, generation, seen, None) };
        }
        0
    }

    fn pthread_barrierattr_init(thread: *mut Thread, attr: Ptr) -> c_int {
        write_u32(attr, 0);
        0
    }

    fn pthread_barrierattr_destroy(thread: *mut Thread, attr: Ptr) -> c_int {
        let _ = read_u32(attr);
        0
    }

    fn sem_init(thread: *mut Thread, at: Ptr, shared: c_int, value: u32) -> c_int {
        if value > i32::MAX as u32 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        object::check_access(at, SEM_SIZE, Access::Write);
        view::write(at, &[0; SEM_SIZE]);
        word(at, SEM_SIZE, 0).store(value, Ordering::Relaxed);
        0
    }

    fn sem_destroy(thread: *mut Thread, at: Ptr) -> c_int {
        let _ = word(at, SEM_SIZE, 0);
        0
    }

    fn sem_post(thread: *mut Thread, at: Ptr) -> c_int {
        let value = word(at, SEM_SIZE, 0);
        if value.fetch_add(1, Ordering::Release) == i32::MAX as u32 {
            value.fetch_sub(1, Ordering::Relaxed);
            unsafe { set_errno(thread, libc::EOVERFLOW) };
            return -1;
        }
        wake(value, 1);
        0
    }

    fn sem_wait(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { sem_take(thread, at, false, None) }
    }

    fn sem_trywait(thread: *mut Thread, at: Ptr) -> c_int {
        unsafe { sem_take(thread, at, true, None) }
    }

    fn sem_timedwait(thread: *mut Thread, at: Ptr, time: Ptr) -> c_int {
        match deadline(libc::CLOCK_REALTIME, time) {
            Ok(deadline) => unsafe { sem_take(thread, at, false, Some(deadline)) },
            Err(error) => sem_result(thread, error),
        }
    }

    fn sem_clockwait(thread: *mut Thread, at: Ptr, clock: c_int, time: Ptr) -> c_int {
        match deadline(clock, time) {
            Ok(deadline) => unsafe { sem_take(thread, at, false, Some(deadline)) },
            Err(error) => sem_result(thread, error),
        }
    }

    fn sem_getvalue(thread: *mut Thread, at: Ptr, out: Ptr) -> c_int {
        give_int(out, word(at, SEM_SIZE, 0).load(Ordering::Relaxed) as c_int)
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated___pthread_key_create", pthread_key_create::getter as *const ()),
        ("pizlonated___pthread_getspecific", pthread_getspecific::getter as *const ()),
        ("pizlonated___pthread_setspecific", pthread_setspecific::getter as *const ()),
        ("pizlonated___pthread_once", pthread_once::getter as *const ()),
        ("pizlonated___pthread_mutex_lock", pthread_mutex_lock::getter as *const ()),
        ("pizlonated___pthread_mutex_unlock", pthread_mutex_unlock::getter as *const ()),
        ("pizlonated___pthread_mutex_trylock", pthread_mutex_trylock::getter as *const ()),
        ("pizlonated___pthread_mutex_init", pthread_mutex_init::getter as *const ()),
        ("pizlonated___pthread_mutex_destroy", pthread_mutex_destroy::getter as *const ()),
        ("pizlonated_pthread_mutexattr_setkind_np", pthread_mutexattr_settype::getter as *const ()),
        ("pizlonated_pthread_mutexattr_getkind_np", pthread_mutexattr_gettype::getter as *const ()),
        ("pizlonated_pthread_mutexattr_setrobust_np", pthread_mutexattr_setrobust::getter as *const ()),
        ("pizlonated___pthread_rwlock_rdlock", pthread_rwlock_rdlock::getter as *const ()),
        ("pizlonated___pthread_rwlock_wrlock", pthread_rwlock_wrlock::getter as *const ()),
        ("pizlonated___pthread_rwlock_unlock", pthread_rwlock_unlock::getter as *const ()),
    ]
}
