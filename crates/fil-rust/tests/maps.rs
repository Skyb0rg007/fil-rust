//! `mmap` and what goes with it, seen from C through the unmodified libc.

mod common;

use std::ffi::c_char;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, runtime};
use fil_rust::{Library, Ptr};

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("maps.c")).unwrap())
}

fn call(name: &str) -> i32 {
    let library = library();
    runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap())
}

fn scratch(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("maps-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(name)
}

fn check(name: &str) {
    let line = call(name);
    assert_eq!(line, 0, "{name} failed at line {line} of maps.c");
}

fn check_with_path(name: &str, path: &Path) {
    let library = library();
    let line = runtime().run(|fil| {
        let function = library.function::<fn(Ptr<c_char>) -> i32>(fil, name).unwrap();
        function.call(fil, (fil.c_str(path.to_str().unwrap()).unwrap(),)).unwrap()
    });
    assert_eq!(line, 0, "{name} failed at line {line} of maps.c");
}

#[test]
fn anonymous_memory() {
    check("anonymous_memory");
}

#[test]
fn arguments_are_turned_down_as_they_are_by_the_kernel() {
    check("rejected_arguments");
}

#[test]
fn files_show_through_mappings() {
    check_with_path("file_mappings", &scratch("file"));
}

#[test]
fn mappings_can_move_and_shrink() {
    check_with_path("remapping", &scratch("remapped"));
}

#[test]
fn file_pages_can_be_rearranged() {
    check_with_path("rearranging_file_pages", &scratch("rearranged"));
}

#[test]
fn shared_memory_segments() {
    check("shared_memory");
}

#[test]
fn a_collection_takes_mappings_nothing_refers_to() {
    let path = scratch("forgotten");
    let file = File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    file.set_len(16 * 4096).unwrap();
    let is_mapped = || std::fs::read_to_string("/proc/self/maps").unwrap().contains(path.to_str().unwrap());

    let library = library();
    runtime().run(|fil| {
        let forget = library.function::<fn(i32) -> i32>(fil, "forget_a_mapping").unwrap();
        assert_eq!(forget.call(fil, (file.as_raw_fd(),)).unwrap(), 0);
    });
    assert!(is_mapped());
    for _ in 0..10 {
        runtime().collect().unwrap();
        if !is_mapped() {
            return;
        }
    }
    panic!("the mapping outlived the references to it");
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    call(&case);
}

#[test]
fn misuse_is_stopped() {
    for (case, message) in [
        ("free_a_mapping", "mmap object"),
        ("use_after_munmap", "free object"),
        ("unmap_past_the_end", "upper - ptr"),
        ("unmap_ordinary_memory", "not mmapped"),
        ("protect_ordinary_memory", "not mmapped"),
        ("drop_memory_outside_a_mapping", "not mmapped"),
        ("map_over_ordinary_memory", "not mmapped"),
        ("detach_from_the_middle", "not to the base"),
        ("unsupported_advice", "MADV_DONTFORK"),
    ] {
        let output = common::without_core_dumps(&mut Command::new(std::env::current_exe().unwrap()))
            .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env("FIL_RUST_CHILD", case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: {stderr}");
        assert!(stderr.contains("thwarted a futile attempt to violate memory safety"), "{case}: {stderr}");
        assert!(stderr.contains(message), "{case}: {stderr}");
    }
}
