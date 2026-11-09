//! Collections at the point a callback starts, which `fil-rust` can't arrange:
//! it decodes the arguments first, and that's what keeps them alive after.
//! Compiled callers keep every pointer in a frame slot as well, so the case is
//! a caller that's in Rust, with the argument in the buffers and nowhere else.

use std::sync::atomic::{AtomicUsize, Ordering};

use fil_rust_rt::abi::Ptr;
use fil_rust_rt::{call, gc, install, object, run, shim};
use fil_rust_sys::{FilcThread, Flavor};

/// Live bytes as of the collection the callback started.
static LIVE_IN_CALLBACK: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn no_main(_: *mut FilcThread, _: std::ffi::c_int) {}

unsafe extern "C" fn collect_then_return(thread: *mut FilcThread, _: u64, _: usize) -> usize {
    unsafe {
        gc::collect(thread.cast());
        LIVE_IN_CALLBACK.store(gc::stats().live_bytes, Ordering::Relaxed);
        shim::fr_cc_set_word(thread, 8, 0, 0);
    }
    8
}

#[test]
fn arguments_are_alive_as_a_callback_starts() {
    install(Flavor::Musl).unwrap();
    unsafe { shim::fr_init(no_main, collect_then_return) };
    gc::set_collection(gc::Collection::Manual);
    let (with_argument, without) = run(|thread| unsafe {
        let closure = shim::fr_closure_new(thread.cast(), 1);
        let closure = Ptr { ptr: closure.ptr.cast(), lower: closure.lower.cast() };
        let measure = |argument| {
            call(thread, closure, &[argument], &mut [Ptr::NULL]);
            LIVE_IN_CALLBACK.load(Ordering::Relaxed)
        };
        let with_argument = measure(Ptr::for_object(object::allocate(thread, 1 << 12, 0)));
        (with_argument, measure(Ptr::NULL))
    });
    assert!(with_argument >= without + (1 << 12), "{with_argument} {without}");
}
