//! Pointer tables, weak references, weak maps and finalizer queues: the parts
//! of the object model that the collector treats specially.
//!
//! Each is a special object whose payload holds a `Box` with the state, freed
//! by [`destruct`] once the object is collected, except a weak reference, which
//! is just the pointer. Their locks are only taken by entered threads, which
//! can't reach a safepoint with one held, and by the collector with the world
//! stopped, so a collection never waits for one. A thread waiting on a
//! finalizer queue is the exception, and it waits exited, with signals blocked.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::abi::*;
use crate::failure::internal_panic;
use crate::gc;
use crate::natives::natives;
use crate::object;
use crate::ptrs;
use crate::thread;

/// What `zptrtable_encode` adds to an index, before shifting it, so that small
/// integers and real addresses don't look like encodings.
const OFFSET: usize = 66666;
const SHIFT: u32 = 4;

/// Tables made and not yet collected, for `ztesting_get_num_ptrtables`.
static PTR_TABLES: AtomicUsize = AtomicUsize::new(0);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(ptr: Ptr) -> (usize, usize) {
    (ptr.ptr as usize, ptr.lower as usize)
}

fn is_free(object: *mut Object) -> bool {
    !object.is_null() && unsafe { &*object }.flags() & FLAG_FREE != 0
}

/// Pointers given small integer names, for code that needs to hand a pointer
/// through an integer. Holds what it names alive until it's freed.
#[derive(Default)]
pub struct PtrTable {
    encoded: HashMap<(usize, usize), usize>,
    entries: Vec<Ptr>,
    free: Vec<usize>,
}

/// Pointers named by their address, which decode back to the capability.
pub struct ExactTable {
    weak: bool,
    map: HashMap<usize, Ptr>,
}

/// Values that live as long as their keys do.
#[derive(Default)]
pub struct WeakMap {
    map: HashMap<(usize, usize), (Ptr, Ptr)>,
}

/// Objects that would have been collected, for the program to finalize.
#[derive(Default)]
pub struct FinalizerQueue {
    queue: Mutex<VecDeque<usize>>,
    ready: Condvar,
}

/// The `Box` in a special object's payload.
unsafe fn payload<'a, T>(object: *mut Object) -> &'a T {
    unsafe { &**lower_for_object(object).cast::<*mut T>() }
}

/// Makes a special object holding `value`, which [`destruct`] frees.
unsafe fn create<T>(thread: *mut Thread, special: usize, value: T) -> Ptr {
    unsafe {
        let object = object::allocate_special(thread, size_of::<*mut T>(), special);
        thread::track(thread, object);
        lower_for_object(object).cast::<*mut T>().write(Box::into_raw(Box::new(value)));
        gc::add_destructible(object);
        Ptr::for_object(object)
    }
}

fn special<'a, T>(ptr: Ptr, special: usize, name: &str) -> &'a T {
    object::check_special(ptr, special, name);
    unsafe { payload(ptr.object()) }
}

/// Frees the state of a table, map or queue that was collected.
pub unsafe fn destruct(object: *mut Object) {
    unsafe {
        let boxed = *lower_for_object(object).cast::<*mut c_void>();
        match (*object).special_type() {
            SPECIAL_PTR_TABLE => {
                drop(Box::from_raw(boxed.cast::<Mutex<PtrTable>>()));
                PTR_TABLES.fetch_sub(1, Ordering::Relaxed);
            }
            SPECIAL_EXACT_PTR_TABLE => drop(Box::from_raw(boxed.cast::<Mutex<ExactTable>>())),
            SPECIAL_WEAK_MAP => drop(Box::from_raw(boxed.cast::<Mutex<WeakMap>>())),
            SPECIAL_FINALIZER_QUEUE => drop(Box::from_raw(boxed.cast::<FinalizerQueue>())),
            special => internal_panic(format_args!("nothing to destruct in special objects of type {special}")),
        }
    }
}

/// Whether a weak reference to `object` stays: it's marked, or it's never
/// collected. A freed object is gone already.
pub fn is_live_for_weak(object: *mut Object) -> bool {
    if object.is_null() {
        return true;
    }
    let flags = unsafe { &*object }.flags();
    if flags & FLAG_FREE != 0 {
        return false;
    }
    flags & FLAG_GLOBAL != 0 || gc::is_marked(object)
}

/// What the collector does with each kind, with the world stopped.
pub mod collect {
    use super::*;

    /// Marks what a pointer table names, and forgets what was freed.
    pub unsafe fn scan_ptr_table(object: *mut Object, mut mark: impl FnMut(*mut Object)) {
        let mut table = lock(unsafe { payload::<Mutex<PtrTable>>(object) });
        let table = &mut *table;
        table.encoded.retain(|_, index| !is_free(table.entries[*index].object()));
        for index in 0..table.entries.len() {
            let entry = table.entries[index];
            if entry.ptr.is_null() {
                continue;
            }
            if is_free(entry.object()) {
                table.entries[index] = Ptr::NULL;
                table.free.push(index);
            } else {
                mark(entry.object());
            }
        }
    }

    /// Marks what a strong exact table holds, and forgets what was freed.
    /// Returns false for a weak one, which the census sees to instead.
    pub unsafe fn scan_exact_table(object: *mut Object, mut mark: impl FnMut(*mut Object)) -> bool {
        let mut table = lock(unsafe { payload::<Mutex<ExactTable>>(object) });
        if table.weak {
            return false;
        }
        table.map.retain(|_, value| !is_free(value.object()));
        for value in table.map.values() {
            mark(value.object());
        }
        true
    }

    /// Marks what's queued for finalizing.
    pub unsafe fn scan_finalizer_queue(object: *mut Object, mut mark: impl FnMut(*mut Object)) {
        for &queued in lock(unsafe { &payload::<FinalizerQueue>(object).queue }).iter() {
            mark(queued as *mut Object);
        }
    }

    /// Marks the values of keys that are live. Returns whether it marked any.
    pub unsafe fn mark_weak_map_values(object: *mut Object, mut mark: impl FnMut(*mut Object) -> bool) -> bool {
        let map = lock(unsafe { payload::<Mutex<WeakMap>>(object) });
        let mut marked = false;
        for (key, value) in map.map.values() {
            if is_live_for_weak(key.object()) && !value.lower.is_null() {
                marked |= mark(value.object());
            }
        }
        marked
    }

    pub unsafe fn census_weak(object: *mut Object) {
        unsafe {
            let weak = lower_for_object(object).cast::<Ptr>();
            if !is_live_for_weak((*weak).object()) {
                weak.write(Ptr::NULL);
            }
        }
    }

    pub unsafe fn census_weak_map(object: *mut Object) {
        lock(unsafe { payload::<Mutex<WeakMap>>(object) }).map.retain(|_, (key, _)| is_live_for_weak(key.object()));
    }

    pub unsafe fn census_exact_table(object: *mut Object) {
        lock(unsafe { payload::<Mutex<ExactTable>>(object) }).map.retain(|_, value| is_live_for_weak(value.object()));
    }

    /// Queues an object for finalizing, and wakes whoever is waiting.
    pub unsafe fn enqueue(queue: *mut Object, object: *mut Object) {
        let queue = unsafe { payload::<FinalizerQueue>(queue) };
        lock(&queue.queue).push_back(object as usize);
        queue.ready.notify_all();
    }
}

natives! {
    fn zptrtable_new(thread: *mut Thread) -> Ptr {
        PTR_TABLES.fetch_add(1, Ordering::Relaxed);
        unsafe { create(thread, SPECIAL_PTR_TABLE, Mutex::new(PtrTable::default())) }
    }

    fn zptrtable_encode(thread: *mut Thread, table: Ptr, ptr: Ptr) -> usize {
        let table = special::<Mutex<PtrTable>>(table, SPECIAL_PTR_TABLE, "ptr table");
        if ptr.ptr.is_null() || ptr.lower.is_null() || is_free(ptr.object()) {
            return 0;
        }
        let mut table = lock(table);
        let table = &mut *table;
        if let Some(&index) = table.encoded.get(&key(ptr)) {
            return (index + OFFSET) << SHIFT;
        }
        let index = match table.free.pop() {
            Some(index) => index,
            None => {
                table.entries.push(Ptr::NULL);
                table.entries.len() - 1
            }
        };
        table.entries[index] = ptr;
        table.encoded.insert(key(ptr), index);
        (index + OFFSET) << SHIFT
    }

    fn zptrtable_decode(thread: *mut Thread, table: Ptr, encoded: usize) -> Ptr {
        let table = lock(special::<Mutex<PtrTable>>(table, SPECIAL_PTR_TABLE, "ptr table"));
        let index = (encoded >> SHIFT).wrapping_sub(OFFSET);
        let Some(&entry) = table.entries.get(index) else { return Ptr::NULL };
        if entry.ptr.is_null() || is_free(entry.object()) {
            return Ptr::NULL;
        }
        unsafe { thread::track(thread, entry.object()) };
        entry
    }

    fn ztesting_get_num_ptrtables(thread: *mut Thread) -> usize {
        PTR_TABLES.load(Ordering::Relaxed)
    }

    fn zexact_ptrtable_new(thread: *mut Thread) -> Ptr {
        unsafe { create(thread, SPECIAL_EXACT_PTR_TABLE, Mutex::new(ExactTable { weak: false, map: HashMap::new() })) }
    }

    fn zexact_ptrtable_new_weak(thread: *mut Thread) -> Ptr {
        unsafe { create(thread, SPECIAL_EXACT_PTR_TABLE, Mutex::new(ExactTable { weak: true, map: HashMap::new() })) }
    }

    fn zexact_ptrtable_is_weak(thread: *mut Thread, table: Ptr) -> bool {
        lock(special::<Mutex<ExactTable>>(table, SPECIAL_EXACT_PTR_TABLE, "exact ptr table")).weak
    }

    fn zexact_ptrtable_encode(thread: *mut Thread, table: Ptr, ptr: Ptr) -> usize {
        let table = special::<Mutex<ExactTable>>(table, SPECIAL_EXACT_PTR_TABLE, "exact ptr table");
        if ptr.lower.is_null() || is_free(ptr.object()) {
            return ptr.ptr as usize;
        }
        lock(table).map.insert(ptr.ptr as usize, ptr);
        ptr.ptr as usize
    }

    fn zexact_ptrtable_decode(thread: *mut Thread, table: Ptr, encoded: usize) -> Ptr {
        let table = lock(special::<Mutex<ExactTable>>(table, SPECIAL_EXACT_PTR_TABLE, "exact ptr table"));
        let invalid = Ptr { ptr: encoded as *mut u8, lower: null_mut() };
        match table.map.get(&encoded) {
            Some(&value) if !is_free(value.object()) => {
                unsafe { thread::track(thread, value.object()) };
                value
            }
            _ => invalid,
        }
    }

    fn zweak_new(thread: *mut Thread, ptr: Ptr) -> Ptr {
        unsafe {
            let object = object::allocate_special(thread, size_of::<Ptr>(), SPECIAL_WEAK);
            lower_for_object(object).cast::<Ptr>().write(ptr);
            Ptr::for_object(object)
        }
    }

    fn zweak_get(thread: *mut Thread, weak: Ptr) -> Ptr {
        object::check_special(weak, SPECIAL_WEAK, "weak");
        let ptr = unsafe { weak.ptr.cast::<Ptr>().read() };
        if is_free(ptr.object()) {
            return Ptr::NULL;
        }
        unsafe { thread::track(thread, ptr.object()) };
        ptr
    }

    fn zweak_map_new(thread: *mut Thread) -> Ptr {
        unsafe { create(thread, SPECIAL_WEAK_MAP, Mutex::new(WeakMap::default())) }
    }

    fn zweak_map_set(thread: *mut Thread, map: Ptr, key_ptr: Ptr, value: Ptr) -> () {
        let mut map = lock(special::<Mutex<WeakMap>>(map, SPECIAL_WEAK_MAP, "weak map"));
        if value.ptr.is_null() && value.lower.is_null() {
            map.map.remove(&key(key_ptr));
        } else {
            map.map.insert(key(key_ptr), (key_ptr, value));
        }
    }

    fn zweak_map_get(thread: *mut Thread, map: Ptr, key_ptr: Ptr) -> Ptr {
        let map = lock(special::<Mutex<WeakMap>>(map, SPECIAL_WEAK_MAP, "weak map"));
        if is_free(key_ptr.object()) {
            return Ptr::NULL;
        }
        match map.map.get(&key(key_ptr)) {
            Some(&(_, value)) => {
                unsafe { thread::track(thread, value.object()) };
                value
            }
            None => Ptr::NULL,
        }
    }

    fn zweak_map_size(thread: *mut Thread, map: Ptr) -> usize {
        lock(special::<Mutex<WeakMap>>(map, SPECIAL_WEAK_MAP, "weak map")).map.len()
    }

    fn zweak_map_snapshot_impl(thread: *mut Thread, map: Ptr) -> Ptr {
        let map = lock(special::<Mutex<WeakMap>>(map, SPECIAL_WEAK_MAP, "weak map"));
        // Pairs of key and value, ending with a pair of nulls. Allocating
        // doesn't reach a safepoint, so the map can stay locked.
        let slots = map.map.len().checked_add(1).and_then(|pairs| pairs.checked_mul(2));
        let size = slots.and_then(|slots| slots.checked_mul(size_of::<usize>())).unwrap_or(usize::MAX);
        unsafe {
            let object = object::allocate(thread, size, 0);
            thread::track(thread, object);
            let result = Ptr::for_object(object);
            let mut at = result.ptr;
            for &(key_ptr, value) in map.map.values() {
                if is_free(key_ptr.object()) {
                    continue;
                }
                ptrs::store(thread, result.with_ptr(at), key_ptr);
                ptrs::store(thread, result.with_ptr(at.add(size_of::<usize>())), value);
                at = at.add(2 * size_of::<usize>());
            }
            result
        }
    }

    fn zgc_finq_new(thread: *mut Thread) -> Ptr {
        unsafe { create(thread, SPECIAL_FINALIZER_QUEUE, FinalizerQueue::default()) }
    }

    fn zgc_finq_alloc(thread: *mut Thread, queue: Ptr, size: usize) -> Ptr {
        special::<FinalizerQueue>(queue, SPECIAL_FINALIZER_QUEUE, "finalizer queue");
        unsafe { finalizable(thread, queue, object::allocate(thread, size, 0)) }
    }

    fn zgc_finq_aligned_alloc(thread: *mut Thread, queue: Ptr, alignment: usize, size: usize) -> Ptr {
        special::<FinalizerQueue>(queue, SPECIAL_FINALIZER_QUEUE, "finalizer queue");
        unsafe { finalizable(thread, queue, object::allocate_aligned(thread, size, alignment, 0)) }
    }

    fn zgc_finq_poll(thread: *mut Thread, queue: Ptr) -> Ptr {
        let queue = special::<FinalizerQueue>(queue, SPECIAL_FINALIZER_QUEUE, "finalizer queue");
        let next = lock(&queue.queue).pop_front();
        unsafe { dequeued(thread, next) }
    }

    fn zgc_finq_wait(thread: *mut Thread, queue: Ptr) -> Ptr {
        let queue = special::<FinalizerQueue>(queue, SPECIAL_FINALIZER_QUEUE, "finalizer queue");
        loop {
            if let Some(next) = lock(&queue.queue).pop_front() {
                return unsafe { dequeued(thread, Some(next)) };
            }
            // Only a collection fills it, so waiting has to let one happen.
            unsafe { thread::exit(thread) };
            {
                let _blocked = crate::signal::Blocked::all();
                let mut queued = lock(&queue.queue);
                while queued.is_empty() {
                    queued = queue.ready.wait(queued).unwrap_or_else(|e| e.into_inner());
                }
            }
            unsafe { thread::enter(thread) };
        }
    }
}

unsafe fn finalizable(thread: *mut Thread, queue: Ptr, object: *mut Object) -> Ptr {
    unsafe {
        thread::track(thread, object);
        gc::add_finalizable(object, queue.object());
        Ptr::for_object(object)
    }
}

unsafe fn dequeued(thread: *mut Thread, next: Option<usize>) -> Ptr {
    match next {
        Some(object) => {
            let object = object as *mut Object;
            unsafe { thread::track(thread, object) };
            Ptr::for_object(object)
        }
        None => Ptr::NULL,
    }
}
