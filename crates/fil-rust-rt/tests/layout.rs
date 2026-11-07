//! Checks the layouts in `abi.rs` against the headers of the runtime they were
//! taken from, by having a C compiler assert each one.

use std::fmt::Write;
use std::mem::{offset_of, size_of};
use std::process::Command;

use fil_rust_rt::abi::*;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set (run the tests in nix-shell)"))
}

#[test]
fn layouts_match_the_runtime_headers() {
    let values: &[(&str, usize)] = &[
        ("sizeof(filc_object)", OBJECT_SIZE),
        ("offsetof(filc_object, upper)", offset_of!(Object, upper)),
        ("offsetof(filc_object, aux)", offset_of!(Object, aux)),
        ("sizeof(filc_ptr)", size_of::<Ptr>()),
        ("sizeof(filc_atomic_box)", size_of::<AtomicBox>()),
        ("offsetof(filc_thread, stack_limit)", offset_of!(Thread, stack_limit)),
        ("offsetof(filc_thread, state)", offset_of!(Thread, state)),
        ("offsetof(filc_thread, tid)", offset_of!(Thread, tid)),
        ("offsetof(filc_thread, top_frame)", offset_of!(Thread, top_frame)),
        ("offsetof(filc_thread, unwind_registers)", offset_of!(Thread, unwind_registers)),
        ("offsetof(filc_thread, cookie_ptr)", offset_of!(Thread, cookie)),
        ("offsetof(filc_thread, cc_inline_buffer)", offset_of!(Thread, cc_inline)),
        ("offsetof(filc_thread, cc_inline_aux_buffer)", offset_of!(Thread, cc_inline_aux)),
        ("offsetof(filc_thread, cc_outline_buffer)", offset_of!(Thread, cc_outline)),
        ("offsetof(filc_thread, cc_outline_aux_buffer)", offset_of!(Thread, cc_outline_aux)),
        ("offsetof(filc_thread, cc_outline_size)", offset_of!(Thread, cc_outline_size)),
        ("FILC_THREAD_ALLOCATOR_OFFSET", ALLOCATOR_SLOTS_OFFSET),
        ("FILC_THREAD_ALLOCATOR_SIZE", ALLOCATOR_SLOT_SIZE),
        ("FILC_THREAD_NUM_ALLOCATORS", NUM_ALLOCATOR_SLOTS),
        ("FILC_CC_INLINE_SIZE", CC_INLINE_SIZE),
        ("FILC_CC_ALIGNMENT", CC_ALIGNMENT),
        ("FILC_WORD_SIZE", WORD_SIZE),
        ("FILC_MINALIGN", MIN_ALIGN),
        ("FILC_THREAD_STATE_ENTERED", STATE_ENTERED as usize),
        ("FILC_THREAD_STATE_CHECK_REQUESTED", STATE_CHECK_REQUESTED as usize),
        ("FILC_THREAD_STATE_STOP_REQUESTED", STATE_STOP_REQUESTED as usize),
        ("FILC_THREAD_STATE_DEFERRED_SIGNAL", STATE_DEFERRED_SIGNAL as usize),
        ("FILC_OBJECT_FLAG_GLOBAL", FLAG_GLOBAL),
        ("FILC_OBJECT_FLAG_READONLY", FLAG_READONLY),
        ("FILC_OBJECT_FLAG_FREE", FLAG_FREE),
        ("FILC_OBJECT_FLAG_MMAP", FLAG_MMAP),
        ("FILC_OBJECT_FLAG_GLOBAL_AUX", FLAG_GLOBAL_AUX),
        ("FILC_OBJECT_FLAGS_SPECIAL_SHIFT", FLAGS_SPECIAL_SHIFT as usize),
        ("FILC_OBJECT_FLAGS_ALIGN_SHIFT", FLAGS_ALIGN_SHIFT as usize),
        ("FILC_SPECIAL_TYPE_FUNCTION", SPECIAL_FUNCTION),
        ("FILC_SPECIAL_TYPE_THREAD", SPECIAL_THREAD),
        ("offsetof(filc_function, generic_entrypoint)", offset_of!(Function, generic_entrypoint)),
        ("sizeof(filc_function)", size_of::<Function>()),
        ("offsetof(filc_function_object, function)", offset_of!(FunctionObject, function)),
        ("offsetof(filc_origin_node, num_lowers_ish)", offset_of!(OriginNode, num_lowers_ish)),
        ("offsetof(filc_function_origin, personality_getter)", offset_of!(FunctionOrigin, personality_getter)),
        ("offsetof(filc_function_origin, num_setjmps)", offset_of!(FunctionOrigin, num_setjmps)),
        ("offsetof(filc_function_origin, num_stack_auxes)", offset_of!(FunctionOrigin, num_stack_auxes)),
        ("offsetof(filc_origin, line)", offset_of!(Origin, line)),
        ("offsetof(filc_inline_frame, origin)", offset_of!(InlineFrame, caller)),
        ("offsetof(filc_frame, parent)", offset_of!(Frame, parent)),
        ("offsetof(filc_frame, origin)", offset_of!(Frame, origin)),
        ("offsetof(filc_frame, lowers)", offset_of!(Frame, lowers)),
        ("offsetof(filc_stack_aux, lowers)", offset_of!(StackAux, lowers)),
    ];
    let mut source = String::from("#include <stddef.h>\n#include \"filc_runtime.h\"\n");
    for (expression, value) in values {
        writeln!(source, "_Static_assert({expression} == {value}, \"{expression}\");").unwrap();
    }
    let directory = env!("CARGO_TARGET_TMPDIR");
    let file = format!("{directory}/layout.c");
    std::fs::write(&file, source).unwrap();

    let clang = env("FILC_HOST_CLANG");
    let resource = Command::new(&clang).arg("-print-resource-dir").output().unwrap();
    let resource = String::from_utf8(resource.stdout).unwrap();
    let mut cmd = Command::new(&clang);
    if cfg!(target_arch = "x86_64") {
        cmd.arg("-march=x86-64-v2");
    }
    let output = cmd
        .args(["-fsyntax-only", "-nostdinc", "-pthread", "-DPAS_FILC=1"])
        .args(["-isystem", &env("FILC_MUSL_INCLUDE"), "-isystem", &env("FILC_LINUX_INCLUDE")])
        .arg("-isystem")
        .arg(format!("{}/include", resource.trim()))
        .args(["-I", &env("FILC_RUNTIME_SRC"), &file])
        .output()
        .expect("running clang");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
