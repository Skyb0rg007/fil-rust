//! The environment, which is the process's: Rust's `std::env` and the host
//! libc see what Fil-C code sets, and the other way around. What `getenv`
//! returns is a copy, which stays the same while the variable does; `environ`
//! is a snapshot, made again when Fil-C code changes the environment. Writing
//! to either changes the copy, not the environment.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString, c_int};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::natives;
use crate::start::set_errno;
use crate::thread::{self, NativeFrame};
use crate::{globals, object, ptrs, view};

/// What `getenv` handed out for each variable, as long as it hasn't changed.
/// These are roots; older copies live as long as the program holds them.
static COPIES: Mutex<Option<HashMap<OsString, Held>>> = Mutex::new(None);

/// A variable's value, and the copy of it `getenv` handed out (`ptr`, `lower`).
type Held = (OsString, usize, usize);

/// Calls `visit` on the copies `getenv` may hand out again.
pub fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let copies = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    for &(_, _, lower) in copies.iter().flat_map(|copies| copies.values()) {
        visit(object_for_lower(lower as *mut u8));
    }
}

fn cached(name: &OsStr, value: &OsStr) -> Option<Ptr> {
    let copies = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let (held, ptr, lower) = copies.as_ref()?.get(name)?;
    (held == value).then_some(Ptr { ptr: *ptr as *mut u8, lower: *lower as *mut u8 })
}

unsafe fn getenv_copy(me: *mut Thread, name: Ptr) -> Ptr {
    let name = OsString::from_vec(view::c_string(name));
    if name.is_empty() || name.as_bytes().contains(&b'=') {
        return Ptr::NULL;
    }
    let Some(value) = std::env::var_os(&name) else { return Ptr::NULL };
    if let Some(copy) = cached(&name, &value) {
        return copy;
    }
    // Made outside the lock, which a collection takes. So it's held only
    // while entered, and never across a safepoint.
    let copy = unsafe { view::new_c_string(me, value.as_bytes()) };
    let mut copies = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    copies.get_or_insert_with(HashMap::new).insert(name, (value, copy.ptr as usize, copy.lower as usize));
    copy
}

/// The name `setenv` and `unsetenv` were given, if it's one.
fn variable(thread: *mut Thread, name: Ptr) -> Option<OsString> {
    let name = if name.ptr.is_null() { Vec::new() } else { view::c_string(name) };
    if name.is_empty() || name.contains(&b'=') {
        unsafe { set_errno(thread, libc::EINVAL) };
        return None;
    }
    Some(OsString::from_vec(name))
}

fn secure() -> bool {
    unsafe { libc::getauxval(libc::AT_SECURE) != 0 }
}

natives! {
    fn getenv(thread: *mut Thread, name: Ptr) -> Ptr {
        unsafe { getenv_copy(thread, name) }
    }

    fn secure_getenv(thread: *mut Thread, name: Ptr) -> Ptr {
        if secure() { Ptr::NULL } else { unsafe { getenv_copy(thread, name) } }
    }

    fn setenv(thread: *mut Thread, name: Ptr, value: Ptr, overwrite: c_int) -> c_int {
        let Some(name) = variable(thread, name) else { return -1 };
        let value = OsString::from_vec(view::c_string(value));
        if overwrite != 0 || std::env::var_os(&name).is_none() {
            unsafe { crate::sys::plain(thread, || std::env::set_var(&name, &value)) };
            unsafe { refresh_environ(thread, false) };
        }
        0
    }

    fn unsetenv(thread: *mut Thread, name: Ptr) -> c_int {
        let Some(name) = variable(thread, name) else { return -1 };
        unsafe { crate::sys::plain(thread, || std::env::remove_var(&name)) };
        unsafe { refresh_environ(thread, false) };
        0
    }

    // glibc keeps the string itself, so changing it later changes the
    // variable. Here it's copied.
    fn putenv(thread: *mut Thread, string: Ptr) -> c_int {
        let string = view::c_string(string);
        match string.iter().position(|&byte| byte == b'=') {
            Some(0) => {
                unsafe { set_errno(thread, libc::EINVAL) };
                -1
            }
            Some(equals) => {
                let (name, value) = (OsStr::from_bytes(&string[..equals]), OsStr::from_bytes(&string[equals + 1..]));
                unsafe { crate::sys::plain(thread, || std::env::set_var(name, value)) };
                unsafe { refresh_environ(thread, false) };
                0
            }
            None => {
                unsafe { crate::sys::plain(thread, || std::env::remove_var(OsStr::from_bytes(&string))) };
                unsafe { refresh_environ(thread, false) };
                0
            }
        }
    }

    fn clearenv(thread: *mut Thread) -> c_int {
        unsafe {
            crate::sys::plain(thread, || {
                for (name, _) in std::env::vars_os() {
                    std::env::remove_var(name);
                }
            })
        };
        unsafe { refresh_environ(thread, false) };
        0
    }
}

/// A one-pointer object that lives for good: the storage behind a variable.
unsafe fn holder(me: *mut Thread) -> Ptr {
    unsafe {
        let object = object::allocate(me, WORD_SIZE, 0);
        globals::keep(object);
        Ptr::for_object(object)
    }
}

/// An array of new strings and a null, tracked by this native.
unsafe fn string_array(me: *mut Thread, strings: &[Vec<u8>]) -> Ptr {
    unsafe {
        let array = object::allocate(me, (strings.len() + 1) * WORD_SIZE, 0);
        thread::track(me, array);
        let array = Ptr::for_object(array);
        for (index, string) in strings.iter().enumerate() {
            let copy = view::new_c_string(me, string);
            ptrs::store(me, array.with_ptr(array.ptr.add(index * WORD_SIZE)), copy);
        }
        array
    }
}

struct Environ {
    holder: (usize, usize),
    snapshot: Vec<Vec<u8>>,
}

static ENVIRON: Mutex<Option<Environ>> = Mutex::new(None);

fn entries() -> Vec<Vec<u8>> {
    std::env::vars_os()
        .map(|(name, value)| {
            let mut entry = name.into_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            entry
        })
        .collect()
}

/// Makes `environ`'s snapshot again if the environment has changed since, and
/// returns its storage. With `create` false, does nothing unless `environ` has
/// been asked for.
unsafe fn refresh_environ(me: *mut Thread, create: bool) -> Ptr {
    unsafe {
        let now = entries();
        let current = {
            let state = thread::lock_entered(me, &ENVIRON);
            state.as_ref().map(|state| (state.holder, state.snapshot == now))
        };
        match current {
            None if !create => Ptr::NULL,
            Some(((ptr, lower), true)) => Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 },
            current => {
                let frame = NativeFrame::push(me);
                let holder = match current {
                    Some(((ptr, lower), _)) => Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 },
                    None => holder(me),
                };
                let array = string_array(me, &now);
                ptrs::store(me, holder, array);
                let mut state = thread::lock_entered(me, &ENVIRON);
                *state = Some(Environ { holder: (holder.ptr as usize, holder.lower as usize), snapshot: now });
                frame.pop(me);
                holder
            }
        }
    }
}

/// `environ`'s storage. Compiled code asks for it once and keeps it, so the
/// snapshot there is made again when Fil-C code changes the environment;
/// what Rust changes shows up then too, and in `getenv` straight away.
unsafe extern "C" fn environ(me: *mut Thread, _: *const Origin) -> Ptr {
    unsafe { refresh_environ(me, true) }
}

/// The program's name as the host started it, and its last component.
fn invocation_names(me: *mut Thread) -> [Ptr; 2] {
    // Not a `OnceLock`: waiting for one while entered could hold up a
    // collection that the thread making the names needs.
    static NAMES: Mutex<Option<[(usize, usize); 2]>> = Mutex::new(None);
    let mut names = unsafe { thread::lock_entered(me, &NAMES) };
    let made = names.get_or_insert_with(|| unsafe {
        let name = std::env::args_os().next().unwrap_or_default().into_vec();
        let short = name.rsplit(|&byte| byte == b'/').next().unwrap_or(&[]).to_vec();
        [name, short].map(|name| {
            let holder = holder(me);
            let copy = object::allocate(me, name.len() + 1, 0);
            globals::keep(copy);
            let copy = Ptr::for_object(copy);
            std::ptr::copy_nonoverlapping(name.as_ptr(), copy.ptr, name.len());
            ptrs::store(me, holder, copy);
            (holder.ptr as usize, holder.lower as usize)
        })
    });
    made.map(|(ptr, lower)| Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 })
}

unsafe extern "C" fn program_invocation_name(me: *mut Thread, _: *const Origin) -> Ptr {
    invocation_names(me)[0]
}

unsafe extern "C" fn program_invocation_short_name(me: *mut Thread, _: *const Origin) -> Ptr {
    invocation_names(me)[1]
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated_environ", environ as *const ()),
        ("pizlonated___environ", environ as *const ()),
        ("pizlonated__environ", environ as *const ()),
        ("pizlonated_program_invocation_name", program_invocation_name as *const ()),
        ("pizlonated_program_invocation_short_name", program_invocation_short_name as *const ()),
    ]
}
