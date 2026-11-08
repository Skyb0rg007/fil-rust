//! The layouts compiled Fil-C code assumes.
//!
//! The compiler hard-codes these (`FilPizlonator.cpp`), so nothing here can
//! change without recompiling every Fil-C library. Offsets are asserted at
//! compile time, and `tests/layout.rs` checks them against the pinned runtime
//! headers. Written against Fil-C 0.687, whose ABI upstream calls `fil1`.

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering, AtomicU32};

pub const WORD_SIZE: usize = 8;
/// Objects are aligned to this, and it's the granule of the mark bitmap.
pub const MIN_ALIGN: usize = 16;
pub const OBJECT_SIZE: usize = size_of::<Object>();

pub const AUX_PTR_MASK: usize = 0xffff_ffff_ffff;
pub const AUX_FLAGS_SHIFT: u32 = 48;

pub const FLAG_GLOBAL: usize = 1;
pub const FLAG_READONLY: usize = 2;
pub const FLAG_FREE: usize = 4;
pub const FLAG_MMAP: usize = 8;
pub const FLAG_GLOBAL_AUX: usize = 16;
pub const FLAGS_SPECIAL_SHIFT: u32 = 7;
pub const FLAGS_SPECIAL_MASK: usize = 15 << FLAGS_SPECIAL_SHIFT;
pub const FLAGS_ALIGN_SHIFT: u32 = 11;

pub const SPECIAL_FUNCTION: usize = 1;
pub const SPECIAL_THREAD: usize = 2;
pub const SPECIAL_SIGNAL_HANDLER: usize = 3;
pub const SPECIAL_DL_HANDLE: usize = 6;
pub const SPECIAL_JMP_BUF: usize = 7;
pub const SPECIAL_PTR_TABLE: usize = 4;
pub const SPECIAL_EXACT_PTR_TABLE: usize = 8;
pub const SPECIAL_WEAK: usize = 9;
pub const SPECIAL_WEAK_MAP: usize = 10;
pub const SPECIAL_FINALIZER_QUEUE: usize = 11;

/// Marks an aux word that holds a box pointer rather than a `lower`.
pub const ATOMIC_BOX_BIT: usize = 1;

pub const STATE_ENTERED: u8 = 1;
pub const STATE_CHECK_REQUESTED: u8 = 2;
pub const STATE_STOP_REQUESTED: u8 = 4;
pub const STATE_DEFERRED_SIGNAL: u8 = 8;

pub const CC_ALIGNMENT: usize = 64;
pub const CC_INLINE_SIZE: usize = 256;

/// A function object whose calls all go through the generic entrypoint.
pub const GENERIC_SIGNATURE: u64 = 0;

/// Inline allocations have one slot per 16 bytes of total size, up to 416.
pub const NUM_ALLOCATOR_SLOTS: usize = 27;
pub const ALLOCATOR_SLOTS_OFFSET: usize = 3072;
/// The compiler bakes in one page size per architecture, whatever the kernel's
/// is, so this doesn't depend on the machine.
#[cfg(target_arch = "x86_64")]
pub const ALLOCATOR_SLOT_SIZE: usize = 208;
#[cfg(target_arch = "aarch64")]
pub const ALLOCATOR_SLOT_SIZE: usize = 568;

/// A flight pointer: an address and the capability that grants access through
/// it. Compiled code passes these flattened into two words, so helpers take
/// `ptr` and `lower` as separate parameters; this form is for returning them
/// (in two registers) and holding them.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ptr {
    pub ptr: *mut u8,
    pub lower: *mut u8,
}

impl Ptr {
    pub const NULL: Ptr = Ptr { ptr: std::ptr::null_mut(), lower: std::ptr::null_mut() };

    pub fn for_object(object: *mut Object) -> Ptr {
        let lower = lower_for_object(object);
        Ptr { ptr: lower, lower }
    }

    pub fn object(self) -> *mut Object {
        object_for_lower(self.lower)
    }

    pub fn with_ptr(self, ptr: *mut u8) -> Ptr {
        Ptr { ptr, lower: self.lower }
    }

    /// Bytes from `ptr` to the end of its object.
    pub fn available(self) -> usize {
        if self.lower.is_null() {
            return 0;
        }
        (unsafe { &*self.object() }.upper() as usize).saturating_sub(self.ptr as usize)
    }

    /// How far `ptr` is past `lower`. Wraps for a pointer below its object.
    pub fn offset(self) -> usize {
        (self.ptr as usize).wrapping_sub(self.lower as usize)
    }
}

/// The header in front of every object's payload. A `lower` points just past
/// it, and that one address is the whole of a pointer's capability.
#[repr(C)]
pub struct Object {
    pub upper: AtomicPtr<u8>,
    pub aux: AtomicUsize,
}

pub fn lower_for_object(object: *mut Object) -> *mut u8 {
    if object.is_null() { std::ptr::null_mut() } else { object.wrapping_add(1).cast() }
}

pub fn object_for_lower(lower: *mut u8) -> *mut Object {
    if lower.is_null() { std::ptr::null_mut() } else { lower.cast::<Object>().wrapping_sub(1) }
}

pub fn aux_flags(aux: usize) -> usize {
    aux >> AUX_FLAGS_SHIFT
}

pub fn aux_ptr(aux: usize) -> *mut u8 {
    (aux & AUX_PTR_MASK) as *mut u8
}

pub fn aux_create(flags: usize, ptr: *mut u8) -> usize {
    debug_assert_eq!(ptr as usize & !AUX_PTR_MASK, 0);
    (flags << AUX_FLAGS_SHIFT) | ptr as usize
}

pub fn object_flags(special: usize, log_align: usize) -> usize {
    (special << FLAGS_SPECIAL_SHIFT) | (log_align << FLAGS_ALIGN_SHIFT)
}

impl Object {
    pub fn flags(&self) -> usize {
        aux_flags(self.aux.load(Ordering::Relaxed))
    }

    pub fn is_special(&self) -> bool {
        self.flags() & FLAGS_SPECIAL_MASK != 0
    }

    pub fn special_type(&self) -> usize {
        (self.flags() & FLAGS_SPECIAL_MASK) >> FLAGS_SPECIAL_SHIFT
    }

    pub fn aux_ptr(&self) -> *mut u8 {
        aux_ptr(self.aux.load(Ordering::Acquire))
    }

    pub fn lower(&self) -> *mut u8 {
        lower_for_object(std::ptr::from_ref(self).cast_mut())
    }

    pub fn upper(&self) -> *mut u8 {
        self.upper.load(Ordering::Relaxed)
    }

    /// The payload size. Zero once the object is freed, since `free` sets
    /// `upper` to `lower`.
    pub fn size(&self) -> usize {
        (self.upper() as usize).wrapping_sub(self.lower() as usize)
    }
}

/// A two-word atomic pointer, reached through an aux word with the box bit
/// set. It has no object header of its own.
#[repr(C)]
pub struct AtomicBox {
    pub ptr: *mut u8,
    pub lower: *mut u8,
}

#[repr(C)]
pub struct PtrPair {
    pub first: *mut u8,
    pub second: *mut u8,
}

#[repr(C)]
pub struct Function {
    pub fast_entrypoint: *const (),
    pub generic_entrypoint: PizlonatedFn,
    pub signature: u64,
}

#[repr(C, align(16))]
pub struct FunctionObject {
    pub object: Object,
    pub function: Function,
}

/// A function object that carries a pointer along. It's reachable from the
/// object's `lower`, which is the only argument its entry point gets for it.
#[repr(C, align(16))]
pub struct Closure {
    pub function: Function,
    /// `Ptr` is aligned like its fields, so it can pass by value the same way
    /// on every target. `filc_ptr` is aligned to 16.
    _padding: u64,
    pub data: Ptr,
}

/// Returned in two registers: the flag and how much of the CC buffers holds
/// return values.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReturnValue {
    pub has_exception: bool,
    pub return_size: usize,
}

pub type PizlonatedFn = unsafe extern "C" fn(*mut Thread, *mut u8, usize) -> ReturnValue;
pub type GetterFn = unsafe extern "C" fn(*mut Thread, *const Origin) -> Ptr;

#[repr(C)]
pub struct OriginNode {
    pub function: *const std::ffi::c_char,
    pub filename: *const std::ffi::c_char,
    /// A frame's `lowers` length, or `u32::MAX` for an inline frame.
    pub num_lowers_ish: u32,
}

#[repr(C)]
pub struct FunctionOrigin {
    pub base: OriginNode,
    pub personality_getter: *const (),
    pub can_throw: bool,
    pub can_catch: bool,
    pub num_setjmps: u32,
    pub num_stack_auxes: u32,
}

#[repr(C)]
pub struct Origin {
    pub origin_node: *const OriginNode,
    pub line: u32,
    pub column: u32,
}

/// What an origin really is if its function has a personality.
#[repr(C)]
pub struct OriginWithEh {
    pub base: Origin,
    pub eh_data_getter: *const (),
}

/// The function an origin is in, however many inlined calls deep.
pub unsafe fn function_origin(mut origin: *const Origin) -> *const FunctionOrigin {
    loop {
        let node = unsafe { (*origin).origin_node };
        if unsafe { (*node).num_lowers_ish } != u32::MAX {
            return node.cast();
        }
        origin = unsafe { &raw const (*node.cast::<InlineFrame>()).caller };
    }
}

#[repr(C)]
pub struct InlineFrame {
    pub base: OriginNode,
    pub caller: Origin,
}

/// A compiled frame: `parent`, `origin`, then `lowers`. The lowers are precise
/// roots, and `origin` is how many there are.
#[repr(C)]
pub struct Frame {
    pub parent: *mut Frame,
    pub origin: *const Origin,
    pub lowers: [*mut u8; 0],
}

#[repr(C)]
pub struct StackAux {
    pub num_lowers: usize,
    pub lowers: [*mut u8; 0],
}

/// One thread-local allocator of `Thread::slots`. Compiled code computes the
/// address and passes it to `verse_local_allocator_allocate`, so the contents
/// are ours.
#[repr(C)]
pub struct AllocatorSlot {
    pub free: *mut u8,
    pub bump: *mut u8,
    pub end: *mut u8,
    /// The chunk `bump` is carving, plus one, or 0 if there isn't one.
    pub chunk: usize,
    /// The size class: `block size / 16`, which is also this slot's index.
    pub class: usize,
    _rest: [u8; ALLOCATOR_SLOT_SIZE - 40],
}

/// `filc_thread`, as far as compiled code can see it. Everything between
/// `cc_outline_size` and `slots` is the runtime's own.
#[repr(C, align(64))]
pub struct Thread {
    pub stack_limit: *mut u8,
    pub state: AtomicU8,
    _pad0: [u8; 3],
    pub tid: u32,
    pub top_frame: *mut Frame,
    _pad1: [u8; 8],
    pub unwind_registers: [Ptr; 2],
    pub cookie: Ptr,
    _pad2: [u8; 48],
    pub cc_inline: [u64; CC_INLINE_SIZE / 8],
    pub cc_inline_aux: [u64; CC_INLINE_SIZE / 8],
    pub cc_outline: *mut u8,
    pub cc_outline_aux: *mut u8,
    pub cc_outline_size: usize,
    /// The runtime's own per-thread state.
    pub extra: *mut crate::thread::ThreadExtra,
    /// Where the thread's stack ends and begins, kept for after it's gone.
    pub stack_low: *mut u8,
    pub stack_top: *mut u8,
    /// What it passed to `zthread_exit`, for whoever joins it.
    pub result: Ptr,
    /// The `pthread_t`, while the thread is running.
    pub host: usize,
    /// Set by a thread that's finished, under `thread::lock_lifecycle`.
    pub stopped: AtomicU32,
    /// Set on threads left behind by a `fork`, which never finish.
    pub forked: AtomicU32,
    /// How many `zincrement_signal_deferral_depth` calls are in effect.
    pub signal_deferral_depth: u64,
    _pad3: [u8; ALLOCATOR_SLOTS_OFFSET - 728],
    pub slots: [AllocatorSlot; NUM_ALLOCATOR_SLOTS],
}

const _: () = {
    assert!(size_of::<Ptr>() == 16);
    assert!(size_of::<Object>() == 16);
    assert!(size_of::<AtomicBox>() == 16);
    assert!(size_of::<AllocatorSlot>() == ALLOCATOR_SLOT_SIZE);
    assert!(offset_of!(Thread, stack_limit) == 0);
    assert!(offset_of!(Thread, state) == 8);
    assert!(offset_of!(Thread, tid) == 12);
    assert!(offset_of!(Thread, top_frame) == 16);
    assert!(offset_of!(Thread, unwind_registers) == 32);
    assert!(offset_of!(Thread, cookie) == 64);
    assert!(offset_of!(Thread, cc_inline) == 128);
    assert!(offset_of!(Thread, cc_inline_aux) == 384);
    assert!(offset_of!(Thread, cc_outline) == 640);
    assert!(offset_of!(Thread, cc_outline_aux) == 648);
    assert!(offset_of!(Thread, cc_outline_size) == 656);
    assert!(offset_of!(Thread, slots) == ALLOCATOR_SLOTS_OFFSET);
    assert!(offset_of!(Function, generic_entrypoint) == 8);
    assert!(offset_of!(Function, signature) == 16);
    assert!(offset_of!(FunctionObject, function) == 16);
    assert!(offset_of!(FunctionOrigin, personality_getter) == 24);
    assert!(offset_of!(Origin, line) == 8);
    assert!(offset_of!(Frame, lowers) == 16);
    assert!(offset_of!(StackAux, lowers) == 8);
};
