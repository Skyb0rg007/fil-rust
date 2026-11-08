//! What Fil-C code can find out about the processor: `cpuid`, `xgetbv`, and
//! the `__cpu_model` and `__cpu_features2` that `__builtin_cpu_supports` and
//! `__builtin_cpu_is` read. Only x86-64 has these.
//!
//! compiler-rt fills those two in (`fil-rust-sys/csrc/compiler-rt`), in the
//! layout Fil-C's clang reads them in, and they're copied into global objects
//! the first time Fil-C code asks for either. A C program gets them filled in
//! by a constructor, so code needn't call `__builtin_cpu_init` first. libpizlo
//! only fills them in when it does, and only has the first word of
//! `__cpu_features2`, of three: issue 7 in `docs/upstream-filc-issues.md`.

use std::ffi::c_int;
use std::sync::Once;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::abi::*;
use crate::natives::natives;
use crate::sys::one_out;

unsafe extern "C" {
    static fr_cpu_model: [u32; 4];
    static fr_cpu_features2: [u32; 3];
    fn fr_cpu_indicator_init() -> c_int;
}

/// A global object with room for four words.
#[repr(C, align(16))]
struct Global {
    object: Object,
    payload: [AtomicU32; 4],
}

impl Global {
    const fn new() -> Global {
        Global {
            object: Object { upper: AtomicPtr::new(std::ptr::null_mut()), aux: AtomicUsize::new(0) },
            payload: [const { AtomicU32::new(0) }; 4],
        }
    }

    fn fill(&self, words: &[u32]) {
        for (slot, &word) in self.payload.iter().zip(words) {
            slot.store(word, Ordering::Relaxed);
        }
        let upper = self.payload.as_ptr().wrapping_add(words.len()).cast_mut().cast::<u8>();
        self.object.upper.store(upper, Ordering::Relaxed);
        self.object.aux.store(aux_create(FLAG_GLOBAL, std::ptr::null_mut()), Ordering::Release);
    }

    fn ptr(&self) -> Ptr {
        Ptr::for_object(std::ptr::from_ref(&self.object).cast_mut())
    }
}

static MODEL: Global = Global::new();
static FEATURES2: Global = Global::new();

fn filled() {
    static FILLED: Once = Once::new();
    FILLED.call_once(|| unsafe {
        fr_cpu_indicator_init();
        MODEL.fill(&*std::ptr::addr_of!(fr_cpu_model));
        FEATURES2.fill(&*std::ptr::addr_of!(fr_cpu_features2));
    });
}

unsafe extern "C" fn cpu_model(_: *mut Thread, _: *const Origin) -> Ptr {
    filled();
    MODEL.ptr()
}

unsafe extern "C" fn cpu_features2(_: *mut Thread, _: *const Origin) -> Ptr {
    filled();
    FEATURES2.ptr()
}

/// The natives, and the getters of the two globals.
pub fn table() -> Vec<(&'static str, *const ())> {
    let mut table = instructions::table();
    table.push(("pizlonated___cpu_model", cpu_model as *const ()));
    table.push(("pizlonated___cpu_features2", cpu_features2 as *const ()));
    table
}

fn cpuid(leaf: u32, count: u32, eax: Ptr, ebx: Ptr, ecx: Ptr, edx: Ptr) {
    let result = std::arch::x86_64::__cpuid_count(leaf, count);
    for (ptr, value) in [(eax, result.eax), (ebx, result.ebx), (ecx, result.ecx), (edx, result.edx)] {
        unsafe { one_out::<u32>(ptr).write_unaligned(value) };
    }
}

mod instructions {
    use super::*;

    natives! {
        fn __cpu_indicator_init(thread: *mut Thread) -> () {
            filled();
        }

        fn zcpuid(thread: *mut Thread, leaf: u32, eax: Ptr, ebx: Ptr, ecx: Ptr, edx: Ptr) -> () {
            cpuid(leaf, 0, eax, ebx, ecx, edx);
        }

        fn zcpuid_count(thread: *mut Thread, leaf: u32, count: u32, eax: Ptr, ebx: Ptr, ecx: Ptr, edx: Ptr) -> () {
            cpuid(leaf, count, eax, ebx, ecx, edx);
        }

        // Traps if the processor has no XSAVE, as the instruction does.
        fn zxgetbv(thread: *mut Thread) -> u64 {
            let (low, high): (u32, u32);
            unsafe { std::arch::asm!("xgetbv", in("ecx") 0, out("eax") low, out("edx") high, options(nomem, nostack)) };
            u64::from(low) | u64::from(high) << 32
        }
    }
}
