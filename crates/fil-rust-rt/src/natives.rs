//! Functions Fil-C code calls by name: `zgc_alloc` and the rest of `stdfil.h`.
//!
//! A native is reached through its `pizlonated_` getter like any compiled
//! function, so it needs a function object and an entry point that speaks the
//! calling convention. The `natives!` macro makes those from a Rust function.

use std::ffi::c_int;
use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::abi::*;
use crate::failure::{safety_panic, user_panic};
use crate::object::{self, Access};
use crate::thread::{self, NativeFrame};
use crate::{cc, memops};

/// A cursor over the argument buffer.
pub struct Args {
    offset: usize,
    size: usize,
}

impl Args {
    fn next(&mut self) -> usize {
        let offset = self.offset;
        if offset >= self.size {
            safety_panic(format_args!(
                "cc cursor {offset},{} doesn't have room for {WORD_SIZE} byte argument.",
                self.size
            ));
        }
        self.offset += WORD_SIZE;
        offset
    }
}

/// How a Rust type travels through the buffers.
pub trait Value: Sized {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> Self;
    unsafe fn give(self, thread: *mut Thread) -> usize;
}

macro_rules! integer_value {
    ($($ty:ty),*) => {$(
        impl Value for $ty {
            unsafe fn take(thread: *mut Thread, args: &mut Args) -> Self {
                unsafe { *cc::slot(thread, args.next()) as $ty }
            }

            unsafe fn give(self, thread: *mut Thread) -> usize {
                unsafe {
                    *cc::slot(thread, 0) = self as u64;
                    *cc::aux_slot(thread, 0) = 0;
                }
                WORD_SIZE
            }
        }
    )*};
}

integer_value!(usize, u64, i64, c_int, u32);

impl Value for bool {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> bool {
        unsafe { *cc::slot(thread, args.next()) as u8 != 0 }
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        unsafe { (self as u64).give(thread) }
    }
}

impl Value for Ptr {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> Ptr {
        let offset = args.next();
        let value = unsafe {
            Ptr { ptr: *cc::slot(thread, offset) as *mut u8, lower: *cc::aux_slot(thread, offset) as *mut u8 }
        };
        // The callee has to root its arguments: the buffers aren't scanned.
        unsafe { thread::track(thread, value.object()) };
        value
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        unsafe {
            *cc::slot(thread, 0) = self.ptr as u64;
            *cc::aux_slot(thread, 0) = self.lower as u64;
        }
        WORD_SIZE
    }
}

impl Value for () {
    unsafe fn take(_: *mut Thread, _: &mut Args) {}

    unsafe fn give(self, _: *mut Thread) -> usize {
        0
    }
}

/// Makes the function object once. Natives never go away, so it can leak.
pub fn function_object(cell: &AtomicPtr<FunctionObject>, entry: PizlonatedFn) -> Ptr {
    let mut existing = cell.load(Ordering::Acquire);
    if existing.is_null() {
        let object = Box::into_raw(Box::new(FunctionObject {
            object: Object { upper: AtomicPtr::new(null_mut()), aux: AtomicUsize::new(0) },
            function: Function {
                fast_entrypoint: entry as *const (),
                generic_entrypoint: entry,
                signature: GENERIC_SIGNATURE,
            },
        }));
        unsafe {
            let function = (&raw mut (*object).function).cast::<u8>();
            (*object).object.upper.store(function, Ordering::Relaxed);
            let flags = FLAG_GLOBAL | FLAG_READONLY | object_flags(SPECIAL_FUNCTION, 0);
            (*object).object.aux.store(aux_create(flags, function), Ordering::Relaxed);
        }
        existing = match cell.compare_exchange(null_mut(), object, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => object,
            Err(won) => {
                drop(unsafe { Box::from_raw(object) });
                won
            }
        };
    }
    let function = unsafe { (&raw mut (*existing).function).cast::<u8>() };
    Ptr { ptr: function, lower: function }
}

macro_rules! natives {
    ($(fn $name:ident($thread:ident: *mut Thread $(, $arg:ident: $ty:ty)* $(,)?) -> $ret:ty $body:block)*) => {
        $(
            #[allow(non_snake_case)]
            mod $name {
                use super::*;

                pub unsafe extern "C" fn entry(thread: *mut Thread, _: *mut u8, size: usize) -> ReturnValue {
                    let frame = unsafe { NativeFrame::push(thread) };
                    #[allow(unused_mut, unused_variables)]
                    let mut args = Args { offset: 0, size };
                    $(let $arg = unsafe { <$ty as Value>::take(thread, &mut args) };)*
                    let result: $ret = unsafe { body(thread $(, $arg)*) };
                    let return_size = unsafe { result.give(thread) };
                    unsafe { frame.pop(thread) };
                    ReturnValue { has_exception: false, return_size }
                }

                #[allow(unused_variables, clippy::unused_unit)]
                unsafe fn body($thread: *mut Thread $(, $arg: $ty)*) -> $ret $body

                pub unsafe extern "C" fn getter(_: *mut Thread, _: *const Origin) -> Ptr {
                    static CELL: AtomicPtr<FunctionObject> = AtomicPtr::new(null_mut());
                    function_object(&CELL, entry)
                }
            }
        )*

        /// Stub slots for every native so far, keyed by symbol.
        pub fn table() -> Vec<(&'static str, *const ())> {
            vec![$((concat!("pizlonated_", stringify!($name)), $name::getter as *const ())),*]
        }
    };
}

/// Copies out a null-terminated string that the access checks allow.
unsafe fn string_of(ptr_: Ptr) -> Vec<u8> {
    object::check_access(ptr_, 1, Access::Read);
    let available = unsafe { (*ptr_.object()).upper() } as usize - ptr_.ptr as usize;
    let bytes = unsafe { std::slice::from_raw_parts(ptr_.ptr, available) };
    match bytes.iter().position(|&byte| byte == 0) {
        Some(length) => bytes[..length].to_vec(),
        None => safety_panic(format_args!("string is not null-terminated (ptr = {:p}).", ptr_.ptr)),
    }
}

unsafe fn realloc(thread: *mut Thread, old: Ptr, size: usize, alignment: usize) -> Ptr {
    if old.ptr.is_null() {
        return Ptr::for_object(unsafe { object::allocate_aligned(thread, size, alignment, 0) });
    }
    unsafe { object::reallocate(thread, old, size, alignment) }
}

/// A function object like `function`, with `data` to go with it.
pub unsafe fn closure_new(thread: *mut Thread, function: Ptr, data: Ptr) -> Ptr {
    object::check_function_call(function);
    unsafe {
        let block = crate::heap::heap().allocate(thread, OBJECT_SIZE + size_of::<Closure>());
        let object = block.cast::<Object>();
        let lower = lower_for_object(object);
        let closure = lower.cast::<Closure>();
        let base = &*function.ptr.cast::<Function>();
        (&raw mut (*closure).function).write(Function {
            fast_entrypoint: base.fast_entrypoint,
            generic_entrypoint: base.generic_entrypoint,
            signature: base.signature,
        });
        (&raw mut (*closure).data).write(data);
        (*object).upper.store(lower, Ordering::Relaxed);
        (*object).aux.store(aux_create(object_flags(SPECIAL_FUNCTION, 0), lower), Ordering::Release);
        Ptr { ptr: lower, lower }
    }
}

/// The closure `function` points at, if it's one.
unsafe fn closure_of(function: Ptr) -> *mut Closure {
    object::check_function_call(function);
    if unsafe { (*function.object()).flags() } & FLAG_READONLY != 0 {
        safety_panic(format_args!("object {:p} is not a closure.", function.object()));
    }
    function.ptr.cast()
}

natives! {
    fn zclosure_new(thread: *mut Thread, function: Ptr, data: Ptr) -> Ptr {
        unsafe { closure_new(thread, function, data) }
    }

    fn zclosure_get_data(thread: *mut Thread, function: Ptr) -> Ptr {
        unsafe { crate::ptrs::atomic16::load(&raw mut (*closure_of(function)).data) }
    }

    fn zclosure_set_data(thread: *mut Thread, function: Ptr, data: Ptr) -> () {
        unsafe { crate::ptrs::atomic16::store(&raw mut (*closure_of(function)).data, data) };
    }

    fn zgc_alloc(thread: *mut Thread, size: usize) -> Ptr {
        Ptr::for_object(unsafe { object::allocate(thread, size, 0) })
    }

    fn zgc_aligned_alloc(thread: *mut Thread, alignment: usize, size: usize) -> Ptr {
        Ptr::for_object(unsafe { object::allocate_aligned(thread, size, alignment, 0) })
    }

    fn zgc_realloc(thread: *mut Thread, old: Ptr, size: usize) -> Ptr {
        unsafe { realloc(thread, old, size, MIN_ALIGN) }
    }

    fn zgc_aligned_realloc(thread: *mut Thread, old: Ptr, alignment: usize, size: usize) -> Ptr {
        unsafe { realloc(thread, old, size, alignment) }
    }

    fn zgc_realloc_preserving_alignment(thread: *mut Thread, old: Ptr, size: usize) -> Ptr {
        let alignment = if old.lower.is_null() {
            MIN_ALIGN
        } else {
            1 << unsafe { (*old.object()).flags() >> FLAGS_ALIGN_SHIFT & 31 }
        };
        unsafe { realloc(thread, old, size, alignment) }
    }

    fn zgc_free(thread: *mut Thread, ptr_: Ptr) -> () {
        unsafe { object::free(ptr_) };
    }

    fn zgetlower(thread: *mut Thread, ptr_: Ptr) -> Ptr {
        ptr_.with_ptr(ptr_.lower)
    }

    fn zgetupper(thread: *mut Thread, ptr_: Ptr) -> Ptr {
        if ptr_.lower.is_null() {
            return ptr_.with_ptr(ptr::null_mut());
        }
        ptr_.with_ptr(unsafe { (*ptr_.object()).upper() })
    }

    fn zhasvalidcap(thread: *mut Thread, ptr_: Ptr) -> bool {
        !ptr_.lower.is_null() && unsafe { (*ptr_.object()).flags() } & FLAG_FREE == 0
    }

    fn zmemset(thread: *mut Thread, dst: Ptr, value: u32, count: usize) -> () {
        unsafe { memops::set_memory(dst, value, count) };
    }

    fn zmemmove(thread: *mut Thread, dst: Ptr, src: Ptr, count: usize) -> () {
        unsafe { memops::move_memory(thread, dst, src, count) };
    }

    fn zstrlen(thread: *mut Thread, string: Ptr) -> usize {
        unsafe { string_of(string) }.len()
    }

    fn zisdigit(thread: *mut Thread, character: c_int) -> c_int {
        (b'0' as c_int..=b'9' as c_int).contains(&character) as c_int
    }

    fn zprint(thread: *mut Thread, string: Ptr) -> () {
        crate::failure::write_stderr(&unsafe { string_of(string) });
    }

    fn zerror(thread: *mut Thread, string: Ptr) -> () {
        let text = unsafe { string_of(string) };
        user_panic(format_args!("{}", String::from_utf8_lossy(&text)));
    }

    fn zsafety_error(thread: *mut Thread, string: Ptr) -> () {
        let text = unsafe { string_of(string) };
        safety_panic(format_args!("{}", String::from_utf8_lossy(&text)));
    }

    fn zgc_request_fresh(thread: *mut Thread) -> u64 {
        crate::gc::request_fresh()
    }

    fn zgc_wait(thread: *mut Thread, cycle: u64) -> () {
        unsafe { crate::gc::wait_for(thread, cycle) };
    }

    fn zis_runtime_testing_enabled(thread: *mut Thread) -> bool {
        false
    }
}
