//! The musl flavor runs a libpizlo compiled against musl's headers on top of
//! the host glibc (see "Fil-C on the host glibc" in docs/DESIGN.md). These
//! tests audit that boundary:
//!
//! * every libc symbol libpizlo imports is on a reviewed list
//!   (`tests/abi/<flavor>_imports.txt`), so a new Fil-C version that imports
//!   something new fails here until someone checks it;
//! * the layouts and constants libpizlo hands to libc, built once against
//!   musl's headers and once against glibc's, differ only in the reviewed
//!   ways listed below;
//! * the reviewed differences actually work, end to end.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{fixture, flavor, runtime};
use fil_rust::Flavor;

fn tmp() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("abi-audit");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn env(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set (run the tests in nix-shell)"))
}

fn libpizlo_imports() -> BTreeSet<String> {
    let lib = common::config().lib_dir().join("libpizlo.so");
    let out = Command::new("nm").args(["-D", "--undefined-only"]).arg(&lib).output().expect("running nm");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().lines().filter_map(|l| l.split_whitespace().last()).map(String::from).collect()
}

#[test]
fn libpizlo_imports_are_reviewed() {
    let name = format!("tests/abi/{}_imports.txt", format!("{:?}", flavor()).to_lowercase());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&name);
    let reviewed: BTreeSet<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    let actual = libpizlo_imports();
    let new: Vec<_> = actual.difference(&reviewed).collect();
    let gone: Vec<_> = reviewed.difference(&actual).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "libpizlo's imports changed; review these against the host libc and update {name}\nnew: {new:?}\ngone: {gone:?}"
    );
}

/// Differences between musl's and glibc's x86-64 headers that libpizlo
/// passes across, and why each is safe.
const REVIEWED: &[(&str, &str)] = &[
    // musl reserves extra space at the end; glibc writes less than the
    // (musl-sized, bounds-checked) buffer libpizlo passes.
    ("sizeof struct rusage", "musl buffer is larger"),
    ("sizeof struct sysinfo", "musl buffer is larger"),
    ("sizeof struct sched_param", "musl buffer is larger; glibc only uses sched_priority at offset 0"),
    ("alignof struct sched_param", "musl buffer is larger"),
    // musl splits these size_t fields into int + padding. libpizlo zeroes the
    // msghdr it builds, so glibc reads the right 8-byte value; on receive the
    // low 4 bytes (little-endian) hold the value.
    ("field struct msghdr.msg_iovlen", "libpizlo zeroes the struct"),
    ("field struct msghdr.msg_controllen", "libpizlo zeroes the struct"),
    // The control buffer comes from user memory, so its padding may be
    // dirty: the stand-in's sendmsg/sendmmsg zero it, as musl's do. See
    // `ancillary_data_with_dirty_padding`.
    ("alignof struct cmsghdr", "CMSG_ALIGN is 8 in both"),
    ("field struct cmsghdr.cmsg_len", "padding zeroed by the stand-in"),
    // unsigned short + padding in musl: the low bytes hold the value.
    ("field struct semid_ds.sem_nsems", "little-endian low bytes"),
    // musl passes the real kernel bit, which is a no-op on 64-bit.
    ("const O_LARGEFILE", "kernel ignores it on 64-bit"),
    // Compile-time minimums; libpizlo uses neither.
    ("const MINSIGSTKSZ", "unused by libpizlo"),
    ("const PTHREAD_STACK_MIN", "unused by libpizlo"),
];

fn probe(name: &str, args: &[String]) -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/abi/layout_probe.c");
    let exe = tmp().join(name);
    let status = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        .arg("-w")
        .args(args)
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("running the host C compiler");
    assert!(status.success(), "building the {name} layout probe");
    let out = Command::new(&exe).output().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().map(String::from).collect()
}

#[test]
fn musl_and_glibc_layouts_differ_only_in_reviewed_ways() {
    let musl = probe(
        "probe-musl",
        &["-nostdinc".into(), "-isystem".into(), env("FILC_MUSL_INCLUDE"), "-isystem".into(), env("FILC_LINUX_INCLUDE")],
    );
    let glibc = probe("probe-glibc", &[]);
    assert_eq!(musl.len(), glibc.len());
    assert!(musl.len() > 600, "the probe should cover the whole boundary");
    let mut unreviewed = Vec::new();
    for (m, g) in musl.iter().zip(&glibc) {
        let key = |l: &str| l.rsplitn(if l.starts_with("field") { 3 } else { 2 }, ' ').last().unwrap().to_string();
        assert_eq!(key(m), key(g));
        if m != g && !REVIEWED.iter().any(|(k, _)| *k == key(m)) {
            unreviewed.push(format!("musl: {m}\nglibc: {g}"));
        }
    }
    assert!(unreviewed.is_empty(), "unreviewed differences:\n{}", unreviewed.join("\n"));
}

fil_rust::foreign! {
    struct Sockets {
        fn pass_fd(fill: i32) -> i32;
        fn sem_roundtrip(nsems: i32) -> i32;
    }
}

#[test]
fn ancillary_data_with_dirty_padding() {
    runtime().run(|fil| {
        let s = Sockets::load(fil, fixture("sockets.c")).unwrap();
        assert_eq!(s.pass_fd(fil, 0).unwrap(), 0, "clean control buffer");
        assert_eq!(s.pass_fd(fil, 0xab).unwrap(), 0, "dirty cmsghdr padding");
    });
}

#[test]
fn sysv_semaphores() {
    runtime().run(|fil| {
        let s = Sockets::load(fil, fixture("sockets.c")).unwrap();
        assert_eq!(s.sem_roundtrip(fil, 3).unwrap(), 0);
    });
}

#[test]
fn flavor_matches_the_audit() {
    // The layout audit only matters for musl, but runs for both: it checks
    // the headers, not the loaded runtime.
    assert!(matches!(flavor(), Flavor::Musl | Flavor::Glibc));
}
