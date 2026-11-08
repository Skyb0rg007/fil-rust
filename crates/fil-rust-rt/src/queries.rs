//! Natives that ask the runtime about itself, or tune parts of libpizlo this
//! runtime doesn't have.
//!
//! There's no scavenger and there are no runtime threads, so the natives for
//! those do nothing. The process title belongs to the host, so `zsetproctitle`
//! only checks its string. The heap dumps are summaries, not libpizlo's listing
//! of every object.

use std::ffi::c_int;
use std::fmt::Write;

use crate::abi::*;
use crate::failure::{QUIET, internal_panic, safety_panic, stack_report, write_stderr};
use crate::gc;
use crate::heap::heap;
use crate::natives::{natives, string_of};
use crate::object::{self, Access};
use crate::start::{c_string, set_errno};

const SPECIAL_NAMES: [&str; 13] = [
    "none", "function", "thread", "signal_handler", "ptr_table", "ptr_table_array", "dl_handle", "jmp_buf",
    "exact_ptr_table", "weak", "weak_map", "finalizer_queue", "fiber_context",
];

/// An object the way libpizlo's `filc_object_dump` writes it.
fn describe_object(text: &mut String, object: *mut Object) {
    if object.is_null() {
        text.push_str("<null>");
        return;
    }
    let object = unsafe { &*object };
    if object.is_special() {
        let _ = write!(text, "aux={:p}", object.aux_ptr());
    } else {
        let _ = write!(text, "{:p},{:p}", object.lower(), object.upper());
        if !object.aux_ptr().is_null() {
            let _ = write!(text, ",aux={:p}", object.aux_ptr());
        }
    }
    let flags = object.flags();
    let mut flag = |name: &str| {
        text.push(',');
        text.push_str(name);
    };
    if flags & FLAG_FREE != 0 {
        flag("free");
    }
    if object.is_special() {
        let special = object.special_type();
        flag(&match SPECIAL_NAMES.get(special) {
            Some(name) => format!("special({name})"),
            None => format!("special(?{special})"),
        });
    }
    for (bit, name) in [(FLAG_GLOBAL, "global"), (FLAG_GLOBAL_AUX, "global_aux"), (FLAG_MMAP, "mmap"), (FLAG_READONLY, "readonly"), (FLAG_WEAK_KEY, "weak_key")] {
        if flags & bit != 0 {
            flag(name);
        }
    }
    let log_align = flags >> FLAGS_ALIGN_SHIFT & 31;
    if log_align != 0 {
        flag(&format!("aligned({})", 1usize << log_align));
    }
}

fn describe(ptr: Ptr) -> String {
    let mut text = format!("{:p},", ptr.ptr);
    describe_object(&mut text, ptr.object());
    text
}

/// [`describe`], then each word of the object: its address, and its object if
/// it holds a pointer.
unsafe fn describe_contents(ptr: Ptr) -> String {
    let mut text = describe(ptr);
    text.push_str(":[");
    if !ptr.lower.is_null() {
        let upper = unsafe { (*ptr.object()).upper() } as usize;
        let aux = unsafe { (*ptr.object()).aux_ptr() };
        for (index, offset) in (0..upper.saturating_sub(ptr.lower as usize)).step_by(WORD_SIZE).enumerate() {
            if index != 0 {
                text.push(' ');
            }
            let word = unsafe { ptr.lower.add(offset).cast::<usize>().read_unaligned() } as *mut u8;
            let entry = if aux.is_null() { 0 } else { unsafe { object::aux_word(aux, offset) }.load(std::sync::atomic::Ordering::Relaxed) };
            let lower = unsafe { crate::ptrs::extract_lower(entry) };
            let _ = write!(text, "+{offset}:");
            if lower.is_null() {
                let _ = write!(text, "{word:p}");
            } else {
                text.push_str(&describe(Ptr { ptr: word, lower }));
            }
        }
    }
    text.push(']');
    text
}

/// The heap block `ptr`'s object is in, if it's in the heap. Its object is
/// tracked by the native's caller, so the block stays put.
fn block_of(ptr: Ptr) -> Option<(usize, usize)> {
    let object = ptr.object() as usize;
    (!ptr.lower.is_null() && heap().contains(object)).then(|| heap().block_of(object))
}

fn heap_summary() -> String {
    let stats = gc::stats();
    format!(
        "fil-rust-rt heap: {} collections, {} live bytes, {} tombstoned, {} reclaimed in the last collection, {} in all, {} allocated since, {} committed\n",
        stats.collections,
        stats.live_bytes,
        stats.tombstoned_bytes,
        stats.reclaimed_bytes,
        stats.total_reclaimed_bytes,
        stats.allocated_bytes,
        stats.committed_bytes,
    )
}

natives! {
    fn zset_errno(thread: *mut Thread, value: c_int) -> () {
        unsafe { set_errno(thread, value) };
    }

    fn zget_quiet_panic(thread: *mut Thread) -> bool {
        QUIET.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn zset_quiet_panic(thread: *mut Thread, value: bool) -> () {
        QUIET.store(value, std::sync::atomic::Ordering::Relaxed);
    }

    fn zprint_long(thread: *mut Thread, value: i64) -> () {
        write_stderr(value.to_string().as_bytes());
    }

    fn zprint_ptr(thread: *mut Thread, ptr: Ptr) -> () {
        write_stderr(describe(ptr).as_bytes());
    }

    fn zptr_to_new_string(thread: *mut Thread, ptr: Ptr) -> Ptr {
        unsafe { c_string(thread, describe(ptr).as_bytes()) }
    }

    fn zptr_contents_to_new_string(thread: *mut Thread, ptr: Ptr) -> Ptr {
        let text = unsafe { describe_contents(ptr) };
        unsafe { c_string(thread, text.as_bytes()) }
    }

    // Fails unless `ptr`'s object looks like one, as the collector's checks
    // under stress do.
    fn zvalidate_ptr(thread: *mut Thread, ptr: Ptr) -> () {
        if ptr.lower.is_null() {
            return;
        }
        match block_of(ptr) {
            Some((block, size)) => unsafe { gc::verify_object(ptr.object(), block, size) },
            None => {
                let object = unsafe { &*ptr.object() };
                if (object.upper() as usize) < (object.lower() as usize) {
                    internal_panic(format_args!("object {:p} has upper {:p} below its lower", ptr.object(), object.upper()));
                }
            }
        }
    }

    fn zgc_get_allocation_size(thread: *mut Thread, ptr: Ptr) -> usize {
        block_of(ptr).map_or(0, |(_, size)| size)
    }

    fn zgc_completed_cycle(thread: *mut Thread) -> u64 {
        gc::completed()
    }

    fn zgc_requested_cycle(thread: *mut Thread) -> u64 {
        gc::requested()
    }

    fn zgc_try_request(thread: *mut Thread) -> u64 {
        unsafe { gc::try_request(thread) }
    }

    fn zgc_is_stw(thread: *mut Thread) -> bool {
        true
    }

    fn zgc_is_scribbling(thread: *mut Thread) -> bool {
        gc::is_checking()
    }

    fn zgc_is_verifying(thread: *mut Thread) -> bool {
        gc::is_checking()
    }

    fn zgc_page_size(thread: *mut Thread) -> usize {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    fn zscavenge_synchronously(thread: *mut Thread) -> () {}
    fn zscavenger_suspend(thread: *mut Thread) -> () {}
    fn zscavenger_resume(thread: *mut Thread) -> () {}
    fn zset_scavenger_periods_to_1ms(thread: *mut Thread) -> () {}
    fn zlock_runtime_threads(thread: *mut Thread) -> () {}

    fn zsetproctitle(thread: *mut Thread, title: Ptr) -> () {
        unsafe { string_of(title) };
    }

    fn zdump_stack(thread: *mut Thread) -> () {
        write_stderr(unsafe { stack_report((*thread).top_frame) }.as_bytes());
    }

    fn zdump_stacks(thread: *mut Thread) -> () {
        unsafe {
            gc::with_world_stopped(thread, |threads| {
                let mut text = String::new();
                for &each in threads {
                    let _ = writeln!(text, "thread {}:", (*each).tid);
                    text.push_str(&stack_report((*each).top_frame));
                }
                write_stderr(text.as_bytes());
            })
        };
    }

    fn zdump_heap(thread: *mut Thread, fd: c_int) -> () {
        let text = heap_summary();
        unsafe { crate::sys::sys(thread, || libc::write(fd, text.as_ptr().cast(), text.len())) };
    }

    fn zdump_pas_status(thread: *mut Thread) -> () {
        write_stderr(heap_summary().as_bytes());
    }

    // Gives each word of `size` bytes at `dst` the capability of `object`'s
    // object, leaving the addresses alone.
    fn zsetcap(thread: *mut Thread, dst: Ptr, object: Ptr, size: usize) -> () {
        if !size.is_multiple_of(WORD_SIZE) {
            safety_panic(format_args!("size argument must be multiple of pointer size (size = {size})."));
        }
        object::check_access(dst, size, Access::Write);
        let remainder = dst.ptr as usize % WORD_SIZE;
        if remainder != 0 {
            safety_panic(format_args!(
                "alignment requirement of {WORD_SIZE} bytes not met; in this case ptr % {WORD_SIZE} = {remainder}."
            ));
        }
        if size == 0 {
            return;
        }
        let aux = unsafe { object::ensure_aux(thread, dst.object()) };
        for offset in (dst.offset()..dst.offset() + size).step_by(WORD_SIZE) {
            unsafe { object::aux_word(aux, offset) }.store(object.lower as usize, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
