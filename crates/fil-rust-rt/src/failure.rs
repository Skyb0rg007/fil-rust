//! Reporting failures. All of them are fatal.
//!
//! This doesn't use Rust panics: compiled frames can't be unwound through, and
//! a panic hook could wander off into code that's only safe at a safepoint. The
//! report goes straight to stderr, in the shape libpizlo gives it, and the
//! process aborts.

use std::ffi::{CStr, c_char};
use std::fmt::{self, Write};

use crate::abi::{Frame, InlineFrame, Origin, OriginNode};

fn text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
}

pub fn write_stderr(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
        if written <= 0 {
            return;
        }
        bytes = &bytes[written as usize..];
    }
}

/// Follows an origin through its inline frames to `report`.
unsafe fn dump_origin(report: &mut String, mut origin: *const Origin) {
    while !origin.is_null() {
        let node: *const OriginNode = unsafe { (*origin).origin_node };
        if node.is_null() {
            return;
        }
        let (function, filename) = unsafe { (text((*node).function), text((*node).filename)) };
        let (line, column) = unsafe { ((*origin).line, (*origin).column) };
        let _ = writeln!(report, "    {filename}:{line}:{column}: {function}");
        if unsafe { (*node).num_lowers_ish } != u32::MAX {
            return;
        }
        origin = unsafe { &raw const (*node.cast::<InlineFrame>()).caller };
    }
}

unsafe fn dump_stack(report: &mut String, mut frame: *mut Frame) {
    report.push_str("stack:\n");
    let mut depth = 0;
    while !frame.is_null() && depth < 64 {
        unsafe { dump_origin(report, (*frame).origin) };
        frame = unsafe { (*frame).parent };
        depth += 1;
    }
}

fn finish(mut report: String, kind: &str) -> ! {
    let thread = crate::thread::current();
    if !thread.is_null() {
        unsafe { dump_stack(&mut report, (*thread).top_frame) };
    }
    let _ = writeln!(report, "filc panic: {kind}");
    write_stderr(report.as_bytes());
    unsafe { libc::abort() }
}

/// Fil-C code tried to break memory safety.
#[cold]
pub fn safety_panic(args: fmt::Arguments<'_>) -> ! {
    finish(format!("filc safety error: {args}\n"), "thwarted a futile attempt to violate memory safety.")
}

/// Fil-C code asked to stop, with `zerror`.
#[cold]
pub fn user_panic(args: fmt::Arguments<'_>) -> ! {
    finish(format!("filc user error: {args}\n"), "user thwarted themselves.")
}

/// The runtime can't go on. Nothing Fil-C code does should lead here.
#[cold]
pub fn internal_panic(args: fmt::Arguments<'_>) -> ! {
    finish(format!("fil-rust-rt internal error: {args}\n"), "the runtime cannot continue.")
}

/// A runtime function that isn't implemented yet was called.
#[cold]
pub fn missing(name: &str) -> ! {
    finish(
        format!("fil-rust-rt: {name} is not implemented by this runtime\n"),
        "the runtime cannot continue.",
    )
}
