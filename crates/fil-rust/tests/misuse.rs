//! Misuse at run time: wrong files, wrong symbols, wrong types, and bad
//! pointers from either side. Each is an `Err` (or, for the Fil-C side of a
//! mistake, a Fil-C panic; see process.rs), never undefined behavior.

mod common;

use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{fixture, flavor, runtime};
use fil_rust::{Callback, Error, FilFn, FilHandle, FilPtr, FilType, Flavor, Function, Library, Ptr, Runtime};

#[derive(FilType, Clone, Copy, Debug)]
#[fil(size = 16)]
struct Holder<'f> {
    p: FilPtr<'f, u8>,
    n: i64,
}

fil_rust::foreign! {
    struct Misuse {
        fn twice(x: i32) -> i32;
        fn call_it(f: fn(i32) -> i32, x: i32) -> i32;
        fn free_it(p: Ptr);
        fn alloc_n(n: usize) -> Ptr<u8>;
        fn int_to_ptr(x: usize) -> Ptr<u8>;
        fn int_to_fn(x: usize) -> fn(i32) -> i32;
        fn holder_launder(h: Ptr<Holder<'static>>);
        fn past_the_end() -> Ptr<i32>;
        fn freed_string() -> Ptr<c_char>;
        fn unterminated() -> Ptr<c_char>;
        static small_global: i32;
        static const_global: i32;
        static table: [i32; 4];
    }
}

fn misuse() -> &'static Misuse {
    static M: std::sync::OnceLock<Misuse> = std::sync::OnceLock::new();
    M.get_or_init(|| runtime().run(|fil| Misuse::load(fil, fixture("misuse.c")).unwrap()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("misuse");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn load_err(path: &Path) -> String {
    match runtime().load(path) {
        Err(Error::Load(msg)) => msg,
        other => panic!("loading {} should fail with Error::Load, got {other:?}", path.display()),
    }
}

#[test]
fn files_that_are_not_libraries() {
    assert!(load_err(&scratch("missing.so")).contains("does not exist"));
    assert!(load_err(&std::env::temp_dir()).contains("does not exist"));

    let empty = scratch("empty.so");
    std::fs::write(&empty, b"").unwrap();
    assert!(load_err(&empty).contains("not an ELF"));

    let text = scratch("text.so");
    std::fs::write(&text, b"#!/bin/sh\necho hi\n").unwrap();
    assert!(load_err(&text).contains("not an ELF"));

    // The start of a real Fil-C library: an ELF header and nothing else.
    let real = std::fs::read(fixture("misuse.c")).unwrap();
    let truncated = scratch("truncated.so");
    std::fs::write(&truncated, &real[..200]).unwrap();
    load_err(&truncated);
}

/// A native library whose constructor leaves a marker file behind.
fn native_library() -> (PathBuf, PathBuf) {
    let src = scratch("native.c");
    let lib = scratch("libnative.so");
    let marker = scratch("native-constructor-ran");
    let _ = std::fs::remove_file(&marker);
    std::fs::write(
        &src,
        format!(
            "#include <stdio.h>\n\
             __attribute__((constructor)) static void ran(void) {{ fclose(fopen(\"{}\", \"w\")); }}\n\
             int pizlonated_twice(void) {{ return 0; }}\n",
            marker.display()
        ),
    )
    .unwrap();
    let cc = ["FILC_TARGET_CC", "CC"].into_iter().find_map(|v| std::env::var(v).ok()).unwrap_or_else(|| "cc".into());
    let status = Command::new(cc).args(["-shared", "-fPIC", "-o"]).arg(&lib).arg(&src).status().unwrap();
    assert!(status.success(), "building the native library");
    (lib, marker)
}

#[test]
fn native_libraries_are_refused_before_they_run() {
    let (lib, marker) = native_library();
    let msg = load_err(&lib);
    assert!(msg.contains("not a Fil-C library"), "{msg}");
    assert!(!marker.exists(), "the native constructor ran");
}

#[test]
fn libraries_of_the_other_flavor_are_refused() {
    let (other, var) = match flavor() {
        Flavor::Musl => (Flavor::Glibc, "FILC_GLIBC_CC"),
        Flavor::Glibc => (Flavor::Musl, "FILC_CC"),
    };
    let Ok(cc) = std::env::var(var) else {
        eprintln!("skipping: {var} isn't set, so there's no {other:?} compiler");
        return;
    };
    let lib = scratch(&format!("libother-{other:?}.so"));
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/misuse.c");
    let status = Command::new(cc).args(["-shared", "-fPIC", "-o"]).arg(&lib).arg(&src).status().unwrap();
    assert!(status.success(), "compiling with the {other:?} compiler");
    let msg = load_err(&lib);
    assert!(msg.contains(&format!("built for the {other:?} flavor")), "{msg}");
}

#[test]
fn symbols_must_exist_and_be_what_they_are_claimed_to_be() {
    let m = misuse();
    runtime().run(|fil| {
        let lib = m.library();
        assert!(lib.has_symbol("twice"));
        assert!(!lib.has_symbol("thrice"));
        assert!(!lib.has_symbol("tw\0ice"));
        assert_eq!(lib.function::<fn(i32) -> i32>(fil, "thrice").unwrap_err(), Error::MissingSymbol("thrice".into()));
        assert_eq!(lib.symbol(fil, "tw\0ice").unwrap_err(), Error::MissingSymbol("tw\0ice".into()));
        // Data isn't a function, and a function isn't data.
        assert_eq!(lib.function::<fn() -> i32>(fil, "small_global").unwrap_err(), Error::NotAFunction);
        assert_eq!(lib.global::<i32>(fil, "twice").unwrap_err(), Error::SpecialObject);
        // Symbols of the Fil-C libc and runtime aren't reachable through a
        // library that doesn't define them... unless it depends on them, as
        // every Fil-C library depends on the libc.
        assert!(lib.has_symbol("malloc"));
    });
}

#[test]
fn foreign_reports_missing_symbols_at_load() {
    fil_rust::foreign! {
        struct Wrong {
            fn twice(x: i32) -> i32;
            fn not_there(x: i32) -> i32;
        }
    }
    fil_rust::foreign! {
        struct WrongGlobal {
            static twice: i32;
        }
    }
    let m = misuse();
    runtime().run(|fil| {
        let lib = m.library().clone();
        assert_eq!(Wrong::from_library(fil, lib.clone()).unwrap_err(), Error::MissingSymbol("not_there".into()));
        assert_eq!(WrongGlobal::from_library(fil, lib).unwrap_err(), Error::SpecialObject);
    });
}

#[test]
fn globals_declared_with_the_wrong_type_are_still_bounded() {
    let m = misuse();
    runtime().run(|fil| {
        let lib = m.library();
        // Claims small_global is a 64-element array.
        let big = lib.global::<[i64; 64]>(fil, "small_global").unwrap();
        assert_eq!(big.read(fil).unwrap_err(), Error::OutOfBounds);
        assert_eq!(big.write(fil, &[0; 64]).unwrap_err(), Error::OutOfBounds);
        assert_eq!(m.small_global(fil).read(fil).unwrap(), 7, "a failed write changes nothing");
        // Indexing past a global array.
        assert_eq!(m.table(fil).read(fil).unwrap(), [1, 2, 3, 4]);
        let table = m.table(fil).cast::<i32>();
        assert_eq!(table.read_at(fil, 4).unwrap_err(), Error::OutOfBounds);
        // Constants are read-only.
        assert_eq!(m.const_global(fil).write(fil, &4).unwrap_err(), Error::ReadOnly);
        assert_eq!(m.const_global(fil).read(fil).unwrap(), 3);
    });
}

#[test]
fn function_pointers_must_be_real_functions() {
    let m = misuse();
    runtime().run(|fil| {
        // Data is not callable.
        let data = fil.alloc_bytes(16).unwrap();
        assert_eq!(FilFn::<fn() -> i32>::from_ptr(data.cast()).unwrap_err(), Error::NotAFunction);
        assert_eq!(data.as_fn::<fn() -> i32>().unwrap_err(), Error::NotAFunction);
        // Neither is null, nor an address without a capability, even the
        // address of a real function.
        assert!(FilFn::<fn() -> i32>::from_ptr(FilPtr::null()).is_err());
        let real = m.library().symbol(fil, "twice").unwrap();
        let forged = FilPtr::<fil_rust::Opaque>::from_addr(real.addr());
        assert!(FilFn::<fn(i32) -> i32>::from_ptr(forged).is_err());
        assert!(FilFn::<fn(i32) -> i32>::null().call(fil, (1,)).is_err());
        assert!(FilFn::<fn(i32) -> i32>::null().is_null());
        // A function pointer that C made from an integer is refused too.
        let from_int = m.int_to_fn(fil, real.addr()).unwrap();
        assert!(from_int.call(fil, (1,)).is_err());
        // The genuine one works.
        let ok = FilFn::<fn(i32) -> i32>::from_ptr(real).unwrap();
        assert_eq!(ok.call(fil, (21,)).unwrap(), 42);
    });
}

#[test]
fn only_global_functions_outlive_their_scope() {
    let m = misuse();
    runtime().run(|fil| {
        let cb = Callback::<fn(i32) -> i32>::new(fil, |_, (x,)| x + 1).unwrap();
        assert!(matches!(Function::from_global(cb.get(fil)), Err(Error::Unsupported(_))));
        assert_eq!(m.call_it(fil, cb.get(fil), 1).unwrap(), 2);
        let twice = m.library().symbol(fil, "twice").unwrap().as_fn::<fn(i32) -> i32>().unwrap();
        let global = Function::from_global(twice).unwrap();
        assert_eq!(global.call(fil, (4,)).unwrap(), 8);
        assert_eq!(Function::<fn(i32) -> i32>::from_global(FilFn::null()).unwrap_err(), Error::NullCapability);
    });
}

#[test]
fn integers_from_c_never_become_pointers() {
    let m = misuse();
    runtime().run(|fil| {
        let real = fil.c_str("real").unwrap();
        let p = m.int_to_ptr(fil, real.addr()).unwrap();
        assert_eq!(p.addr(), real.addr());
        assert!(!p.has_capability());
        assert_eq!(p.read(fil).unwrap_err(), Error::NullCapability);
        assert_eq!(p.write(fil, &0).unwrap_err(), Error::NullCapability);

        let h = fil.alloc::<Holder<'static>>(&Holder { p: real.cast(), n: 5 }).unwrap();
        assert!(h.read(fil).unwrap().p.has_capability());
        m.holder_launder(fil, h).unwrap();
        let laundered = h.read(fil).unwrap();
        assert_eq!((laundered.p.addr(), laundered.n), (real.addr(), 5));
        assert_eq!(laundered.p.read(fil).unwrap_err(), Error::NullCapability);
    });
}

#[test]
fn bad_pointers_from_c_are_errors() {
    let m = misuse();
    runtime().run(|fil| {
        let end = m.past_the_end(fil).unwrap();
        assert_eq!(end.read(fil).unwrap_err(), Error::OutOfBounds);
        assert!(end.byte_offset(-40).read(fil).is_ok(), "back in bounds, it reads");
        let freed = m.freed_string(fil).unwrap();
        assert_eq!(freed.read_string(fil).unwrap_err(), Error::Freed);
        assert!(freed.capability().unwrap().freed);
        let open = m.unterminated(fil).unwrap();
        assert_eq!(open.read_cstring(fil).unwrap_err(), Error::OutOfBounds);
        assert_eq!(open.cast::<u8>().read_vec(fil, 16).unwrap(), b"abcdefghijklmnop");
    });
}

#[test]
fn casting_to_a_bigger_type_does_not_widen_access() {
    runtime().run(|fil| {
        // (Allocations can be rounded up; 16 bytes aren't.)
        let small = fil.alloc_array::<u32>(&[1, 2, 3, 4]).unwrap();
        assert_eq!(small.len(), 4);
        assert_eq!(small.cast::<[u32; 5]>().read(fil).unwrap_err(), Error::OutOfBounds);
        assert_eq!(small.cast::<[u32; 5]>().write(fil, &[9; 5]).unwrap_err(), Error::OutOfBounds);
        assert_eq!(small.byte_add(8).cast::<[u64; 2]>().write(fil, &[9; 2]).unwrap_err(), Error::OutOfBounds);
        assert_eq!(small.read_array(fil, 4).unwrap(), [1, 2, 3, 4], "failed writes change nothing");
        // Reading integer memory as a pointer gives a pointer without a
        // capability.
        let ints = fil.alloc_array::<u64>(&[real_addr(fil), 0]).unwrap();
        let as_ptr = ints.cast::<Ptr<u8>>().read(fil).unwrap();
        assert!(!as_ptr.has_capability());
    });
}

fn real_addr(fil: &fil_rust::Fil<'_>) -> u64 {
    fil.alloc_bytes(8).unwrap().addr() as u64
}

#[test]
fn handles_to_freed_objects_report_it() {
    let m = misuse();
    let rt = runtime();
    let handle: FilHandle<u8> = rt.run(|fil| fil.root(m.alloc_n(fil, 8).unwrap()).unwrap());
    rt.run(|fil| m.free_it(fil, handle.get(fil).cast()).unwrap());
    rt.run(|fil| {
        assert_eq!(handle.get(fil).read(fil).unwrap_err(), Error::Freed);
        // Freeing again is C's mistake: Fil-C catches double frees (process.rs).
    });
    let null: FilHandle<u8> = rt.run(|fil| fil.root(FilPtr::null()).unwrap());
    rt.run(|fil| assert_eq!(null.get(fil).read(fil).unwrap_err(), Error::NullCapability));
}

#[test]
fn huge_and_empty_allocations() {
    runtime().run(|fil| {
        assert!(fil.alloc_zeroed::<u64>(usize::MAX / 4).is_err(), "size overflows");
        assert!(fil.alloc_zeroed::<[u8; 1 << 20]>(usize::MAX / (1 << 19)).is_err());
        let empty = fil.alloc_array::<i32>(&[]).unwrap();
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.read(fil).unwrap_err(), Error::OutOfBounds);
        assert_eq!(fil.bytes(b"").unwrap().read_vec(fil, 0).unwrap(), b"");
        assert_eq!(fil.buf(0).unwrap().len(), 0);
    });
}

#[test]
fn void_is_not_a_value() {
    let m = misuse();
    runtime().run(|fil| {
        let twice = m.library().function::<fn(i32) -> i32>(fil, "twice").unwrap();
        let r = twice.dyn_call(fil).arg_value::<()>(&()).call::<i32>();
        assert!(matches!(r, Err(Error::UnsupportedAbi(_))));
        let wrong = twice.cast::<fn(()) -> i32>();
        assert!(matches!(wrong.call(fil, ((),)), Err(Error::UnsupportedAbi(_))));
    });
}

#[test]
fn values_that_leave_the_runtime_can_be_dropped_anywhere() {
    let rt = runtime();
    let (handle, cb, buf) = rt.run(|fil| {
        let handle = fil.root(fil.alloc_bytes(4).unwrap()).unwrap();
        let cb = Callback::<fn(i32) -> i32>::new(fil, |_, (x,)| x).unwrap();
        let buf = fil.buf(16).unwrap();
        (handle, cb, buf)
    });
    // On a thread that never entered Fil-C.
    std::thread::spawn(move || drop((handle, cb, buf))).join().unwrap();
    // And the runtime keeps working (the released slots are reused).
    let again: Vec<_> = rt.run(|fil| (0..100).map(|_| fil.root(fil.alloc_bytes(1).unwrap()).unwrap()).collect());
    drop(again);
}

#[test]
fn shared_types_are_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Runtime>();
    send_sync::<Library>();
    send_sync::<Function<fn(i32) -> i32>>();
    send_sync::<FilHandle<u8>>();
    send_sync::<fil_rust::FilBuf>();
    send_sync::<Callback<fn(i32) -> i32>>();
    send_sync::<Misuse>();
    send_sync::<Error>();
    send::<fil_rust::JoinHandle<i32>>();
}

#[test]
fn errors_describe_themselves() {
    let all = [
        Error::Load("x".into()),
        Error::Unsupported("x".into()),
        Error::AlreadyStarted,
        Error::NullCapability,
        Error::Freed,
        Error::OutOfBounds,
        Error::ReadOnly,
        Error::Misaligned,
        Error::SpecialObject,
        Error::NotAFunction,
        Error::Exception,
        Error::MissingSymbol("x".into()),
        Error::InteriorNul,
        Error::Utf8,
        Error::ReturnTooSmall { expected: 8, actual: 4 },
        Error::UnsupportedAbi("x".into()),
        Error::HandleTableFull,
    ];
    let mut seen = std::collections::HashSet::new();
    for e in all {
        let msg = e.to_string();
        assert!(!msg.is_empty());
        assert!(seen.insert(msg.clone()), "two errors read the same: {msg}");
        let boxed: Box<dyn std::error::Error + Send + Sync + 'static> = Box::new(e);
        assert_eq!(boxed.to_string(), msg);
    }
}
