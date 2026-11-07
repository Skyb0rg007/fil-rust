//! The table of `fr_*` functions the `fil-rust` crate is written against: the
//! same one its C shim gives it for libpizlo.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::{null_mut, read_volatile};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use fil_rust_sys::{CallbackFn, FilcThread, PtrInfo, RawPtr, SHIM_ABI_VERSION, Shim, ThreadMainFn, status};

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic, user_panic};
use crate::natives::function_object;
use crate::thread::{self, NativeFrame};
use crate::{cc, memops, object, ptrs};

fn to_ptr(raw: RawPtr) -> Ptr {
    Ptr { ptr: raw.ptr.cast(), lower: raw.lower.cast() }
}

fn to_raw(ptr: Ptr) -> RawPtr {
    RawPtr { ptr: ptr.ptr.cast(), lower: ptr.lower.cast() }
}

fn to_thread(thread: *mut FilcThread) -> *mut Thread {
    thread.cast()
}

/// Rust's entry point for callbacks. Set once, by `fr_init`.
static CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// References Rust holds on to. The collector treats the table as roots.
static ROOTS: Mutex<Vec<Ptr>> = Mutex::new(Vec::new());

/// As much as anyone could want, given that the table takes no memory until
/// it's used.
const ROOT_CAPACITY: usize = 1 << 32;

unsafe impl Send for Ptr {}

/// Calls `visit` on every object Rust holds a reference to.
pub unsafe fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    for root in roots.iter() {
        visit(root.object());
    }
}

unsafe extern "C" fn fr_abi_version() -> u32 {
    SHIM_ABI_VERSION
}

unsafe extern "C" fn fr_sizeof_thread() -> usize {
    size_of::<Thread>()
}

unsafe extern "C" fn fr_runtime_is_glibc_flavor() -> c_int {
    0
}

unsafe extern "C" fn fr_init(_: ThreadMainFn, callback: CallbackFn) {
    CALLBACK.store(callback as usize, Ordering::Release);
}

unsafe extern "C" fn fr_main_getter(_: *mut FilcThread, _: *const c_void) -> RawPtr {
    internal_panic(format_args!("the Rust runtime has no main thread"));
}

unsafe extern "C" fn fr_my_thread() -> *mut FilcThread {
    thread::current().cast()
}

unsafe extern "C" fn fr_check_thread_layout(thread: *mut FilcThread) -> c_int {
    (to_thread(thread) == thread::current()) as c_int
}

unsafe extern "C" fn fr_enter(t: *mut FilcThread) {
    unsafe { thread::enter(to_thread(t)) };
}

unsafe extern "C" fn fr_exit(t: *mut FilcThread) {
    unsafe { thread::exit(to_thread(t)) };
}

unsafe extern "C" fn fr_is_entered(t: *mut FilcThread) -> c_int {
    unsafe { crate::helpers::is_entered(to_thread(t)) as c_int }
}

unsafe extern "C" fn fr_pollcheck(t: *mut FilcThread) {
    unsafe { thread::poll(to_thread(t)) };
}

unsafe extern "C" fn fr_with_native_frame(t: *mut FilcThread, body: unsafe extern "C" fn(*mut c_void), arg: *mut c_void) {
    let thread = to_thread(t);
    unsafe {
        let frame = NativeFrame::push(thread);
        body(arg);
        frame.pop(thread);
    }
}

unsafe extern "C" fn fr_worker_function() -> RawPtr {
    internal_panic(format_args!("the Rust runtime has no executors"));
}

unsafe extern "C" fn fr_panic(message: *const c_char) {
    user_panic(format_args!("{}", unsafe { CStr::from_ptr(message) }.to_string_lossy()));
}

unsafe extern "C" fn fr_args_check_failure(actual: usize, expected: usize) {
    safety_panic(format_args!("argument size mismatch (actual = {actual}, expected = {expected})."));
}

unsafe extern "C" fn fr_root_capacity() -> usize {
    ROOT_CAPACITY
}

unsafe extern "C" fn fr_root_set(_: *mut FilcThread, index: usize, value: RawPtr) -> c_int {
    if index >= ROOT_CAPACITY {
        return status::OUT_OF_BOUNDS;
    }
    let mut roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    if roots.len() <= index {
        roots.resize(index + 1, Ptr::NULL);
    }
    roots[index] = to_ptr(value);
    status::OK
}

unsafe extern "C" fn fr_root_get(t: *mut FilcThread, index: usize, out: *mut RawPtr) -> c_int {
    unsafe { *out = RawPtr::NULL };
    if index >= ROOT_CAPACITY {
        return status::OUT_OF_BOUNDS;
    }
    let roots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    let value = roots.get(index).copied().unwrap_or(Ptr::NULL);
    drop(roots);
    unsafe {
        thread::track(to_thread(t), value.object());
        *out = to_raw(value);
    }
    status::OK
}

/// Why an access can't happen, as the `fil-rust` crate wants to hear it.
fn access_status(ptr: Ptr, size: usize, write: bool) -> c_int {
    if ptr.lower.is_null() {
        return status::NULL_CAPABILITY;
    }
    let object = unsafe { &*ptr.object() };
    let flags = object.flags();
    if flags & FLAG_FREE != 0 {
        return status::FREED;
    }
    if object.special_type() != 0 {
        return status::SPECIAL;
    }
    let address = ptr.ptr as usize;
    let lower = ptr.lower as usize;
    let upper = object.upper() as usize;
    if address < lower || address > upper || size > upper - address {
        // `free` shrinks the object before setting the flag, so an access
        // racing with it can see the new bounds first.
        if object.flags() & FLAG_FREE != 0 {
            return status::FREED;
        }
        return status::OUT_OF_BOUNDS;
    }
    if write && flags & FLAG_READONLY != 0 {
        return status::READONLY;
    }
    status::OK
}

unsafe extern "C" fn fr_ptr_info_get(raw: RawPtr, out: *mut PtrInfo) -> c_int {
    let ptr = to_ptr(raw);
    unsafe {
        *out = PtrInfo::default();
        if ptr.lower.is_null() {
            return status::NULL_CAPABILITY;
        }
        let object = &*ptr.object();
        (*out).lower = ptr.lower.cast();
        (*out).upper = object.upper().cast();
        (*out).flags = object.flags() as u32;
        (*out).special_type = object.special_type() as u32;
    }
    status::OK
}

unsafe extern "C" fn fr_check(raw: RawPtr, size: usize, write: c_int) -> c_int {
    access_status(to_ptr(raw), size, write != 0)
}

unsafe extern "C" fn fr_allocate(t: *mut FilcThread, size: usize, alignment: usize) -> RawPtr {
    let thread = to_thread(t);
    unsafe {
        thread::poll(thread);
        let object = object::allocate_aligned(thread, size, alignment, 0);
        thread::track(thread, object);
        to_raw(Ptr::for_object(object))
    }
}

unsafe extern "C" fn fr_copy_in(_: *mut FilcThread, dst: RawPtr, src: *const c_void, size: usize) -> c_int {
    let result = access_status(to_ptr(dst), size, true);
    if result == status::OK && size != 0 {
        unsafe { std::ptr::copy_nonoverlapping(src.cast::<u8>(), dst.ptr.cast(), size) };
    }
    result
}

unsafe extern "C" fn fr_copy_out(_: *mut FilcThread, src: RawPtr, dst: *mut c_void, size: usize) -> c_int {
    let result = access_status(to_ptr(src), size, false);
    if result == status::OK && size != 0 {
        unsafe { std::ptr::copy_nonoverlapping(src.ptr.cast::<u8>(), dst.cast(), size) };
    }
    result
}

unsafe extern "C" fn fr_memmove(t: *mut FilcThread, dst: RawPtr, src: RawPtr, size: usize) -> c_int {
    let (dst, src) = (to_ptr(dst), to_ptr(src));
    let mut result = access_status(dst, size, true);
    if result == status::OK {
        result = access_status(src, size, false);
    }
    if result == status::OK {
        unsafe { memops::move_checked(to_thread(t), dst, src, size) };
    }
    result
}

unsafe extern "C" fn fr_strlen(raw: RawPtr, out: *mut usize) -> c_int {
    let ptr = to_ptr(raw);
    let result = access_status(ptr, 0, false);
    if result != status::OK {
        return result;
    }
    let available = unsafe { (*ptr.object()).upper() } as usize - ptr.ptr as usize;
    let bytes = unsafe { std::slice::from_raw_parts(ptr.ptr, available) };
    match bytes.iter().position(|&byte| byte == 0) {
        Some(length) => {
            unsafe { *out = length };
            status::OK
        }
        None => status::OUT_OF_BOUNDS,
    }
}

fn word_access_status(ptr: Ptr, write: bool) -> c_int {
    let result = access_status(ptr, WORD_SIZE, write);
    if result == status::OK && !(ptr.ptr as usize).is_multiple_of(WORD_SIZE) {
        return status::MISALIGNED;
    }
    result
}

unsafe extern "C" fn fr_load_ptr(t: *mut FilcThread, base: RawPtr, out: *mut RawPtr) -> c_int {
    unsafe { *out = RawPtr::NULL };
    let base = to_ptr(base);
    let result = word_access_status(base, false);
    if result != status::OK {
        return result;
    }
    unsafe {
        let value = ptrs::load(base);
        thread::track(to_thread(t), value.object());
        *out = to_raw(value);
    }
    status::OK
}

unsafe extern "C" fn fr_store_ptr(t: *mut FilcThread, base: RawPtr, value: RawPtr) -> c_int {
    let base = to_ptr(base);
    let result = word_access_status(base, true);
    if result == status::OK {
        unsafe { ptrs::store(to_thread(t), base, to_ptr(value)) };
    }
    result
}

unsafe extern "C" fn fr_track(t: *mut FilcThread, raw: RawPtr) {
    unsafe { thread::track(to_thread(t), to_ptr(raw).object()) };
}

unsafe extern "C" fn fr_call_getter(t: *mut FilcThread, getter: *mut c_void) -> RawPtr {
    let thread = to_thread(t);
    unsafe {
        let getter = std::mem::transmute::<*mut c_void, GetterFn>(getter);
        let result = getter(thread, std::ptr::null());
        thread::track(thread, result.object());
        to_raw(result)
    }
}

unsafe extern "C" fn fr_check_function(raw: RawPtr) -> c_int {
    let ptr = to_ptr(raw);
    if ptr.lower.is_null() {
        return status::NULL_CAPABILITY;
    }
    let object = unsafe { &*ptr.object() };
    if object.special_type() != SPECIAL_FUNCTION || ptr.ptr != object.aux_ptr() {
        return status::NOT_A_FUNCTION;
    }
    status::OK
}

unsafe extern "C" fn fr_cc_begin(t: *mut FilcThread, size: usize) {
    let thread = to_thread(t);
    unsafe {
        cc::ensure_total(thread, size);
        let mut offset = 0;
        while offset < size {
            *cc::slot(thread, offset) = 0;
            *cc::aux_slot(thread, offset) = 0;
            offset += WORD_SIZE;
        }
    }
}

unsafe fn slot_ok(thread: *mut Thread, size: usize, offset: usize) -> bool {
    offset.is_multiple_of(WORD_SIZE)
        && offset.checked_add(WORD_SIZE).is_some_and(|end| end <= size)
        && size <= CC_INLINE_SIZE + unsafe { (*thread).cc_outline_size }
}

unsafe extern "C" fn fr_cc_set_word(t: *mut FilcThread, size: usize, offset: usize, value: u64) -> c_int {
    let thread = to_thread(t);
    unsafe {
        if !slot_ok(thread, size, offset) {
            return status::BAD_SLOT;
        }
        *cc::slot(thread, offset) = value;
        *cc::aux_slot(thread, offset) = 0;
    }
    status::OK
}

unsafe extern "C" fn fr_cc_set_ptr(t: *mut FilcThread, size: usize, offset: usize, value: RawPtr) -> c_int {
    let thread = to_thread(t);
    unsafe {
        if !slot_ok(thread, size, offset) {
            return status::BAD_SLOT;
        }
        *cc::aux_slot(thread, offset) = value.lower as u64;
        *cc::slot(thread, offset) = value.ptr as u64;
    }
    status::OK
}

unsafe extern "C" fn fr_cc_get_word(t: *mut FilcThread, size: usize, offset: usize, out: *mut u64) -> c_int {
    let thread = to_thread(t);
    unsafe {
        *out = 0;
        if !slot_ok(thread, size, offset) {
            return status::BAD_SLOT;
        }
        *out = *cc::slot(thread, offset);
    }
    status::OK
}

unsafe extern "C" fn fr_cc_get_ptr(t: *mut FilcThread, size: usize, offset: usize, out: *mut RawPtr) -> c_int {
    let thread = to_thread(t);
    unsafe {
        *out = RawPtr::NULL;
        if !slot_ok(thread, size, offset) {
            return status::BAD_SLOT;
        }
        let value = Ptr { ptr: *cc::slot(thread, offset) as *mut u8, lower: *cc::aux_slot(thread, offset) as *mut u8 };
        thread::track(thread, value.object());
        *out = to_raw(value);
    }
    status::OK
}

unsafe extern "C" fn fr_call(t: *mut FilcThread, function: RawPtr, argument_size: usize, return_size: *mut usize) -> c_int {
    let thread = to_thread(t);
    unsafe {
        *return_size = 0;
        let result = fr_check_function(function);
        if result != status::OK {
            return result;
        }
        let function = to_ptr(function);
        let entry = (*function.ptr.cast::<Function>()).generic_entrypoint;
        let returned = entry(thread, function.lower, argument_size);
        if returned.has_exception {
            crate::failure::internal_panic(format_args!("Fil-C exceptions aren't supported yet"));
        }
        *return_size = returned.return_size;
    }
    status::OK
}

/// The closure's whole data pointer: Rust's id for the callback is its address.
unsafe extern "C" fn callback_entry(thread: *mut Thread, callee_lower: *mut u8, argument_size: usize) -> ReturnValue {
    let object = object_for_lower(callee_lower);
    if object.is_null() || unsafe { (*object).special_type() } != SPECIAL_FUNCTION || unsafe { (*object).flags() } & FLAG_GLOBAL != 0 {
        safety_panic(format_args!("fil-rust: callback invoked without closure data."));
    }
    unsafe {
        let frame = NativeFrame::push(thread);
        // A callback can reach a safepoint before it decodes its arguments,
        // and the buffers aren't roots: track what might be a pointer in them.
        let mut offset = 0;
        while offset < argument_size {
            thread::track(thread, object_for_lower(*cc::aux_slot(thread, offset) as *mut u8));
            offset += WORD_SIZE;
        }
        let id = read_volatile(&raw const (*callee_lower.cast::<Closure>()).data.ptr) as u64;
        let callback = std::mem::transmute::<usize, CallbackFn>(CALLBACK.load(Ordering::Acquire));
        let return_size = callback(thread.cast(), id, argument_size);
        frame.pop(thread);
        ReturnValue { has_exception: false, return_size }
    }
}

unsafe extern "C" fn fr_closure_new(t: *mut FilcThread, id: u64) -> RawPtr {
    static BASE: AtomicPtr<FunctionObject> = AtomicPtr::new(null_mut());
    let thread = to_thread(t);
    let base = function_object(&BASE, callback_entry);
    unsafe {
        let result = crate::natives::closure_new(thread, base, Ptr { ptr: id as *mut u8, lower: null_mut() });
        thread::track(thread, result.object());
        to_raw(result)
    }
}

/// The table for the `fil-rust` crate.
pub fn table() -> Shim {
    Shim {
        fr_abi_version,
        fr_sizeof_thread,
        fr_runtime_is_glibc_flavor,
        fr_init,
        fr_main_getter,
        fr_my_thread,
        fr_check_thread_layout,
        fr_enter,
        fr_exit,
        fr_is_entered,
        fr_pollcheck,
        fr_with_native_frame,
        fr_worker_function,
        fr_panic,
        fr_args_check_failure,
        fr_root_capacity,
        fr_root_set,
        fr_root_get,
        fr_ptr_info_get,
        fr_check,
        fr_allocate,
        fr_copy_in,
        fr_copy_out,
        fr_memmove,
        fr_strlen,
        fr_load_ptr,
        fr_store_ptr,
        fr_track,
        fr_call_getter,
        fr_check_function,
        fr_cc_begin,
        fr_cc_set_word,
        fr_cc_set_ptr,
        fr_cc_get_word,
        fr_cc_get_ptr,
        fr_call,
        fr_closure_new,
    }
}
