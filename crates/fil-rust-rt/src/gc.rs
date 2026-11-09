//! The collector: stopping the world, marking from precise roots, and sweeping.
//!
//! There's no collector thread. Whoever needs a collection runs it, with every
//! thread, itself included, exited. A thread can be exited at any safepoint, so
//! all a collection needs is for the rest to reach one: an entered thread
//! stops at its next pollcheck, and an exited one can't enter until it's over.
//!
//! Roots are exact, and nothing here looks at machine stacks. What compiled
//! code holds across a safepoint is in its frames, and what Rust holds is
//! tracked. The CC buffers aren't roots, which is why this must only run at a
//! safepoint: see [`check`] and [`collect`], the only ways in. In particular
//! nothing reached from `verse_local_allocator_allocate` or the
//! `_without_exiting` helpers may lead here. The allocator can only ask, by
//! setting the Check bit on its own thread.
//!
//! This is ported from FUGC, whose marking rules it follows, including the
//! redirection of references to freed objects, without which those objects
//! could never go away.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use crate::abi::*;
use crate::failure::internal_panic;
use crate::heap::heap;
use crate::thread::{self, extra};
use crate::{globals, maps, ptrs, shim, start, tables};

/// When collections happen. A collection is the only thing that gives memory
/// back, `free` included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// Never trace, so nothing is reclaimed.
    Never,
    /// Trace when Rust asks for it, and at no other time.
    Manual,
    /// Also trace once the program has allocated `min_bytes` since the last
    /// collection, or `growth_percent` of what that collection left alive,
    /// whichever is more.
    Threshold { min_bytes: usize, growth_percent: u32 },
}

impl Default for Collection {
    fn default() -> Collection {
        Collection::Threshold { min_bytes: 32 << 20, growth_percent: 100 }
    }
}

/// The state of the heap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Collections so far.
    pub collections: u64,
    /// Bytes in blocks the last collection found reachable. Includes
    /// `tombstoned_bytes`.
    pub live_bytes: usize,
    /// Bytes in freed objects that something still referred to in the last
    /// collection, from a place that can't be redirected.
    pub tombstoned_bytes: usize,
    /// Bytes the last collection reclaimed.
    pub reclaimed_bytes: usize,
    /// Bytes every collection so far reclaimed.
    pub total_reclaimed_bytes: usize,
    /// Bytes handed out since the last collection.
    pub allocated_bytes: usize,
    /// Bytes in chunks that are in use.
    pub committed_bytes: usize,
}

struct Collector {
    policy: Collection,
    stats: HeapStats,
}

/// Held only for a moment, with signals blocked, and never while waiting: an
/// entered thread can take it, so a collection can't wait for it while the
/// world is running.
static COLLECTOR: Mutex<Collector> = Mutex::new(Collector {
    policy: Collection::Threshold { min_bytes: 32 << 20, growth_percent: 100 },
    stats: HeapStats {
        collections: 0,
        live_bytes: 0,
        tombstoned_bytes: 0,
        reclaimed_bytes: 0,
        total_reclaimed_bytes: 0,
        allocated_bytes: 0,
        committed_bytes: 0,
    },
});

/// Threads that are waiting for a resume, and those a resume woke that have yet
/// to enter. A stop waits for the second kind: otherwise a thread collecting
/// often could stop the world again before any of the others got to run.
struct World {
    waiting: usize,
    owed: usize,
}

/// Also serves as the lock for `STOPPED`, `RESUMED` and `CAUGHT_UP`.
static WORLD: Mutex<World> = Mutex::new(World { waiting: 0, owed: 0 });
static STOPPED: Condvar = Condvar::new();
static RESUMED: Condvar = Condvar::new();
static CAUGHT_UP: Condvar = Condvar::new();

fn lock_world() -> MutexGuard<'static, World> {
    WORLD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes allocated since the last collection that call for a new one.
static THRESHOLD: AtomicUsize = AtomicUsize::new(32 << 20);

/// Collections finished, for `zgc_wait`.
static COMPLETED: AtomicU64 = AtomicU64::new(0);

/// `FIL_RUST_GC_STRESS=n`: collect at every nth safepoint, 0 being never.
static STRESS: AtomicUsize = AtomicUsize::new(0);
static STRESS_POLLS: AtomicUsize = AtomicUsize::new(0);

/// `FIL_RUST_GC_STRESS_ALLOCATIONS=1`: collect at the first safepoint after
/// any allocation, whatever the policy, as libpizlo's `FUGC_MIN_THRESHOLD=0`
/// does. Unlike [`STRESS`], a loop that doesn't allocate runs at full speed.
static STRESS_ALLOCATIONS: AtomicBool = AtomicBool::new(false);

/// Reads the settings from the environment.
pub fn configure_from_env() {
    if let Some(every) = std::env::var("FIL_RUST_GC_STRESS").ok().and_then(|value| value.parse().ok()) {
        STRESS.store(every, Ordering::Release);
    }
    if std::env::var("FIL_RUST_GC_STRESS_ALLOCATIONS").as_deref() == Ok("1") {
        STRESS_ALLOCATIONS.store(true, Ordering::Release);
        THRESHOLD.store(1, Ordering::Release);
    }
}

fn is_stressed() -> bool {
    STRESS.load(Ordering::Relaxed) != 0
}

fn lock_collector() -> MutexGuard<'static, Collector> {
    COLLECTOR.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_collector<R>(body: impl FnOnce(&mut Collector) -> R) -> R {
    let _blocked = crate::signal::Blocked::all();
    body(&mut lock_collector())
}

/// Whose turn it is to collect, or to fork. Turns go in the order they were
/// asked for, since a thread collecting often would otherwise keep taking
/// them back. Only exited threads take one, with signals blocked.
struct Turns {
    next: u64,
    serving: u64,
}

static TURNS: Mutex<Turns> = Mutex::new(Turns { next: 0, serving: 0 });
static TURN: Condvar = Condvar::new();

fn lock_turns() -> MutexGuard<'static, Turns> {
    TURNS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Turn;

/// Waits for this thread's turn among collections and whatever else stops the
/// world or changes the thread list, which is first come, first served: a
/// thread that keeps collecting can't keep others from attaching.
pub(crate) fn take_turn() -> Turn {
    let mut turns = lock_turns();
    let mine = turns.next;
    turns.next += 1;
    while turns.serving != mine {
        turns = TURN.wait(turns).unwrap_or_else(|e| e.into_inner());
    }
    Turn
}

impl Drop for Turn {
    fn drop(&mut self) {
        lock_turns().serving += 1;
        TURN.notify_all();
    }
}

fn threshold_for(policy: Collection, live: usize) -> usize {
    if STRESS_ALLOCATIONS.load(Ordering::Relaxed) {
        return 1;
    }
    match policy {
        Collection::Never | Collection::Manual => usize::MAX,
        Collection::Threshold { min_bytes, growth_percent } => {
            min_bytes.max(live / 100 * growth_percent as usize).max(1)
        }
    }
}

/// Changes when collections happen, from now on.
pub fn set_collection(policy: Collection) {
    with_collector(|collector| {
        collector.policy = policy;
        THRESHOLD.store(threshold_for(policy, collector.stats.live_bytes), Ordering::Release);
    });
}

pub fn collection() -> Collection {
    with_collector(|collector| collector.policy)
}

/// Where the heap stands now.
pub fn stats() -> HeapStats {
    let stats = with_collector(|collector| collector.stats);
    HeapStats {
        allocated_bytes: heap().counters.allocated_since_collection.load(Ordering::Relaxed),
        committed_bytes: heap().counters.committed.load(Ordering::Relaxed),
        ..stats
    }
}

/// Called as allocation goes on, on the thread that allocated, possibly under
/// the heap's lock and possibly not at a safepoint. Takes no locks and starts
/// nothing: the thread gets to it at its next pollcheck.
#[inline]
pub fn note_allocated(total: usize) {
    if total >= THRESHOLD.load(Ordering::Relaxed) {
        unsafe { arm(thread::current()) };
    }
}

/// Sends the thread into [`check`] at its next pollcheck.
pub unsafe fn arm(thread: *mut Thread) {
    if !thread.is_null() {
        unsafe { (*thread).state.fetch_or(STATE_CHECK_REQUESTED, Ordering::AcqRel) };
    }
}

/// Keeps a thread collecting under stress, from when it attaches on.
pub unsafe fn rearm_under_stress(thread: *mut Thread) {
    if is_stressed() {
        unsafe { arm(thread) };
    }
}

/// The thread is at a safepoint and has a Check bit to answer.
pub unsafe fn check(thread: *mut Thread) {
    let every = STRESS.load(Ordering::Relaxed);
    if every != 0 {
        if STRESS_POLLS.fetch_add(1, Ordering::Relaxed).is_multiple_of(every) {
            unsafe { collect_if(thread, |_| true) };
        }
        return;
    }
    if heap().counters.allocated_since_collection.load(Ordering::Relaxed) >= THRESHOLD.load(Ordering::Relaxed) || is_requested() {
        // Someone else may have dealt with it by the time it's our turn.
        unsafe {
            collect_if(thread, |collector| {
                heap().counters.allocated_since_collection.load(Ordering::Relaxed)
                    >= threshold_for(collector.policy, collector.stats.live_bytes)
                    || is_requested() && matches!(collector.policy, Collection::Threshold { .. })
            })
        };
    }
}

/// Collects now, on Rust's request. `thread` is the caller's, which has to be
/// at a safepoint if it's entered, and may be null. Does nothing under
/// [`Collection::Never`].
pub unsafe fn collect(thread: *mut Thread) {
    unsafe { collect_if(thread, |collector| collector.policy != Collection::Never) };
}

/// The collection `zgc_wait` should wait for, to see everything up to now.
pub fn request_fresh() -> u64 {
    COMPLETED.load(Ordering::Acquire) + 1
}

/// Collections finished so far.
pub fn completed() -> u64 {
    COMPLETED.load(Ordering::Acquire)
}

/// The last collection `zgc_try_request` asked for.
static REQUESTED: AtomicU64 = AtomicU64::new(0);

fn is_requested() -> bool {
    REQUESTED.load(Ordering::Acquire) > COMPLETED.load(Ordering::Acquire)
}

/// Asks for a collection at the thread's next safepoint, if Fil-C code gets to
/// start them, and returns its number.
pub unsafe fn try_request(thread: *mut Thread) -> u64 {
    let cycle = request_fresh();
    if matches!(collection(), Collection::Threshold { .. }) {
        REQUESTED.fetch_max(cycle, Ordering::AcqRel);
        unsafe { arm(thread) };
    }
    cycle
}

/// The last collection asked for that hasn't been turned down: the one running
/// or about to, if it's past [`completed`].
pub fn requested() -> u64 {
    REQUESTED.load(Ordering::Acquire).max(completed())
}

/// Whether sweeping scribbles over what it frees, and marking checks what it
/// finds, which stress mode does.
pub fn is_checking() -> bool {
    is_stressed() || STRESS_ALLOCATIONS.load(Ordering::Relaxed)
}

/// Runs `body` with every thread stopped, the way a collection does, so their
/// frames hold still. The rules for `thread` are those of [`collect`]. `body`
/// mustn't allocate Fil-C memory.
pub unsafe fn with_world_stopped(thread: *mut Thread, body: impl FnOnce(&[*mut Thread])) {
    let entered = !thread.is_null() && unsafe { crate::helpers::is_entered(thread) };
    if entered {
        unsafe { thread::exit(thread) };
    }
    let blocked = crate::signal::Blocked::all();
    let turn = take_turn();
    {
        let threads = thread::lock_threads();
        stop_the_world(&threads.0);
        body(&threads.0.iter().map(|&entry| thread_at(entry)).collect::<Vec<_>>());
        resume_the_world(&threads.0);
    }
    drop(turn);
    drop(blocked);
    if entered {
        unsafe { thread::enter(thread) };
    }
}

/// Returns once `cycle` collections are done. Fil-C code doesn't get to start
/// collections unless the policy is to run them on its own.
pub unsafe fn wait_for(thread: *mut Thread, cycle: u64) {
    while COMPLETED.load(Ordering::Acquire) < cycle {
        if !matches!(collection(), Collection::Threshold { .. }) {
            return;
        }
        unsafe { collect_if(thread, |_| COMPLETED.load(Ordering::Acquire) < cycle) };
    }
}

unsafe fn collect_if(thread: *mut Thread, wanted: impl FnOnce(&Collector) -> bool) {
    let entered = !thread.is_null() && unsafe { crate::helpers::is_entered(thread) };
    if entered {
        unsafe { thread::exit(thread) };
    }
    // This thread looks exited to a signal, but a handler on top of a collection
    // would wait for the collection to end.
    let blocked = crate::signal::Blocked::all();
    let turn = take_turn();
    if wanted(&lock_collector()) {
        on_collector_stack();
    }
    drop(turn);
    drop(blocked);
    if entered {
        unsafe { thread::enter(thread) };
    }
}

/// How much stack a collection gets. Marking is iterative and needs little, but
/// the thread may have come here with its stack nearly used up.
const COLLECTOR_STACK_SIZE: usize = 1 << 20;

fn on_collector_stack() {
    static STACK: OnceLock<usize> = OnceLock::new();
    let base = *STACK.get_or_init(|| Box::leak(vec![0u8; COLLECTOR_STACK_SIZE].into_boxed_slice()).as_mut_ptr() as usize);
    let top = (base + COLLECTOR_STACK_SIZE) & !15;

    unsafe extern "C" fn body(_: *mut c_void) {
        run();
    }
    let argument = std::ptr::null_mut::<c_void>();
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "mov r12, rsp",
            "mov rsp, {top}",
            "call {body}",
            "mov rsp, r12",
            top = in(reg) top,
            body = in(reg) body as unsafe extern "C" fn(*mut c_void),
            in("rdi") argument,
            out("r12") _,
            clobber_abi("C"),
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "mov x20, sp",
            "mov sp, {top}",
            "blr {body}",
            "mov sp, x20",
            top = in(reg) top,
            body = in(reg) body as unsafe extern "C" fn(*mut c_void),
            in("x0") argument,
            out("x20") _,
            clobber_abi("C"),
        );
    }
}

fn thread_at(entry: usize) -> *mut Thread {
    entry as *mut Thread
}

/// Stops every thread: they're exited, and stay that way.
fn stop_the_world(threads: &[usize]) {
    let mut world = lock_world();
    while world.owed != 0 {
        world = CAUGHT_UP.wait(world).unwrap_or_else(|e| e.into_inner());
    }
    for &entry in threads {
        unsafe { (*thread_at(entry)).state.fetch_or(STATE_STOP_REQUESTED, Ordering::AcqRel) };
    }
    while threads.iter().any(|&entry| unsafe { (*thread_at(entry)).state.load(Ordering::Acquire) } & STATE_ENTERED != 0) {
        world = STOPPED.wait(world).unwrap_or_else(|e| e.into_inner());
    }
}

fn resume_the_world(threads: &[usize]) {
    let mut world = lock_world();
    for &entry in threads {
        unsafe { (*thread_at(entry)).state.fetch_and(!STATE_STOP_REQUESTED, Ordering::AcqRel) };
    }
    world.owed = world.waiting;
    drop(world);
    RESUMED.notify_all();
}

/// A thread that was stopping exited.
pub fn note_stopped() {
    let _blocked = crate::signal::Blocked::all();
    drop(lock_world());
    STOPPED.notify_all();
}

/// Waits for the end of the collection that stopped `thread`. If this had to
/// wait, it returns true, and no one can stop the world again until the thread
/// enters and calls [`caught_up`].
pub unsafe fn wait_for_resume(thread: *mut Thread) -> bool {
    let _blocked = crate::signal::Blocked::all();
    let stopped = || unsafe { (*thread).state.load(Ordering::Acquire) } & STATE_STOP_REQUESTED != 0;
    let mut world = lock_world();
    if !stopped() {
        return false;
    }
    world.waiting += 1;
    while stopped() {
        world = RESUMED.wait(world).unwrap_or_else(|e| e.into_inner());
    }
    world.waiting -= 1;
    true
}

/// A thread that [`wait_for_resume`] made wait has entered.
pub fn caught_up() {
    let _blocked = crate::signal::Blocked::all();
    let mut world = lock_world();
    world.owed -= 1;
    let done = world.owed == 0;
    drop(world);
    if done {
        CAUGHT_UP.notify_all();
    }
}

/// Forks with the runtime still, so that nothing is left behind half done or
/// holding a lock, and has the child forget the other threads. Takes an entered
/// thread and leaves it entered. Returns what `fork` did, with its `errno`.
pub unsafe fn fork(thread: *mut Thread) -> (libc::pid_t, i32) {
    unsafe {
        // Waits for whoever is initializing a global, so needs doing now.
        globals::lock(thread);
        thread::exit(thread);
    }
    let blocked = crate::signal::Blocked::all();
    let turn = take_turn();
    let mut threads = thread::lock_threads();
    stop_the_world(&threads.0);
    for &entry in &threads.0 {
        for slot in unsafe { &mut (*thread_at(entry)).slots } {
            heap().flush_slot(slot);
        }
    }
    // Exited threads can take these whenever they like. If one had any of
    // them as the fork happened, no one would ever release it in the child.
    let lifecycle = thread::lock_lifecycle();
    let mut world = lock_world();
    let mut turns = lock_turns();
    let (result, error) = {
        let _roots = (shim::hold(), start::hold(), globals::hold(), lock_collector());
        let _heap = heap().hold();
        unsafe {
            *libc::__errno_location() = 0;
            let result = libc::fork();
            (result, *libc::__errno_location())
        }
    };
    if result == 0 {
        for &entry in &threads.0 {
            let other = thread_at(entry);
            if other != thread {
                unsafe {
                    (*other).host = 0;
                    (*other).tid = 0;
                    (*other).forked.store(1, Ordering::Relaxed);
                    (*other).stopped.store(1, Ordering::Release);
                }
            }
        }
        threads.0.clear();
        threads.0.push(thread as usize);
        // Whoever was waiting is gone.
        world.waiting = 0;
        turns.next = turns.serving + 1;
        unsafe { (*thread).tid = libc::gettid() as u32 };
        crate::signal::set_main(thread);
    }
    drop(turns);
    drop(world);
    drop(lifecycle);
    resume_the_world(&threads.0);
    drop(threads);
    drop(turn);
    drop(blocked);
    unsafe { thread::enter(thread) };
    globals::unlock();
    (result, error)
}

struct Marker {
    stack: Vec<*mut Object>,
    tombstoned: usize,
    verify: bool,
    /// What the census has to see to, as marking found them.
    weaks: Vec<*mut Object>,
    weak_maps: Vec<*mut Object>,
    weak_tables: Vec<*mut Object>,
}

impl Marker {
    /// Marks what a reference refers to, from a place that keeps referring to
    /// it even if it's been freed.
    unsafe fn mark(&mut self, object: *mut Object) {
        if object.is_null() {
            return;
        }
        let aux = unsafe { (*object).aux.load(Ordering::Relaxed) };
        let flags = aux_flags(aux);
        if flags & FLAG_GLOBAL != 0 {
            return;
        }
        if !heap().contains(object as usize) {
            internal_panic(format_args!("object {object:p} is outside the heap, and isn't global"));
        }
        let (block, size) = heap().block_of(object as usize);
        if self.verify {
            unsafe { verify_object(object, block, size) };
        }
        if !heap().mark(block) {
            return;
        }
        if flags & FLAG_FREE != 0 {
            self.tombstoned += size;
            // Whatever the aux referred to goes in this collection, and
            // nothing can get past the object's empty bounds to see it.
            unsafe { (*object).aux.store(aux_create(flags, std::ptr::null_mut()), Ordering::Relaxed) };
            return;
        }
        if !aux_ptr(aux).is_null() || flags & FLAGS_SPECIAL_MASK != 0 {
            self.stack.push(object);
        }
    }

    /// Marks an object from the list of roots. Global objects aren't collected,
    /// but they have outgoing pointers, and the rest of them are never marked.
    unsafe fn root(&mut self, object: *mut Object) {
        if object.is_null() {
            return;
        }
        if unsafe { (*object).flags() } & FLAG_GLOBAL == 0 {
            unsafe { self.mark(object) };
            return;
        }
        // Some are in the heap, for want of anywhere else to put them.
        if heap().contains(object as usize) {
            heap().mark(heap().block_of(object as usize).0);
        }
        self.stack.push(object);
    }

    /// Marks what a `Ptr` in memory refers to. If it's been freed, the `Ptr`
    /// lets go of the object in favor of one that's always free.
    unsafe fn mark_or_free_flight(&mut self, slot: *mut Ptr) {
        let lower = unsafe { (*slot).lower };
        if lower.is_null() {
            return;
        }
        let object = object_for_lower(lower);
        if unsafe { (*object).flags() } & FLAG_FREE == 0 {
            unsafe { self.mark(object) };
        } else if object != crate::object::free_singleton() {
            unsafe { (*slot).lower = lower_for_object(crate::object::free_singleton()) };
        }
    }

    /// The same for an entry of an aux array, which is a `lower`, or an atomic
    /// box holding a `Ptr`.
    unsafe fn mark_or_free_entry(&mut self, entry: &AtomicUsize) {
        let value = entry.load(Ordering::Relaxed);
        if value == 0 {
            return;
        }
        if ptrs::is_box(value) {
            let target = ptrs::box_of(value);
            heap().mark(heap().block_of(target as usize).0);
            unsafe { self.mark_or_free_flight(target) };
            return;
        }
        let object = object_for_lower(value as *mut u8);
        if unsafe { (*object).flags() } & FLAG_FREE == 0 {
            unsafe { self.mark(object) };
        } else if object != crate::object::free_singleton() {
            entry.store(lower_for_object(crate::object::free_singleton()) as usize, Ordering::Relaxed);
        }
    }

    unsafe fn scan(&mut self, object: *mut Object) {
        let this = unsafe { &*object };
        let flags = this.flags();
        match this.special_type() {
            0 => {}
            SPECIAL_FUNCTION => {
                // Global ones are plain functions, and only closures have more.
                if flags & FLAG_READONLY == 0 {
                    let closure = this.aux_ptr().cast::<Closure>();
                    unsafe { self.mark_or_free_flight(&raw mut (*closure).data) };
                }
                return;
            }
            SPECIAL_THREAD => {
                // This much of a thread is still of use once it's gone. A
                // running thread gets the rest from `mark_thread`.
                let thread = lower_for_object(object).cast::<Thread>();
                unsafe {
                    self.mark_or_free_flight(&raw mut (*thread).cookie);
                    self.mark_or_free_flight(&raw mut (*thread).result);
                }
                return;
            }
            SPECIAL_DL_HANDLE => return,
            SPECIAL_SIGNAL_HANDLER => {
                let handler = lower_for_object(object).cast::<crate::signal::Handler>();
                unsafe { self.mark_or_free_flight(&raw mut (*handler).function) };
                return;
            }
            SPECIAL_JMP_BUF => {
                let buffer = lower_for_object(object).cast::<crate::jump::JmpBuf>();
                unsafe { crate::jump::for_each_lower(buffer, |lower| self.mark(object_for_lower(lower))) };
                return;
            }
            SPECIAL_PTR_TABLE => {
                unsafe { tables::collect::scan_ptr_table(object, |target| self.mark(target)) };
                return;
            }
            SPECIAL_EXACT_PTR_TABLE => {
                if !unsafe { tables::collect::scan_exact_table(object, |target| self.mark(target)) } {
                    self.weak_tables.push(object);
                }
                return;
            }
            SPECIAL_WEAK => {
                self.weaks.push(object);
                return;
            }
            SPECIAL_WEAK_MAP => {
                // Its values get marked once it's known which keys are live.
                self.weak_maps.push(object);
                return;
            }
            SPECIAL_FINALIZER_QUEUE => {
                unsafe { tables::collect::scan_finalizer_queue(object, |target| self.mark(target)) };
                return;
            }
            special => internal_panic(format_args!("the collector can't scan special objects of type {special}")),
        }
        let aux = this.aux_ptr();
        if aux.is_null() {
            return;
        }
        // The aux of a compiled constant isn't in the heap.
        if heap().contains(aux as usize) {
            heap().mark(heap().block_of(aux as usize).0);
        }
        let size = this.size();
        let mut offset = 0;
        while offset < size {
            unsafe { self.mark_or_free_entry(crate::object::aux_word(aux, offset)) };
            offset += WORD_SIZE;
        }
    }

    unsafe fn mark_frames(&mut self, mut frame: *mut Frame) {
        while !frame.is_null() {
            unsafe {
                let origin = &*function_origin((*frame).origin);
                let count = origin.base.num_lowers_ish as usize;
                let stack_aux_end = count - origin.num_setjmps as usize;
                let stack_aux_begin = stack_aux_end - origin.num_stack_auxes as usize;
                let lowers = (&raw const (*frame).lowers).cast::<*mut u8>();
                for index in 0..count {
                    let lower = *lowers.add(index);
                    if !(stack_aux_begin..stack_aux_end).contains(&index) {
                        self.mark(object_for_lower(lower));
                    } else if !lower.is_null() {
                        let stack_aux = lower.cast::<StackAux>();
                        let lowers = (&raw const (*stack_aux).lowers).cast::<*mut u8>();
                        for index in 0..(*stack_aux).num_lowers {
                            self.mark(object_for_lower(*lowers.add(index)));
                        }
                    }
                }
                frame = (*frame).parent;
            }
        }
    }

    unsafe fn mark_thread(&mut self, thread: *mut Thread) {
        unsafe {
            self.mark_frames((*thread).top_frame);
            let extra = extra(thread);
            for index in 0..extra.tracked.len() {
                self.mark(extra.tracked[index]);
            }
            for index in 0..extra.thread_locals.len() {
                self.mark(extra.thread_locals[index]);
            }
            extra.libc.for_each_object(|object| self.mark(object));
            self.mark(object_for_lower(thread.cast()));
            let unwinding = &raw mut extra.unwinding;
            self.mark_or_free_flight(&raw mut (*unwinding).context);
            self.mark_or_free_flight(&raw mut (*unwinding).exception);
            self.mark_or_free_flight(&raw mut (*unwinding).stop_callback);
            self.mark_or_free_flight(&raw mut (*unwinding).stop_argument);
        }
    }

    unsafe fn drain(&mut self) {
        while let Some(object) = self.stack.pop() {
            unsafe { self.scan(object) };
        }
    }

    /// Drains, and marks the values of weak maps whose keys turned out to be
    /// live, until that finds nothing more.
    unsafe fn drain_with_ephemerons(&mut self) {
        loop {
            unsafe { self.drain() };
            let mut marked = false;
            let mut index = 0;
            while index < self.weak_maps.len() {
                let map = self.weak_maps[index];
                marked |= unsafe {
                    tables::collect::mark_weak_map_values(map, |target| {
                        let new = (*target).flags() & FLAG_GLOBAL == 0 && !is_marked(target);
                        self.mark(target);
                        new
                    })
                };
                index += 1;
            }
            if !marked && self.stack.is_empty() {
                return;
            }
        }
    }

    /// Clears what weak references, weak maps and weak tables hold that
    /// marking didn't reach.
    unsafe fn census(&mut self) {
        unsafe {
            for &weak in &self.weaks {
                tables::collect::census_weak(weak);
            }
            for &map in &self.weak_maps {
                tables::collect::census_weak_map(map);
            }
            for &table in &self.weak_tables {
                tables::collect::census_exact_table(table);
            }
        }
    }

    /// Marks finalizable objects that nothing reached, and queues them for
    /// their finalizers, if their queues are still around. Weak references to
    /// them are already gone. Whatever this marks gets a census too, as a weak
    /// map only reachable from one of them hasn't had one.
    unsafe fn revive(&mut self) {
        let mut finalizable = FINALIZABLE.lock().unwrap_or_else(|e| e.into_inner());
        let revived: Vec<bool> = finalizable
            .iter()
            .map(|&(object, _)| {
                let object = object as *mut Object;
                let dead = !is_marked(object);
                if dead {
                    unsafe { self.mark(object) };
                }
                dead
            })
            .collect();
        unsafe {
            self.drain_with_ephemerons();
            self.census();
        }
        let mut index = 0;
        finalizable.retain(|&(object, queue)| {
            let (object, queue) = (object as *mut Object, queue as *mut Object);
            let was_revived = revived[index];
            index += 1;
            let queue_lives = is_marked(queue);
            if was_revived && queue_lives {
                unsafe { tables::collect::enqueue(queue, object) };
            }
            // Live with its queue gone, it's just an object now.
            !was_revived && queue_lives
        });
    }
}

/// Objects allocated with a finalizer queue, and the queue, until they're
/// queued. Entered threads are the only ones who add to this.
static FINALIZABLE: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

pub unsafe fn add_finalizable(object: *mut Object, queue: *mut Object) {
    FINALIZABLE.lock().unwrap_or_else(|e| e.into_inner()).push((object as usize, queue as usize));
}

/// Whether this collection has marked `object`, which is in the heap.
pub fn is_marked(object: *mut Object) -> bool {
    heap().is_marked(heap().block_of(object as usize).0)
}

/// Objects that need something done when they go. Blocks have no record of the
/// objects in them, so sweeping couldn't find these.
static DESTRUCTIBLE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Has [`destruct`] see `object`. Entered threads are the only ones who call
/// this, so the lock can't be held by a thread that's stopped.
pub unsafe fn add_destructible(object: *mut Object) {
    DESTRUCTIBLE.lock().unwrap_or_else(|e| e.into_inner()).push(object as usize);
}

/// Does what's needed for objects that this collection found unreachable,
/// while their blocks are intact. The world is stopped, and everything is
/// marked.
unsafe fn destruct() {
    DESTRUCTIBLE.lock().unwrap_or_else(|e| e.into_inner()).retain(|&object| {
        let object = object as *mut Object;
        if is_marked(object) {
            return true;
        }
        unsafe {
            if (*object).flags() & FLAG_MMAP != 0 {
                maps::destruct(object);
            } else {
                tables::destruct(object);
            }
        }
        false
    });
}

/// Fails if `object` can't be a live object in its block: a block that was
/// swept, or a reference that was never one, would show up here. Sweeping
/// scribbles over blocks under stress, which is what makes this catch much.
pub(crate) unsafe fn verify_object(object: *mut Object, block: usize, size: usize) {
    let this = unsafe { &*object };
    let (lower, upper) = (lower_for_object(object) as usize, this.upper() as usize);
    let aux = this.aux_ptr() as usize;
    let good = (object as usize) >= block
        && upper >= lower
        && upper <= block + size
        && matches!(
            this.special_type(),
            0 | SPECIAL_FUNCTION
                | SPECIAL_THREAD
                | SPECIAL_DL_HANDLE
                | SPECIAL_JMP_BUF
                | SPECIAL_SIGNAL_HANDLER
                | SPECIAL_PTR_TABLE
                | SPECIAL_EXACT_PTR_TABLE
                | SPECIAL_WEAK
                | SPECIAL_WEAK_MAP
                | SPECIAL_FINALIZER_QUEUE
        )
        && (aux == 0 || heap().contains(aux) || this.special_type() != 0);
    if !good {
        internal_panic(format_args!(
            "object {object:p} (upper {upper:#x}, aux {aux:#x}) doesn't fit its block at {block:#x} of {size} bytes"
        ));
    }
}

fn run() {
    let threads = thread::lock_threads();
    stop_the_world(&threads.0);
    for &entry in &threads.0 {
        for slot in unsafe { &mut (*thread_at(entry)).slots } {
            heap().flush_slot(slot);
        }
    }
    let stressed = is_checking();
    let mut marker = Marker {
        stack: Vec::new(),
        tombstoned: 0,
        verify: stressed,
        weaks: Vec::new(),
        weak_maps: Vec::new(),
        weak_tables: Vec::new(),
    };
    unsafe {
        globals::for_each_root(|object| marker.root(object));
        shim::for_each_root(|object| marker.mark(object));
        start::for_each_root(|object| marker.mark(object));
        crate::signal::for_each_root(|object| marker.mark(object));
        crate::glibc::for_each_root(|object| marker.mark(object));
        for &entry in &threads.0 {
            marker.mark_thread(thread_at(entry));
        }
        marker.drain_with_ephemerons();
        marker.census();
        marker.revive();
        destruct();
    }
    let (reclaimed, live) = heap().sweep(stressed);
    heap().counters.allocated_since_collection.store(0, Ordering::Relaxed);
    let mut collector = lock_collector();
    let stats = &mut collector.stats;
    stats.collections += 1;
    stats.live_bytes = live;
    stats.tombstoned_bytes = marker.tombstoned;
    stats.reclaimed_bytes = reclaimed;
    stats.total_reclaimed_bytes += reclaimed;
    THRESHOLD.store(threshold_for(collector.policy, live), Ordering::Release);
    drop(collector);
    COMPLETED.fetch_add(1, Ordering::Release);
    resume_the_world(&threads.0);
}
