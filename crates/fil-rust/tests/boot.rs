//! How the runtime sits in the process for each flavor: which objects are
//! mapped, which libc each side uses, and which threads run Fil-C code.

mod common;

use std::ffi::c_char;

use common::{flavor, runtime};
use fil_rust::{Flavor, Ptr};

fn thread_name(tid: &str) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")).unwrap_or_default().trim().to_string()
}

fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|e| thread_name(e.unwrap().file_name().to_str().unwrap()))
        .collect()
}

#[test]
fn jobs_run_on_fil_c_executor_threads() {
    let rt = runtime();
    let caller = unsafe { libc::gettid() };
    let tid = rt.run(|fil| fil.libc().function::<fn() -> i32>(fil, "gettid").unwrap().call(fil, ()).unwrap());
    assert_ne!(tid, caller);
    assert_eq!(thread_name(&tid.to_string()), "fil-c-worker");

    // One Fil-C main thread (the executor spawner) and at least the
    // configured number of executors.
    let names = thread_names();
    assert_eq!(names.iter().filter(|n| *n == "fil-c-main").count(), 1, "{names:?}");
    let workers = names.iter().filter(|n| *n == "fil-c-worker").count();
    assert!(workers >= rt.config().worker_count(), "{names:?}");
}

#[test]
fn the_yolo_libc_is_replaced_by_stand_ins() {
    let rt = runtime();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let paths: Vec<&str> = maps.lines().filter_map(|l| l.split_whitespace().nth(5)).collect();
    let real_yolo: &[&str] = match rt.flavor() {
        Flavor::Musl => &["libyoloc.so", "ld-fil1-x86_64.so"],
        Flavor::Glibc => &["libyolocimpl.so", "libyolomimpl.so", "ld-fil1-x86_64.so"],
    };
    for name in real_yolo {
        assert!(
            !paths.iter().any(|p| p.starts_with('/') && p.ends_with(&format!("/{name}"))),
            "the distribution's {name} is mapped"
        );
    }
    // The stand-ins are loaded from memory.
    assert!(paths.iter().any(|p| *p == format!("/memfd:{}", real_yolo[0])), "no stand-in for {}", real_yolo[0]);
    // And the runtime comes from the configured distribution.
    let lib_dir = rt.config().lib_dir();
    assert!(paths.iter().any(|p| *p == lib_dir.join("libpizlo.so").to_str().unwrap()));
}

#[test]
fn each_side_has_its_own_libc() {
    let rt = runtime();
    assert_eq!(rt.flavor(), flavor());
    // The Rust side is always linked to the host glibc.
    let host = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) }.to_str().unwrap().to_string();
    assert!(host.starts_with("2."));

    // The Fil-C side uses the flavor's own user libc.
    let fil_c_glibc = rt.run(|fil| {
        let libc = fil.libc();
        if !libc.has_symbol("gnu_get_libc_version") {
            return None;
        }
        let version = libc.function::<fn() -> Ptr<c_char>>(fil, "gnu_get_libc_version").unwrap();
        Some(version.call(fil, ()).unwrap().read_string(fil).unwrap())
    });
    match rt.flavor() {
        Flavor::Musl => assert_eq!(fil_c_glibc, None),
        Flavor::Glibc => assert!(fil_c_glibc.unwrap().starts_with("2.")),
    }
}
