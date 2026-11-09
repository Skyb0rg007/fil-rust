//! The parts of the libc that call back into Fil-C: exit handlers, sorting
//! and searching. An exception a callback throws goes on through the native;
//! a `longjmp` out of one drops the native's frame like any other.

use std::ffi::c_int;
use std::sync::Mutex;

use crate::abi::*;
use crate::natives::{Thrown, natives};
use crate::object::{self, Access};
use crate::{cc, memops, thread};

/// Calls `callee` with `args`, giving back its first returned word, or
/// `None` if it threw.
pub(crate) unsafe fn call_back(me: *mut Thread, callee: Ptr, args: &[Ptr]) -> Option<Ptr> {
    object::check_function_call(callee);
    let size = args.len() * WORD_SIZE;
    unsafe {
        cc::ensure_total(me, size);
        for (index, arg) in args.iter().enumerate() {
            *cc::slot(me, index * WORD_SIZE) = arg.ptr as u64;
            *cc::aux_slot(me, index * WORD_SIZE) = arg.lower as u64;
        }
        let function = &*callee.ptr.cast::<Function>();
        let result = (function.generic_entrypoint)(me, callee.lower, size);
        if result.has_exception {
            return None;
        }
        if result.return_size == 0 {
            return Some(Ptr::NULL);
        }
        Some(Ptr { ptr: *cc::slot(me, 0) as *mut u8, lower: *cc::aux_slot(me, 0) as *mut u8 })
    }
}

fn int(value: usize) -> Ptr {
    Ptr { ptr: value as *mut u8, lower: std::ptr::null_mut() }
}

/// What an exit handler is called with.
#[derive(Clone, Copy)]
enum Kind {
    /// `atexit`: nothing.
    Plain,
    /// `__cxa_atexit`: its argument, and run by `__cxa_finalize` for its DSO.
    Cxa { dso: usize },
    /// `on_exit`: the exit status and its argument.
    OnExit,
}

#[derive(Clone, Copy)]
struct Handler {
    function: (usize, usize),
    argument: (usize, usize),
    kind: Kind,
}

impl Handler {
    fn new(function: Ptr, argument: Ptr, kind: Kind) -> Handler {
        Handler { function: (function.ptr as usize, function.lower as usize), argument: (argument.ptr as usize, argument.lower as usize), kind }
    }
}

fn ptr((ptr, lower): (usize, usize)) -> Ptr {
    Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 }
}

/// Registered handlers, oldest first. Roots; locked only while entered and
/// never across a call or an allocation.
static AT_EXIT: Mutex<Vec<Handler>> = Mutex::new(Vec::new());
static AT_QUICK_EXIT: Mutex<Vec<Handler>> = Mutex::new(Vec::new());

pub fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    for list in [&AT_EXIT, &AT_QUICK_EXIT] {
        for handler in list.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            visit(object_for_lower(handler.function.1 as *mut u8));
            if handler.argument.1 != 0 {
                visit(object_for_lower(handler.argument.1 as *mut u8));
            }
        }
    }
}

fn register(list: &Mutex<Vec<Handler>>, handler: Handler) -> c_int {
    object::check_function_call(ptr(handler.function));
    list.lock().unwrap_or_else(|e| e.into_inner()).push(handler);
    static HOOKED: std::sync::Once = std::sync::Once::new();
    HOOKED.call_once(|| unsafe {
        libc::atexit(at_host_exit);
    });
    0
}

/// A process that exits some other way than Fil-C's `exit`, as by returning
/// from Rust's `main`, runs what's left of the handlers here.
extern "C" fn at_host_exit() {
    crate::run(|me| unsafe { run_handlers(me, &AT_EXIT, 0, |_| true) });
}

unsafe fn run(me: *mut Thread, handler: Handler, status: c_int) {
    let (function, argument) = (ptr(handler.function), ptr(handler.argument));
    let args = match handler.kind {
        Kind::Plain => vec![],
        Kind::Cxa { .. } => vec![argument],
        Kind::OnExit => vec![int(status as usize), argument],
    };
    if unsafe { call_back(me, function, &args) }.is_none() {
        crate::failure::safety_panic(format_args!("an exception escaped an exit handler."));
    }
}

/// Runs the handlers in `list` that `which` picks, newest first, one at a
/// time, so that one may register more.
unsafe fn run_handlers(me: *mut Thread, list: &Mutex<Vec<Handler>>, status: c_int, which: impl Fn(&Handler) -> bool) {
    loop {
        let next = {
            let mut handlers = list.lock().unwrap_or_else(|e| e.into_inner());
            match handlers.iter().rposition(&which) {
                Some(index) => handlers.remove(index),
                None => return,
            }
        };
        unsafe { run(me, next, status) };
    }
}

/// Swaps the `size`-byte elements at `a` and `b` through `spare`, keeping the
/// capabilities of the pointers in them.
unsafe fn swap(me: *mut Thread, a: Ptr, b: Ptr, spare: Ptr, size: usize) {
    unsafe {
        memops::move_memory(me, spare, a, size);
        memops::move_memory(me, a, b, size);
        memops::move_memory(me, b, spare, size);
    }
}

/// Sorts `count` elements of `size` bytes at `base` in place, by heapsort:
/// the comparison only ever sees elements of the array.
unsafe fn sort(me: *mut Thread, base: Ptr, count: usize, size: usize, compare: impl Fn(Ptr, Ptr) -> Option<c_int>) -> Option<()> {
    if count < 2 || size == 0 {
        return Some(());
    }
    let total = count.checked_mul(size).unwrap_or_else(|| crate::failure::safety_panic(format_args!("qsort: array size overflows.")));
    object::check_access(base, total, Access::Write);
    let spare = unsafe { object::allocate(me, size, 0) };
    unsafe { thread::track(me, spare) };
    let spare = Ptr::for_object(spare);
    let at = |index: usize| base.with_ptr(base.ptr.wrapping_add(index * size));
    let sift = |mut root: usize, end: usize| -> Option<()> {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return Some(());
            }
            if child + 1 < end && compare(at(child), at(child + 1))? < 0 {
                child += 1;
            }
            if compare(at(root), at(child))? >= 0 {
                return Some(());
            }
            unsafe { swap(me, at(root), at(child), spare, size) };
            root = child;
        }
    };
    for root in (0..count / 2).rev() {
        sift(root, count)?;
    }
    for end in (1..count).rev() {
        unsafe { swap(me, at(0), at(end), spare, size) };
        sift(0, end)?;
    }
    Some(())
}

fn comparison(result: Ptr) -> c_int {
    result.ptr as usize as u32 as c_int
}

natives! {
    fn atexit(thread: *mut Thread, function: Ptr) -> c_int {
        register(&AT_EXIT, Handler::new(function, Ptr::NULL, Kind::Plain))
    }

    fn __cxa_atexit(thread: *mut Thread, function: Ptr, argument: Ptr, dso: Ptr) -> c_int {
        register(&AT_EXIT, Handler::new(function, argument, Kind::Cxa { dso: dso.ptr as usize }))
    }

    fn on_exit(thread: *mut Thread, function: Ptr, argument: Ptr) -> c_int {
        register(&AT_EXIT, Handler::new(function, argument, Kind::OnExit))
    }

    fn at_quick_exit(thread: *mut Thread, function: Ptr) -> c_int {
        register(&AT_QUICK_EXIT, Handler::new(function, Ptr::NULL, Kind::Plain))
    }

    fn __cxa_at_quick_exit(thread: *mut Thread, function: Ptr, dso: Ptr) -> c_int {
        register(&AT_QUICK_EXIT, Handler::new(function, Ptr::NULL, Kind::Plain))
    }

    // A library's destructors, from its `.fini_array`, or all of them for a
    // null DSO.
    fn __cxa_finalize(thread: *mut Thread, dso: Ptr) -> () {
        let dso = dso.ptr as usize;
        unsafe {
            run_handlers(thread, &AT_EXIT, 0, |handler| dso == 0 || matches!(handler.kind, Kind::Cxa { dso: theirs } if theirs == dso))
        };
    }

    // Fil-C's handlers, then what the process has to flush, then the host's
    // `exit`, which runs Rust's and the host's handlers and the libraries'
    // destructors.
    fn exit(thread: *mut Thread, status: c_int) -> () {
        unsafe {
            run_handlers(thread, &AT_EXIT, status, |_| true);
            crate::glibc::flush_all(thread);
            thread::exit(thread);
            libc::exit(status)
        }
    }

    fn quick_exit(thread: *mut Thread, status: c_int) -> () {
        unsafe {
            run_handlers(thread, &AT_QUICK_EXIT, status, |_| true);
            crate::glibc::flush_all(thread);
            thread::exit(thread);
            libc::_exit(status)
        }
    }

    fn qsort(thread: *mut Thread, base: Ptr, count: usize, size: usize, compare: Ptr) -> Thrown<()> {
        let sorted = unsafe { sort(thread, base, count, size, |a, b| call_back(thread, compare, &[a, b]).map(comparison)) };
        match sorted {
            Some(()) => Thrown::Value(()),
            None => Thrown::Exception,
        }
    }

    fn qsort_r(thread: *mut Thread, base: Ptr, count: usize, size: usize, compare: Ptr, argument: Ptr) -> Thrown<()> {
        let sorted = unsafe { sort(thread, base, count, size, |a, b| call_back(thread, compare, &[a, b, argument]).map(comparison)) };
        match sorted {
            Some(()) => Thrown::Value(()),
            None => Thrown::Exception,
        }
    }

    fn bsearch(thread: *mut Thread, key: Ptr, base: Ptr, count: usize, size: usize, compare: Ptr) -> Thrown<Ptr> {
        let (mut low, mut high) = (0usize, count);
        while low < high {
            let middle = low + (high - low) / 2;
            let element = base.with_ptr(base.ptr.wrapping_add(middle * size));
            object::check_access(element, size.max(1), Access::Read);
            let Some(result) = (unsafe { call_back(thread, compare, &[key, element]) }) else { return Thrown::Exception };
            match comparison(result) {
                0 => return Thrown::Value(element),
                order if order < 0 => high = middle,
                _ => low = middle + 1,
            }
        }
        Thrown::Value(Ptr::NULL)
    }
}
