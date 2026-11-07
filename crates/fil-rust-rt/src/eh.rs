//! Exceptions: the `_Unwind_*` functions compiled code and libc++abi use.
//!
//! Compiled code doesn't unwind with tables. A function that throws returns with
//! `has_exception` set, and every caller either passes that on or calls
//! `filc_landing_pad`, so phase two of the usual protocol happens one return at
//! a time. Phase one, the search, runs here by walking frames, whose origins
//! have the personality functions.

use std::ffi::c_int;
use std::ptr::null_mut;

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::natives::{Thrown, natives};
use crate::object::{self, Access};
use crate::ptrs;
use crate::thread::{NativeFrame, extra, track};

const REASON_NONE: c_int = 0;
const REASON_FATAL_PHASE1_ERROR: c_int = 3;
const REASON_END_OF_STACK: c_int = 5;
const REASON_HANDLER_FOUND: c_int = 6;
const REASON_INSTALL_CONTEXT: c_int = 7;
const REASON_CONTINUE_UNWIND: c_int = 8;

const ACTION_SEARCH_PHASE: c_int = 1;
const ACTION_CLEANUP_PHASE: c_int = 2;
const ACTION_HANDLER_FRAME: c_int = 4;
const ACTION_FORCE_UNWIND: c_int = 8;
const ACTION_END_OF_STACK: c_int = 16;

const EH_VERSION: usize = 1;

/// What `_Unwind_Context` is for Fil-C code.
#[repr(C)]
struct UnwindContext {
    language_specific_data: *mut u8,
    frame_address: usize,
    registers: [*mut u8; 2],
}

#[repr(C)]
struct UnwindException {
    exception_class: u64,
    exception_cleanup: *mut u8,
}

/// An exception on its way up a thread.
pub struct Unwinding {
    forced: bool,
    pub found_frame: *mut Frame,
    pub context: Ptr,
    pub exception: Ptr,
    pub stop_callback: Ptr,
    pub stop_argument: Ptr,
}

impl Unwinding {
    pub const fn new() -> Unwinding {
        Unwinding {
            forced: false,
            found_frame: null_mut(),
            context: Ptr::NULL,
            exception: Ptr::NULL,
            stop_callback: Ptr::NULL,
            stop_argument: Ptr::NULL,
        }
    }
}

/// Getter of a function object, as origins hold them.
type Getter = unsafe extern "C" fn(*mut Thread, *const Origin) -> Ptr;

fn new_context(thread: *mut Thread) -> Ptr {
    unsafe {
        let object = object::allocate(thread, size_of::<UnwindContext>(), 0);
        track(thread, object);
        Ptr::for_object(object)
    }
}

fn origin_of(frame: *mut Frame) -> &'static FunctionOrigin {
    unsafe { &*function_origin((*frame).origin) }
}

/// The function's own getter, if it has a personality.
fn personality_getter(origin: &FunctionOrigin) -> Option<Getter> {
    (!origin.personality_getter.is_null()).then(|| unsafe { std::mem::transmute(origin.personality_getter) })
}

/// The language-specific data of `frame`, which has a personality, for its
/// current call site.
unsafe fn eh_data(thread: *mut Thread, frame: *mut Frame) -> Ptr {
    unsafe {
        let getter = (*(*frame).origin.cast::<OriginWithEh>()).eh_data_getter;
        if getter.is_null() {
            return Ptr::NULL;
        }
        std::mem::transmute::<*const (), Getter>(getter)(thread, null_mut())
    }
}

/// Fills in the context for `frame`.
unsafe fn describe(thread: *mut Thread, context: Ptr, frame: *mut Frame, data: Ptr) {
    object::check_access(context, size_of::<UnwindContext>(), Access::Write);
    let fields = context.ptr.cast::<UnwindContext>();
    unsafe {
        ptrs::store(thread, context.with_ptr((&raw mut (*fields).language_specific_data).cast()), data);
        (*fields).frame_address = frame as usize;
    }
}

unsafe fn exception_class(exception: Ptr) -> u64 {
    object::check_access(exception, size_of::<UnwindException>(), Access::Read);
    unsafe { (*exception.ptr.cast::<UnwindException>()).exception_class }
}

unsafe fn call_personality(thread: *mut Thread, frame: *mut Frame, actions: c_int, exception: Ptr, context: Ptr) -> c_int {
    unsafe {
        let data = eh_data(thread, frame);
        describe(thread, context, frame, data);
        let class = exception_class(exception);
        let getter = personality_getter(origin_of(frame)).unwrap();
        let personality = getter(thread, null_mut());
        track(thread, personality.object());
        let args = [
            Ptr { ptr: EH_VERSION as *mut u8, lower: null_mut() },
            Ptr { ptr: actions as usize as *mut u8, lower: null_mut() },
            Ptr { ptr: class as *mut u8, lower: null_mut() },
            exception,
            context,
        ];
        let mut result = [Ptr::NULL];
        crate::call(thread, personality, &args, &mut result);
        result[0].ptr as usize as c_int
    }
}

/// Phase two of a forced unwind, from the frame above `frame`, as far as the
/// next frame with cleanups.
unsafe fn forced_unwind(thread: *mut Thread, context: Ptr, exception: Ptr, mut frame: *mut Frame) {
    unsafe {
        loop {
            frame = (*frame).parent;
            let origin = (!frame.is_null()).then(|| origin_of(frame));
            if let Some(origin) = origin
                && !origin.can_catch
            {
                let (file, function) = (origin.base.filename, origin.base.function);
                safety_panic(format_args!(
                    "encountered function {}: {} that cannot catch during forced unwind.",
                    std::ffi::CStr::from_ptr(file).to_string_lossy(),
                    std::ffi::CStr::from_ptr(function).to_string_lossy()
                ));
            }
            let personality = origin.and_then(personality_getter);
            let data = if personality.is_some() { eh_data(thread, frame) } else { Ptr::NULL };
            describe(thread, context, frame, data);
            let class = exception_class(exception);
            let mut actions = ACTION_FORCE_UNWIND | ACTION_CLEANUP_PHASE;
            if frame.is_null() {
                actions |= ACTION_END_OF_STACK;
            }
            let unwinding = &extra(thread).unwinding;
            let args = [
                Ptr { ptr: EH_VERSION as *mut u8, lower: null_mut() },
                Ptr { ptr: actions as usize as *mut u8, lower: null_mut() },
                Ptr { ptr: class as *mut u8, lower: null_mut() },
                exception,
                context,
                unwinding.stop_argument,
            ];
            let mut result = [Ptr::NULL];
            crate::call(thread, unwinding.stop_callback, &args, &mut result);
            let stopped = result[0].ptr as usize as c_int;
            if stopped != REASON_NONE {
                safety_panic(format_args!(
                    "stop function returned result {stopped} instead of unwind_reason_none during forced unwinding."
                ));
            }
            if frame.is_null() {
                safety_panic(format_args!("stop function returned even though we've reached the end of the stack."));
            }
            if personality.is_some() {
                match call_personality(thread, frame, actions, exception, context) {
                    REASON_INSTALL_CONTEXT => {
                        let unwinding = &mut extra(thread).unwinding;
                        unwinding.found_frame = frame;
                        unwinding.context = context;
                        unwinding.exception = exception;
                        return;
                    }
                    REASON_CONTINUE_UNWIND => {}
                    other => safety_panic(format_args!(
                        "personality function returned result {other} instead of either install_context or continue_unwind during forced unwinding."
                    )),
                }
            }
        }
    }
}

unsafe fn landing_pad_catches(thread: *mut Thread, frame: *mut Frame) -> bool {
    unsafe {
        let (context, exception, found) = {
            let unwinding = &extra(thread).unwinding;
            (unwinding.context, unwinding.exception, unwinding.found_frame)
        };
        let origin = origin_of(frame);
        if extra(thread).unwinding.forced {
            if frame != found {
                return false;
            }
            extra(thread).unwinding.found_frame = null_mut();
        } else {
            if origin.personality_getter.is_null() {
                return false;
            }
            let mut action = ACTION_CLEANUP_PHASE;
            if frame == found {
                action |= ACTION_HANDLER_FRAME;
            }
            match call_personality(thread, frame, action, exception, context) {
                REASON_CONTINUE_UNWIND => return false,
                REASON_INSTALL_CONTEXT => {}
                _ => safety_panic(format_args!("personality function returned neither continue_unwind nor install_context.")),
            }
        }
        object::check_access(context, size_of::<UnwindContext>(), Access::Read);
        let registers = &raw mut (*context.ptr.cast::<UnwindContext>()).registers;
        for index in 0..(*thread).unwind_registers.len() {
            let slot = registers.cast::<*mut u8>().add(index).cast::<u8>();
            (*thread).unwind_registers[index] = ptrs::load(context.with_ptr(slot));
        }
        true
    }
}

/// Whether the top frame's landing pad runs. If it does, nothing may reach a
/// safepoint until compiled code takes `unwind_registers`, which aren't roots.
pub unsafe fn landing_pad(thread: *mut Thread) -> bool {
    unsafe {
        let frame = (*thread).top_frame;
        let native = NativeFrame::push(thread);
        let caught = landing_pad_catches(thread, frame);
        if !caught {
            if frame == extra(thread).unwinding.found_frame {
                safety_panic(format_args!("personality function told us to continue phase2 unwinding past the frame found in phase1."));
            }
            if !origin_of(frame).can_throw && !extra(thread).unwinding.forced {
                safety_panic(format_args!("cannot unwind from landing pad, function claims not to throw and we aren't force unwinding."));
            }
            if (*frame).parent.is_null() {
                safety_panic(format_args!("cannot unwind from landing pad, reached end of stack."));
            }
            if !origin_of((*frame).parent).can_catch {
                safety_panic(format_args!("cannot unwind from landing pad, parent frame doesn't support catching."));
            }
        }
        native.pop(thread);
        caught
    }
}

pub unsafe fn resume_unwind(thread: *mut Thread, origin: *const Origin) {
    unsafe {
        let frame = (*thread).top_frame;
        if !origin.is_null() {
            (*frame).origin = origin;
        }
        let forced = extra(thread).unwinding.forced;
        if !origin_of(frame).can_throw && !forced {
            safety_panic(format_args!("cannot resume unwinding, current frame claims not to throw and we aren't force unwinding."));
        }
        if (*frame).parent.is_null() {
            safety_panic(format_args!("cannot resume unwinding, reached end of stack."));
        }
        if !origin_of((*frame).parent).can_catch {
            safety_panic(format_args!("cannot resume unwinding, parent frame doesn't support catching."));
        }
        let found = extra(thread).unwinding.found_frame;
        if frame == found {
            safety_panic(format_args!("cannot resume unwinding from frame that was found in search phase."));
        }
        if !forced {
            if found.is_null() {
                internal_panic(format_args!("resumed an exception no frame was found for"));
            }
            return;
        }
        if !found.is_null() {
            return;
        }
        // The landing pad consumed the frame, so on to the next one.
        let native = NativeFrame::push(thread);
        let (context, exception) = (extra(thread).unwinding.context, extra(thread).unwinding.exception);
        forced_unwind(thread, context, exception, frame);
        native.pop(thread);
    }
}

/// Ends an exception that reached a Rust caller, as a foreign catch would.
pub unsafe fn dispose(thread: *mut Thread) {
    const FOREIGN_EXCEPTION_CAUGHT: usize = 1;
    unsafe {
        (*thread).unwind_registers = [Ptr::NULL; 2];
        let unwinding = &mut extra(thread).unwinding;
        let exception = unwinding.exception;
        unwinding.exception = Ptr::NULL;
        unwinding.context = Ptr::NULL;
        unwinding.found_frame = null_mut();
        let native = NativeFrame::push(thread);
        track(thread, exception.object());
        if object::is_accessible(exception, size_of::<UnwindException>(), Access::Read) {
            let cleanup = (&raw mut (*exception.ptr.cast::<UnwindException>()).exception_cleanup).cast::<u8>();
            let cleanup = ptrs::load(exception.with_ptr(cleanup));
            if object::is_function(cleanup) {
                let code = Ptr { ptr: FOREIGN_EXCEPTION_CAUGHT as *mut u8, lower: null_mut() };
                crate::call(thread, cleanup, &[code, exception], &mut []);
            }
        }
        native.pop(thread);
    }
}

natives! {
    fn _Unwind_RaiseException(thread: *mut Thread, exception: Ptr) -> Thrown<c_int> {
        unsafe {
            let context = new_context(thread);
            extra(thread).unwinding.forced = false;
            let mut frame = (*thread).top_frame;
            while !frame.is_null() {
                let origin = origin_of(frame);
                if !origin.can_catch {
                    return Thrown::Value(REASON_FATAL_PHASE1_ERROR);
                }
                if origin.personality_getter.is_null() {
                    if !origin.can_throw {
                        return Thrown::Value(REASON_FATAL_PHASE1_ERROR);
                    }
                } else {
                    let result = call_personality(thread, frame, ACTION_SEARCH_PHASE, exception, context);
                    if result == REASON_HANDLER_FOUND {
                        let unwinding = &mut extra(thread).unwinding;
                        unwinding.found_frame = frame;
                        unwinding.context = context;
                        unwinding.exception = exception;
                        return Thrown::Exception;
                    }
                    if result != REASON_CONTINUE_UNWIND || !origin.can_throw {
                        return Thrown::Value(REASON_FATAL_PHASE1_ERROR);
                    }
                }
                frame = (*frame).parent;
            }
            Thrown::Value(REASON_END_OF_STACK)
        }
    }

    fn _Unwind_ForcedUnwind(thread: *mut Thread, exception: Ptr, stop: Ptr, argument: Ptr) -> Thrown<c_int> {
        unsafe {
            let context = new_context(thread);
            let frame = (*thread).top_frame;
            object::check_function_call(stop);
            let unwinding = &mut extra(thread).unwinding;
            unwinding.forced = true;
            unwinding.stop_callback = stop;
            unwinding.stop_argument = argument;
            // The walk starts at the caller of this frame.
            let mut base = Frame { parent: frame, origin: (*frame).origin, lowers: [] };
            forced_unwind(thread, context, exception, &mut base);
            Thrown::Exception
        }
    }
}
