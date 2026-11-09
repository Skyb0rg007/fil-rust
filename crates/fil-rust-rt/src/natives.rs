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
use crate::thread;
use crate::{cc, memops};

/// A cursor over the argument buffer.
pub struct Args {
    pub(crate) offset: usize,
    pub(crate) size: usize,
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

    /// Whether the caller gets an exception rather than this.
    fn is_exception(&self) -> bool {
        false
    }
}

/// What a native that can throw returns.
pub enum Thrown<T> {
    Value(T),
    Exception,
}

impl<T: Value> Value for Thrown<T> {
    unsafe fn take(_: *mut Thread, _: &mut Args) -> Self {
        crate::failure::internal_panic(format_args!("an exception can't be an argument"));
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        match self {
            Thrown::Value(value) => unsafe { value.give(thread) },
            Thrown::Exception => 0,
        }
    }

    fn is_exception(&self) -> bool {
        matches!(self, Thrown::Exception)
    }
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

// Floating-point values take a word like integers do, in its low bytes.
impl Value for f64 {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> f64 {
        f64::from_bits(unsafe { *cc::slot(thread, args.next()) })
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        unsafe { self.to_bits().give(thread) }
    }
}

/// A `long double`, as near as a `double` gets: 16 bytes at a 16-byte
/// boundary, the x87 80-bit value in the low 10.
#[derive(Clone, Copy)]
pub struct LongDouble(pub f64);

impl Value for LongDouble {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> LongDouble {
        args.offset = args.offset.next_multiple_of(16);
        let low = unsafe { *cc::slot(thread, args.next()) };
        let high = unsafe { *cc::slot(thread, args.next()) };
        LongDouble(x87_to_f64(low, high as u16))
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        let (mantissa, sign_exponent) = f64_to_x87(self.0);
        unsafe {
            cc::ensure_total(thread, 16);
            *cc::slot(thread, 0) = mantissa;
            *cc::aux_slot(thread, 0) = 0;
            *cc::slot(thread, WORD_SIZE) = sign_exponent as u64;
            *cc::aux_slot(thread, WORD_SIZE) = 0;
        }
        16
    }
}

/// An x87 80-bit extended value, as a `double`.
pub fn x87_to_f64(mantissa: u64, sign_exponent: u16) -> f64 {
    let negative = sign_exponent & 0x8000 != 0;
    let exponent = (sign_exponent & 0x7fff) as i32;
    let magnitude = if exponent == 0 && mantissa == 0 {
        0.0
    } else if exponent == 0x7fff {
        if mantissa << 1 == 0 { f64::INFINITY } else { f64::NAN }
    } else {
        // In two steps: for a result near the bottom of `double`'s range,
        // the power of two alone would underflow.
        let power = exponent - 16383 - 63;
        let first = power.clamp(-1000, 1000);
        (mantissa as f64) * 2f64.powi(first) * 2f64.powi(power - first)
    };
    if negative { -magnitude } else { magnitude }
}

/// A `double` as an x87 80-bit extended value: the mantissa with its explicit
/// integer bit, and the sign and exponent.
pub fn f64_to_x87(value: f64) -> (u64, u16) {
    let bits = value.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    match exponent {
        0 if fraction == 0 => (0, sign),
        0 => {
            // Subnormal: normalize, as the x87 format has room.
            let shift = fraction.leading_zeros() as i32;
            ((fraction << shift), sign | (16383 - 1022 - (shift - 11)) as u16)
        }
        0x7ff => ((1 << 63) | (fraction << 11), sign | 0x7fff),
        _ => ((1 << 63) | (fraction << 11), sign | (exponent - 1023 + 16383) as u16),
    }
}

impl Value for f32 {
    unsafe fn take(thread: *mut Thread, args: &mut Args) -> f32 {
        f32::from_bits(unsafe { *cc::slot(thread, args.next()) } as u32)
    }

    unsafe fn give(self, thread: *mut Thread) -> usize {
        unsafe { (self.to_bits() as u64).give(thread) }
    }
}

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

/// The arguments of a variadic function, past the named ones.
pub struct Rest {
    args: Args,
}

impl Rest {
    pub fn has_next(&self) -> bool {
        self.args.offset < self.args.size
    }

    pub unsafe fn next<T: Value>(&mut self, thread: *mut Thread) -> T {
        unsafe { T::take(thread, &mut self.args) }
    }

    /// Skips to the next multiple of `alignment`, where an argument aligned
    /// to more than a word starts.
    pub fn align(&mut self, alignment: usize) {
        self.args.offset = self.args.offset.next_multiple_of(alignment);
    }
}

impl Value for Rest {
    unsafe fn take(_: *mut Thread, args: &mut Args) -> Rest {
        Rest { args: Args { offset: args.offset, size: args.size } }
    }

    unsafe fn give(self, _: *mut Thread) -> usize {
        crate::failure::internal_panic(format_args!("variadic arguments can't be returned"));
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

/// The function object a getter hands back for a function nothing implements:
/// taking its address is fine, calling it fails with its name.
#[repr(C)]
struct MissingFunction {
    object: FunctionObject,
    /// The symbol, as the stub has it.
    name: *const std::ffi::c_char,
}

/// Where a missing function's name is, from the function itself, which is what
/// its entry point gets as `lower`.
const MISSING_NAME_OFFSET: usize =
    std::mem::offset_of!(MissingFunction, name) - std::mem::offset_of!(MissingFunction, object.function);

unsafe extern "C" fn missing_entry(_: *mut Thread, function: *mut u8, _: usize) -> ReturnValue {
    let name = unsafe { *function.add(MISSING_NAME_OFFSET).cast::<*const std::ffi::c_char>() };
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    crate::failure::missing(name.strip_prefix("pizlonated_").unwrap_or(&name));
}

/// The function object for `name`, a symbol the stub has no implementation
/// for: one per symbol, made the first time it's asked for. They never go
/// away.
pub fn missing_function(name: *const std::ffi::c_char) -> Ptr {
    static MADE: std::sync::Mutex<Option<std::collections::HashMap<usize, usize>>> = std::sync::Mutex::new(None);
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    let made = made.get_or_insert_with(Default::default);
    let function = match made.get(&(name as usize)) {
        Some(&function) => function as *mut u8,
        None => {
            let object = Box::into_raw(Box::new(MissingFunction {
                object: FunctionObject {
                    object: Object { upper: AtomicPtr::new(null_mut()), aux: AtomicUsize::new(0) },
                    function: Function {
                        fast_entrypoint: missing_entry as *const (),
                        generic_entrypoint: missing_entry,
                        signature: GENERIC_SIGNATURE,
                    },
                },
                name,
            }));
            let function = unsafe { (&raw mut (*object).object.function).cast::<u8>() };
            unsafe {
                (*object).object.object.upper.store(function, Ordering::Relaxed);
                let flags = FLAG_GLOBAL | FLAG_READONLY | object_flags(SPECIAL_FUNCTION, 0);
                (*object).object.object.aux.store(aux_create(flags, function), Ordering::Relaxed);
            }
            made.insert(name as usize, function as usize);
            function
        }
    };
    Ptr { ptr: function, lower: function }
}

macro_rules! natives {
    ($(fn $name:ident($thread:ident: *mut Thread $(, $arg:ident: $ty:ty)* $(,)?) -> $ret:ty $body:block)*) => {
        $(
            #[allow(non_snake_case)]
            mod $name {
                #[allow(unused_imports)]
                use super::*;
                use std::ptr::null_mut;
                use std::sync::atomic::AtomicPtr;
                use $crate::abi::{FunctionObject, Origin, Ptr, ReturnValue, Thread};
                use $crate::natives::{Args, Value, function_object};
                use $crate::thread::NativeFrame;

                pub unsafe extern "C" fn entry(thread: *mut Thread, _: *mut u8, size: usize) -> ReturnValue {
                    let frame = unsafe { NativeFrame::push(thread) };
                    #[allow(unused_mut, unused_variables)]
                    let mut args = Args { offset: 0, size };
                    $(let $arg = unsafe { <$ty as Value>::take(thread, &mut args) };)*
                    let result: $ret = unsafe { body(thread $(, $arg)*) };
                    let has_exception = result.is_exception();
                    let return_size = unsafe { result.give(thread) };
                    unsafe { frame.pop(thread) };
                    ReturnValue { has_exception, return_size }
                }

                // Natives take what the C function they stand for takes.
                #[allow(unused_variables, clippy::unused_unit, unused_mut, clippy::too_many_arguments)]
                unsafe fn body($thread: *mut Thread $(, mut $arg: $ty)*) -> $ret $body

                pub unsafe extern "C" fn getter(_: *mut Thread, _: *const Origin) -> Ptr {
                    static CELL: AtomicPtr<FunctionObject> = AtomicPtr::new(null_mut());
                    function_object(&CELL, entry)
                }
            }
        )*

        /// Stub slots for the natives here, keyed by symbol.
        pub fn table() -> Vec<(&'static str, *const ())> {
            vec![$((concat!("pizlonated_", stringify!($name)), $name::getter as *const ())),*]
        }
    };
}

pub(crate) use natives;

/// Copies out a null-terminated string that the access checks allow.
pub unsafe fn string_of(ptr_: Ptr) -> Vec<u8> {
    object::check_access(ptr_, 1, Access::Read);
    let available = unsafe { (*ptr_.object()).upper() } as usize - ptr_.ptr as usize;
    let bytes = unsafe { std::slice::from_raw_parts(ptr_.ptr, available) };
    match bytes.iter().position(|&byte| byte == 0) {
        Some(length) => bytes[..length].to_vec(),
        None => safety_panic(format_args!("string is not null-terminated (ptr = {:p}).", ptr_.ptr)),
    }
}

pub(crate) unsafe fn realloc(thread: *mut Thread, old: Ptr, size: usize, alignment: usize) -> Ptr {
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

    fn zcall(thread: *mut Thread, callee: Ptr, arguments: Ptr) -> Thrown<Ptr> {
        unsafe {
            let size = cc::demote_from_heap(thread, arguments);
            object::check_function_call(callee);
            let function = &*callee.ptr.cast::<Function>();
            let result = (function.generic_entrypoint)(thread, callee.lower, size);
            if result.has_exception {
                return Thrown::Exception;
            }
            Thrown::Value(cc::promote_to_heap(thread, result.return_size))
        }
    }
}
