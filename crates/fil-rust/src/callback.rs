//! Rust closures callable from Fil-C.

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fil_rust_sys::FilcThread;

use crate::call::{FilFn, FilSig};
use crate::handle::FilHandle;
use crate::ptr::{FilPtr, Opaque};
use crate::runtime::{Inner, lock_anywhere, set_current_thread};
use crate::{Fil, Result};

trait Erased: Send + Sync {
    fn invoke(&self, fil: &Fil<'_>, argument_size: usize) -> usize;
}

struct Typed<Sig, F> {
    f: F,
    _marker: PhantomData<fn() -> Sig>,
}

impl<Sig, F> Erased for Typed<Sig, F>
where
    Sig: FilSig,
    F: for<'f> Fn(&Fil<'f>, Sig::Args<'f>) -> Sig::Ret<'f> + Send + Sync,
{
    fn invoke(&self, fil: &Fil<'_>, argument_size: usize) -> usize {
        let (args, sret) = Sig::callback_args(fil, argument_size);
        let ret = (self.f)(fil, args);
        Sig::callback_return(fil, &ret, sret)
            .unwrap_or_else(|e| fail(fil, &format!("encoding a callback's return value: {e}")))
    }
}

pub(crate) struct Registry {
    map: Mutex<HashMap<u64, Arc<dyn Erased>>>,
    next: AtomicU64,
}

impl Registry {
    pub(crate) fn new() -> Registry {
        Registry { map: Mutex::new(HashMap::new()), next: AtomicU64::new(1) }
    }
}

/// Reports a fatal error inside a callback. Unwinding can't cross back into
/// Fil-C, so this ends the process with a Fil-C panic.
pub(crate) fn fail(fil: &Fil<'_>, message: &str) -> ! {
    let msg = std::ffi::CString::new(format!("fil-rust: {message}"))
        .unwrap_or_else(|_| c"fil-rust: callback failed".to_owned());
    unsafe { (fil.shim().fr_panic)(msg.as_ptr()) };
    std::process::abort()
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Called by the shim's callback thunk on whatever Fil-C thread invoked the
/// closure. The thread is entered and inside a fresh native frame.
pub(crate) unsafe extern "C" fn callback_entry(
    thread: *mut FilcThread,
    id: u64,
    argument_size: usize,
) -> usize {
    let inner: &'static Inner = crate::Runtime::get().expect("runtime is running").inner();
    let fil = unsafe { Fil::new(inner, thread) };
    let previous = set_current_thread(thread);
    let callback = fil.lock(&inner.callbacks.map).get(&id).cloned();
    let Some(callback) = callback else {
        fail(&fil, "a callback was invoked after it was dropped");
    };
    let result = catch_unwind(AssertUnwindSafe(|| callback.invoke(&fil, argument_size)));
    set_current_thread(previous);
    match result {
        Ok(size) => size,
        Err(panic) => fail(&fil, &format!("a Rust callback panicked: {}", panic_message(&*panic))),
    }
}

/// A Rust closure exposed to Fil-C as a function pointer of signature `Sig`.
///
/// The closure receives a [`Fil`] for the calling thread plus its decoded
/// arguments, and may be called from any Fil-C thread, concurrently, which
/// is why it must be `Send + Sync`. Arguments are decoded with the same
/// checks as calls into Fil-C: a caller passing too few arguments gets a
/// Fil-C panic, and pointers keep their capabilities.
///
/// Dropping the `Callback` unregisters it; Fil-C code that calls the stale
/// function pointer afterwards is stopped with a Fil-C panic. A panic inside
/// the closure also ends the process (it can't unwind into Fil-C).
pub struct Callback<Sig: FilSig> {
    inner: &'static Inner,
    id: u64,
    closure: FilHandle<Opaque>,
    _marker: PhantomData<fn() -> Sig>,
}

impl<Sig: FilSig> std::fmt::Debug for Callback<Sig> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Callback(#{})", self.id)
    }
}

impl<Sig: FilSig> Callback<Sig> {
    /// Registers `f` and creates the Fil-C function pointer for it.
    pub fn new<F>(fil: &Fil<'_>, f: F) -> Result<Callback<Sig>>
    where
        F: for<'f> Fn(&Fil<'f>, Sig::Args<'f>) -> Sig::Ret<'f> + Send + Sync + 'static,
    {
        let inner = fil.inner;
        let id = inner.callbacks.next.fetch_add(1, Ordering::Relaxed);
        let erased: Arc<dyn Erased> = Arc::new(Typed::<Sig, F> { f, _marker: PhantomData });
        fil.lock(&inner.callbacks.map).insert(id, erased);
        let raw = unsafe { (fil.shim().fr_closure_new)(fil.thread, id) };
        let closure = match fil.root(FilPtr::<Opaque>::from_raw(raw)) {
            Ok(handle) => handle,
            Err(e) => {
                fil.lock(&inner.callbacks.map).remove(&id);
                return Err(e);
            }
        };
        Ok(Callback { inner, id, closure, _marker: PhantomData })
    }

    /// The function pointer to hand to Fil-C.
    pub fn get<'f>(&self, fil: &Fil<'f>) -> FilFn<'f, Sig> {
        FilFn::from_raw_unchecked(self.closure.get(fil).raw())
    }
}

impl<Sig: FilSig> Drop for Callback<Sig> {
    fn drop(&mut self) {
        // In a forked child the registry's lock may be held forever by a
        // thread that didn't survive the fork: leave the entry (the handle
        // leaks the same way).
        if crate::runtime::in_forked_child() {
            return;
        }
        let removed = lock_anywhere(self.inner, &self.inner.callbacks.map).remove(&self.id);
        // Drop the closure after releasing the lock: its destructor may do
        // anything, including creating or dropping other callbacks.
        drop(removed);
    }
}

impl<'f> Fil<'f> {
    /// Shorthand for [`Callback::new`].
    pub fn callback<Sig, F>(&self, f: F) -> Result<Callback<Sig>>
    where
        Sig: FilSig,
        F: for<'g> Fn(&Fil<'g>, Sig::Args<'g>) -> Sig::Ret<'g> + Send + Sync + 'static,
    {
        Callback::new(self, f)
    }
}
