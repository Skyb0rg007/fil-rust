//! Global variables, which compiled code initializes on first use.
//!
//! A compiled global has a `pizlonated_` getter. Its first caller allocates the
//! object and fills it in, which can touch other globals in a cycle, so a
//! global only becomes visible once everything reachable from it is done. Until
//! then the initializing thread gets at it through the map here, and other
//! threads wait for the lock.

use std::collections::HashMap;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering, fence};

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::object::aux_word;
use crate::ptrs::atomic16;
use crate::thread;

/// What's being initialized, one generation per nested ifunc.
#[derive(Default)]
struct Initialization {
    map: HashMap<usize, (usize, *mut Object)>,
    generations: Vec<Vec<usize>>,
    depth: usize,
}

unsafe impl Send for Initialization {}

/// The thread doing initialization, and so holding the lock.
static OWNER: AtomicUsize = AtomicUsize::new(0);
static STATE: Mutex<Option<Initialization>> = Mutex::new(None);
static ROOTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn with_state<R>(body: impl FnOnce(&mut Initialization) -> R) -> R {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    body(state.get_or_insert_with(Initialization::default))
}

unsafe fn lock(me: *mut Thread) {
    if OWNER.load(Ordering::Relaxed) == me as usize {
        with_state(|state| state.depth += 1);
        return;
    }
    let try_lock = || OWNER.compare_exchange(0, me as usize, Ordering::Acquire, Ordering::Relaxed).is_ok();
    if !try_lock() {
        // Waiting has to be a safepoint: whoever has the lock may need a
        // collection to get done.
        unsafe { thread::exit(me) };
        while !try_lock() {
            std::thread::yield_now();
        }
        unsafe { thread::enter(me) };
    }
    with_state(|state| {
        state.depth = 1;
        state.generations.push(Vec::new());
    });
}

fn unlock() {
    let released = with_state(|state| {
        state.depth -= 1;
        if state.depth != 0 {
            return false;
        }
        state.generations.pop();
        true
    });
    if released {
        OWNER.store(0, Ordering::Release);
    }
}

fn check_stack_room(me: *mut Thread) {
    let marker = 0u8;
    if (&raw const marker as usize) < unsafe { (*me).stack_limit } as usize {
        safety_panic(format_args!("stack overflow."));
    }
}

pub unsafe fn start(me: *mut Thread, origin: *const Origin, slot: *mut Ptr, object: *mut Object) -> bool {
    unsafe {
        crate::helpers::set_origin(me, origin);
        check_stack_room(me);
        lock(me);
        if !atomic16::load(slot).ptr.is_null() {
            fence(Ordering::SeqCst);
            unlock();
            return false;
        }
    }
    let new = with_state(|state| {
        let generation = state.generations.len();
        match state.map.get(&(slot as usize)) {
            Some(&(seen, existing)) => {
                if seen != generation {
                    safety_panic(format_args!("attempted to use global variable that is currently being initialized."));
                }
                if existing != object {
                    internal_panic(format_args!("two objects for one global"));
                }
                false
            }
            None => {
                state.map.insert(slot as usize, (generation, object));
                state.generations.last_mut().unwrap().push(slot as usize);
                true
            }
        }
    });
    if !new {
        unlock();
        return false;
    }
    if unsafe { (*object).flags() } & FLAG_GLOBAL != 0 {
        ROOTS.lock().unwrap_or_else(|e| e.into_inner()).push(object as usize);
    }
    true
}

fn finish_generation() {
    fence(Ordering::Release);
    with_state(|state| {
        let slots = std::mem::take(state.generations.last_mut().unwrap());
        for slot in slots.into_iter().rev() {
            let (_, object) = state.map.remove(&slot).unwrap();
            if !object.is_null() {
                unsafe { atomic16::store(slot as *mut Ptr, Ptr::for_object(object)) };
            }
        }
    });
}

pub fn end() {
    if with_state(|state| state.depth) == 1 {
        finish_generation();
    }
    unlock();
}

/// Resolves an ifunc: runs `resolver` and publishes what it returns in `slot`.
pub unsafe fn call_ifunc(me: *mut Thread, origin: *const Origin, slot: *mut Ptr, resolver: Ptr) -> Ptr {
    unsafe {
        crate::helpers::set_origin(me, origin);
        lock(me);
        let existing = atomic16::load(slot);
        if !existing.ptr.is_null() {
            unlock();
            return existing;
        }
    }
    with_state(|state| {
        if state.map.contains_key(&(slot as usize)) {
            safety_panic(format_args!("attempted to use ifunc-resolved global recursively."));
        }
        let generation = state.generations.len();
        // No object: the entry is only here to catch recursion.
        state.map.insert(slot as usize, (generation, std::ptr::null_mut()));
        state.generations.last_mut().unwrap().push(slot as usize);
        state.generations.push(Vec::new());
    });
    let mut returned = [Ptr::NULL];
    unsafe { crate::call(me, resolver, &[], &mut returned) };
    let result = returned[0];
    if !result.lower.is_null() {
        ROOTS.lock().unwrap_or_else(|e| e.into_inner()).push(result.object() as usize);
    }
    finish_generation();
    // Only now: everything the resolver referred to is initialized.
    fence(Ordering::Release);
    unsafe { atomic16::store(slot, result) };
    with_state(|state| {
        state.generations.pop();
    });
    end();
    result
}

#[repr(C)]
pub struct ConstexprNode {
    opcode: u32,
    left_kind: u32,
    left_target: *const (),
    right_value: usize,
}

#[repr(C)]
pub struct ConstantRelocation {
    offset: usize,
    kind: u32,
    target: *const (),
}

const GLOBAL_CONSTANT: u32 = 0;
const EXPR_CONSTANT: u32 = 1;

unsafe fn constant_value(me: *mut Thread, kind: u32, target: *const ()) -> Ptr {
    match kind {
        GLOBAL_CONSTANT => {
            // A weak symbol that nothing defines.
            if target.is_null() {
                return Ptr::NULL;
            }
            let getter = unsafe { std::mem::transmute::<*const (), GetterFn>(target) };
            unsafe { getter(me, std::ptr::null()) }
        }
        EXPR_CONSTANT => unsafe {
            let node = &*target.cast::<ConstexprNode>();
            let left = constant_value(me, node.left_kind, node.left_target);
            left.with_ptr(left.ptr.wrapping_add(node.right_value))
        },
        _ => internal_panic(format_args!("bad constant relocation kind {kind}")),
    }
}

pub unsafe fn execute_relocations(
    me: *mut Thread,
    constant: *mut Object,
    relocations: *const ConstantRelocation,
    count: usize,
) {
    unsafe {
        let payload = (*constant).lower();
        let aux = (*constant).aux_ptr();
        // Nothing here needs to be atomic: nobody can see the constant until
        // initialization ends.
        for index in (0..count).rev() {
            let relocation = &*relocations.add(index);
            let value = constant_value(me, relocation.kind, relocation.target);
            *payload.add(relocation.offset).cast::<*mut u8>() = value.ptr;
            aux_word(aux, relocation.offset).store(value.lower as usize, Ordering::Relaxed);
        }
    }
}

/// Calls `visit` on every object global state refers to.
pub unsafe fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    for &object in roots.iter() {
        visit(object as *mut Object);
    }
    drop(roots);
    let ctors = DEFERRED_CTORS.lock().unwrap_or_else(|e| e.into_inner());
    for &(_, lower) in ctors.iter() {
        visit(object_for_lower(lower as *mut u8));
    }
}

/// Constructors run as they turn up, in a process with no libc.
const CTORS_BARE: u8 = 0;
/// They wait for the libc to reach `__libc_start_main`.
const CTORS_DEFERRED: u8 = 1;
/// That happened, and any more run as they turn up.
const CTORS_DONE: u8 = 2;

static CTOR_MODE: AtomicU8 = AtomicU8::new(CTORS_BARE);
static DEFERRED_CTORS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Holds back constructors until the libc that's about to load is ready.
/// Constructors from before this have already run.
pub fn defer_ctors() {
    CTOR_MODE.store(CTORS_DEFERRED, Ordering::Release);
}

/// Whether Fil-C code may create threads. Until the libc has set itself up,
/// it can't.
pub fn ctors_started() -> bool {
    CTOR_MODE.load(Ordering::Acquire) != CTORS_DEFERRED
}

/// `__libc_start_main` got to the point of running constructors. They run in
/// the order they were deferred, which is what preserves their priorities.
pub unsafe fn run_deferred_ctors(me: *mut Thread) {
    if CTOR_MODE.swap(CTORS_DONE, Ordering::AcqRel) != CTORS_DEFERRED {
        crate::failure::safety_panic(format_args!("cannot run deferred global constructors twice."));
    }
    let mut index = 0;
    loop {
        let next = DEFERRED_CTORS.lock().unwrap_or_else(|e| e.into_inner()).get(index).copied();
        let Some((ptr, lower)) = next else { break };
        unsafe { run_ctor(me, Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 }) };
        index += 1;
    }
    DEFERRED_CTORS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Empty, null-terminated argv and environ, for constructors in a process
/// that has no Fil-C `main`. Both are the same read-only word.
fn empty_vector(me: *mut Thread) -> Ptr {
    static VECTOR: AtomicUsize = AtomicUsize::new(0);
    let existing = VECTOR.load(Ordering::Acquire);
    if existing != 0 {
        return Ptr::for_object(existing as *mut Object);
    }
    let object = unsafe { crate::object::allocate(me, WORD_SIZE, FLAG_GLOBAL) };
    match VECTOR.compare_exchange(0, object as usize, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            ROOTS.lock().unwrap_or_else(|e| e.into_inner()).push(object as usize);
            Ptr::for_object(object)
        }
        Err(existing) => Ptr::for_object(existing as *mut Object),
    }
}

unsafe fn run_ctor(me: *mut Thread, ctor: Ptr) {
    let args = crate::start::user_environment().map_or_else(
        || {
            let vector = empty_vector(me);
            [Ptr::NULL, vector, vector]
        },
        |user| [Ptr { ptr: user.argc as *mut u8, lower: std::ptr::null_mut() }, user.argv, user.environ],
    );
    unsafe { crate::call(me, ctor, &args, &mut []) };
}

pub unsafe fn defer_or_run_ctor(ctor: Ptr) {
    if CTOR_MODE.load(Ordering::Acquire) == CTORS_DEFERRED {
        let mut deferred = DEFERRED_CTORS.lock().unwrap_or_else(|e| e.into_inner());
        deferred.push((ctor.ptr as usize, ctor.lower as usize));
        return;
    }
    let me = thread::attach_current();
    unsafe {
        let was_entered = (*me).state.load(Ordering::Relaxed) & STATE_ENTERED != 0;
        if !was_entered {
            thread::enter(me);
        }
        run_ctor(me, ctor);
        if !was_entered {
            thread::exit(me);
        }
    }
}

/// Runs a destructor. It can run at any time, on any thread, as the host
/// unloads the library.
pub unsafe fn run_dtor(dtor: Ptr) {
    let me = thread::attach_current();
    unsafe {
        let was_entered = (*me).state.load(Ordering::Relaxed) & STATE_ENTERED != 0;
        if !was_entered {
            thread::enter(me);
        }
        crate::call(me, dtor, &[], &mut []);
        if !was_entered {
            thread::exit(me);
        }
    }
}
