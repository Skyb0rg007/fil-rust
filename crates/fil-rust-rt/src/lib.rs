//! A Fil-C runtime in Rust.
//!
//! Compiled Fil-C code needs a runtime: helpers for what it can't do inline, and
//! an object model the two share. The stock one, libpizlo, runs a concurrent
//! collector and assumes it owns the process. This one is a library. It starts
//! no threads, it has no collector thread, and nothing happens unless the host
//! program asks for it.
//!
//! Compiled libraries link against `libpizlo.so` by name, so `fil-rust-sys`
//! embeds a generated stand-in with that soname: one jump slot per symbol.
//! [`install`] loads it and fills in the slots.
//!
//! This code is ported from the runtime it replaces, so the failures and the
//! layouts match; see `docs/RUNTIME.md`.

#![allow(clippy::missing_safety_doc)]

pub mod abi;
mod calls;
mod tables;
mod cc;
#[cfg(target_arch = "x86_64")]
mod cpu;
pub mod failure;
pub mod gc;
mod glibc;
pub mod globals;
pub mod heap;
pub mod eh;
mod helpers;
mod jump;
mod maps;
mod net;
mod process;
mod queries;
mod memops;
mod natives;
pub mod signal;
pub mod object;
pub mod ptrs;
pub mod shim;
pub mod start;
pub mod sys;
pub mod thread;
pub mod zthread;

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::OnceLock;

use abi::*;
use fil_rust_sys::{Flavor, dlopen_bytes, dlopen_path, dlsym_checked};

unsafe extern "C" fn missing_helper(name: *const c_char) -> ! {
    failure::missing(&unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy());
}

/// A function's getter with nothing behind it: the function fails when it's
/// called, since taking its address doesn't mean it will be.
unsafe extern "C" fn missing_native(_: *mut Thread, _: *const Origin, name: *const c_char) -> Ptr {
    natives::missing_function(name)
}

/// A variable's getter with nothing behind it, which can't hand anything back.
unsafe extern "C" fn missing_data(_: *mut Thread, _: *const Origin, name: *const c_char) -> ! {
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    failure::missing(name.strip_prefix("pizlonated_").unwrap_or(&name));
}

/// What [`install`] loaded, for looking symbols up in.
#[derive(Clone, Copy, Debug)]
pub struct Installed {
    /// The stub, which has every symbol the runtime defines.
    pub runtime: usize,
    /// The stand-in that took the place of the Fil-C libc (`libc.so` or
    /// `libc.so.6666`).
    pub libc: usize,
}

fn load_stub(flavor: Flavor) -> Result<Installed, String> {
    let compat = flavor.compat_libs().ok_or_else(|| format!("{flavor:?} support wasn't built"))?;
    let flags = libc::RTLD_NOW | libc::RTLD_GLOBAL;
    for lib in compat {
        unsafe { dlopen_bytes(lib.soname, lib.bytes, flags) }?;
    }
    let mut stand_ins = Vec::new();
    for lib in flavor.libc_stand_ins() {
        stand_ins.push(unsafe { dlopen_bytes(lib.soname, lib.bytes, flags) }?);
    }
    let stub = flavor.runtime_stub().ok_or_else(|| format!("{flavor:?} has no runtime stub"))?;
    let handle = unsafe { dlopen_bytes(stub.soname, stub.bytes, flags) }?;
    // The support library binds to the stub, but runs nothing as it loads.
    let support = match flavor.support_library() {
        Some(lib) => unsafe { dlopen_bytes(lib.soname, lib.bytes, libc::RTLD_NOW | libc::RTLD_LOCAL) }?,
        None => std::ptr::null_mut(),
    };
    unsafe { fill_stub(handle, &runtime_table(), support) }?;
    // musl's stand-in has no slots: it's only a soname.
    if flavor == Flavor::Glibc {
        let natives: std::collections::HashMap<_, _> = glibc::table().into_iter().collect();
        for &stand_in in &stand_ins {
            unsafe { fill_stub(stand_in, &natives, std::ptr::null_mut()) }?;
        }
    }
    Ok(Installed { runtime: handle as usize, libc: stand_ins[0] as usize })
}

/// What implements the runtime stub's symbols, by name.
fn runtime_table() -> std::collections::HashMap<&'static str, *const ()> {
    let mut implemented: std::collections::HashMap<_, _> = helpers::table().into_iter().collect();
    implemented.extend(natives::table());
    implemented.extend(sys::table());
    implemented.extend(zthread::table());
    implemented.extend(jump::table());
    implemented.extend(eh::table());
    implemented.extend(signal::table());
    implemented.extend(maps::table());
    implemented.extend(net::table());
    implemented.extend(calls::table());
    implemented.extend(process::table());
    implemented.extend(tables::table());
    implemented.extend(queries::table());
    #[cfg(target_arch = "x86_64")]
    implemented.extend(cpu::table());
    implemented
}

/// Points a stub's slots at what implements them: `implemented`, or else the
/// support library, if there is one. That one goes through the runtime stub
/// as well, since the stub comes first in the search order even for the
/// library's own uses. The rest keep their failure paths.
unsafe fn fill_stub(
    stub: *mut c_void,
    implemented: &std::collections::HashMap<&'static str, *const ()>,
    support: *mut c_void,
) -> Result<(), String> {
    unsafe {
        let table = dlsym_checked(stub, "fr_stub_table\0")?.cast::<*const ()>();
        let names = dlsym_checked(stub, "fr_stub_names\0")?.cast::<*const c_char>();
        let count = *dlsym_checked(stub, "fr_stub_count\0")?.cast::<usize>();
        dlsym_checked(stub, "fr_stub_missing_native\0")?.cast::<*const ()>().write(missing_native as *const ());
        // Only the runtime stub has helpers, and only the libc's variables.
        for (slot, function) in [
            ("fr_stub_missing_helper\0", missing_helper as *const ()),
            ("fr_stub_missing_data\0", missing_data as *const ()),
        ] {
            let symbol = libc::dlsym(stub, slot.as_ptr().cast());
            if !symbol.is_null() {
                symbol.cast::<*const ()>().write(function);
            }
        }
        for index in 0..count {
            let name = std::ffi::CStr::from_ptr(*names.add(index));
            let text = name.to_str().map_err(|e| e.to_string())?;
            if let Some(&function) = implemented.get(text) {
                table.add(index).write(function);
            } else if !support.is_null() {
                let symbol = libc::dlsym(support, name.as_ptr());
                if !symbol.is_null() {
                    table.add(index).write(symbol.cast());
                }
            }
        }
    }
    Ok(())
}

/// Makes the process ready to load compiled libraries.
pub fn install(flavor: Flavor) -> Result<Installed, String> {
    static INSTALLED: OnceLock<Result<Installed, String>> = OnceLock::new();
    gc::configure_from_env();
    INSTALLED.get_or_init(|| load_stub(flavor)).clone()
}

/// A loaded Fil-C library.
pub struct Library {
    handle: *mut c_void,
}

unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    /// Loads a library, running its constructors on this thread. They can't
    /// import from libc yet.
    pub unsafe fn open(path: &Path) -> Result<Library, String> {
        let handle = unsafe { dlopen_path(path, libc::RTLD_NOW | libc::RTLD_LOCAL) }?;
        Ok(Library { handle })
    }

    /// A symbol of the library, as a pointer `run` closures can use.
    pub unsafe fn symbol(&self, thread: *mut Thread, name: &str) -> Result<Ptr, String> {
        let getter = unsafe { dlsym_checked(self.handle, &format!("pizlonated_{name}\0")) }?;
        let getter = unsafe { std::mem::transmute::<*mut c_void, GetterFn>(getter) };
        Ok(unsafe { getter(thread, std::ptr::null()) })
    }
}

/// Runs `body` on this thread as Fil-C code.
pub fn run<R>(body: impl FnOnce(*mut Thread) -> R) -> R {
    struct Exit(*mut Thread);
    impl Drop for Exit {
        fn drop(&mut self) {
            unsafe { thread::exit(self.0) };
        }
    }
    let thread = thread::attach_current();
    if unsafe { helpers::is_entered(thread) } {
        return body(thread);
    }
    unsafe { thread::enter(thread) };
    let _exit = Exit(thread);
    body(thread)
}

/// Calls a compiled function, and returns how much it returned. Integers go in
/// `ptr`, with a null `lower`.
pub unsafe fn call(thread: *mut Thread, callee: Ptr, args: &[Ptr], rets: &mut [Ptr]) -> usize {
    object::check_function_call(callee);
    let size = args.len() * WORD_SIZE;
    unsafe {
        cc::ensure_total(thread, size);
        for (index, arg) in args.iter().enumerate() {
            *cc::slot(thread, index * WORD_SIZE) = arg.ptr as u64;
            *cc::aux_slot(thread, index * WORD_SIZE) = arg.lower as u64;
        }
        let function = &*callee.ptr.cast::<Function>();
        let result = (function.generic_entrypoint)(thread, callee.lower, size);
        if result.has_exception {
            failure::internal_panic(format_args!("an exception escaped a function the runtime called"));
        }
        for (index, ret) in rets.iter_mut().enumerate() {
            let offset = index * WORD_SIZE;
            if offset < result.return_size {
                ret.ptr = *cc::slot(thread, offset) as *mut u8;
                ret.lower = *cc::aux_slot(thread, offset) as *mut u8;
            }
        }
        result.return_size
    }
}
