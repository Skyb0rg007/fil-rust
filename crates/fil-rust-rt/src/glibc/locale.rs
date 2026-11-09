//! `<locale.h>`, `<langinfo.h>` and `<nl_types.h>`, with only the C locale.
//! `newlocale` hands out one read-only `__locale_struct`, in glibc's layout,
//! whose ctype tables are the ones `__ctype_b_loc` points into, so the
//! headers' inline `isalpha_l` and the like work. `""` and `"POSIX"` name it
//! too; other names fail as glibc fails for a locale that isn't installed.
//! The `*_l` functions are the plain ones, which ignore the locale.

use std::ffi::{CStr, c_int};
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::natives;
use crate::glibc::set_errno;
use crate::{globals, object, ptrs, thread, view};

/// `sizeof(struct __locale_struct)`: 13 locale data pointers, the three
/// ctype tables, and 13 category names.
const LOCALE_SIZE: usize = 232;
const CTYPE_B: usize = 104;
const NAMES: usize = 128;
const CATEGORIES: usize = 13;

/// `struct lconv`: ten strings, then fourteen `char`s.
const LCONV_SIZE: usize = 96;

/// `LC_GLOBAL_LOCALE`, `(locale_t) -1`.
fn global() -> Ptr {
    Ptr { ptr: usize::MAX as *mut u8, lower: std::ptr::null_mut() }
}

/// An object made once and kept, by `make`.
fn kept(me: *mut Thread, cell: &'static Mutex<Option<(usize, usize)>>, make: impl FnOnce() -> Ptr) -> Ptr {
    let mut made = unsafe { thread::lock_entered(me, cell) };
    let (ptr, lower) = *made.get_or_insert_with(|| {
        let ptr = make();
        (ptr.ptr as usize, ptr.lower as usize)
    });
    Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 }
}

/// The C locale's object.
fn c_locale(me: *mut Thread) -> Ptr {
    static LOCALE: Mutex<Option<(usize, usize)>> = Mutex::new(None);
    kept(me, &LOCALE, || unsafe {
        let object = object::allocate(me, LOCALE_SIZE, 0);
        thread::track(me, object);
        let locale = Ptr::for_object(object);
        let at = |offset: usize| locale.with_ptr(locale.ptr.add(offset));
        for (index, holder) in super::ctype::tables(me).into_iter().enumerate() {
            ptrs::store(me, at(CTYPE_B + index * WORD_SIZE), ptrs::load(holder));
        }
        let name = crate::glibc::interned(me, b"C");
        for category in 0..CATEGORIES {
            ptrs::store(me, at(NAMES + category * WORD_SIZE), name);
        }
        object::make_readonly(object);
        globals::keep(object);
        locale
    })
}

/// Whether `name` is a name of the C locale.
fn is_c(name: &[u8]) -> bool {
    matches!(name, b"" | b"C" | b"POSIX")
}

natives! {
    fn newlocale(thread: *mut Thread, mask: c_int, name: Ptr, base: Ptr) -> Ptr {
        if name.ptr.is_null() || mask & !((1 << CATEGORIES) - 1) != 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return Ptr::NULL;
        }
        if !is_c(&view::c_string(name)) {
            unsafe { set_errno(thread, libc::ENOENT) };
            return Ptr::NULL;
        }
        c_locale(thread)
    }

    fn duplocale(thread: *mut Thread, locale: Ptr) -> Ptr {
        c_locale(thread)
    }

    fn freelocale(thread: *mut Thread, locale: Ptr) -> () {}

    // The thread's locale, null for the global one.
    fn uselocale(thread: *mut Thread, locale: Ptr) -> Ptr {
        let state = &mut unsafe { thread::extra(thread) }.libc;
        let previous = if state.locale.ptr.is_null() { global() } else { state.locale };
        if !locale.ptr.is_null() {
            state.locale = if locale.ptr == global().ptr { Ptr::NULL } else { locale };
        }
        previous
    }

    fn setlocale(thread: *mut Thread, category: c_int, name: Ptr) -> Ptr {
        if !(0..CATEGORIES as c_int).contains(&category) {
            unsafe { set_errno(thread, libc::EINVAL) };
            return Ptr::NULL;
        }
        if !name.ptr.is_null() && !is_c(&view::c_string(name)) {
            return Ptr::NULL;
        }
        unsafe { crate::glibc::interned(thread, b"C") }
    }

    fn localeconv(thread: *mut Thread) -> Ptr {
        static LCONV: Mutex<Option<(usize, usize)>> = Mutex::new(None);
        kept(thread, &LCONV, || unsafe {
            let object = object::allocate(thread, LCONV_SIZE, 0);
            thread::track(thread, object);
            let lconv = Ptr::for_object(object);
            let (empty, point) = (crate::glibc::interned(thread, b""), crate::glibc::interned(thread, b"."));
            for index in 0..10 {
                ptrs::store(thread, lconv.with_ptr(lconv.ptr.add(index * WORD_SIZE)), if index == 0 { point } else { empty });
            }
            view::write(lconv.with_ptr(lconv.ptr.add(10 * WORD_SIZE)), &[libc::c_char::MAX as u8; 14]);
            object::make_readonly(object);
            globals::keep(object);
            lconv
        })
    }

    // The host's answers, which are the C locale's, since nothing in the
    // process sets another.
    fn nl_langinfo(thread: *mut Thread, item: c_int) -> Ptr {
        let text = unsafe { CStr::from_ptr(libc::nl_langinfo(item)) }.to_bytes().to_vec();
        unsafe { crate::glibc::interned(thread, &text) }
    }

    // No message catalogs: `catgets` always gives back its default.
    fn catopen(thread: *mut Thread, name: Ptr, flag: c_int) -> Ptr {
        let _ = view::c_string(name);
        unsafe { set_errno(thread, libc::ENOENT) };
        global()
    }

    fn catgets(thread: *mut Thread, catalog: Ptr, set: c_int, number: c_int, default: Ptr) -> Ptr {
        unsafe { set_errno(thread, libc::EBADF) };
        default
    }

    fn catclose(thread: *mut Thread, catalog: Ptr) -> c_int {
        unsafe { set_errno(thread, libc::EBADF) };
        -1
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated___newlocale", newlocale::getter as *const ()),
        ("pizlonated___duplocale", duplocale::getter as *const ()),
        ("pizlonated___freelocale", freelocale::getter as *const ()),
        ("pizlonated___uselocale", uselocale::getter as *const ()),
        ("pizlonated_nl_langinfo_l", nl_langinfo::getter as *const ()),
        ("pizlonated___nl_langinfo_l", nl_langinfo::getter as *const ()),
    ]
}
