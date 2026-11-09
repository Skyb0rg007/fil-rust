//! The glibc flavor's boundary with the host libc. Natives hand Fil-C's
//! structs to the host's glibc as they are, so the two sets of headers must
//! agree but where reviewed; and the libc stand-ins must export everything the
//! distribution's libc does, at every version, or the loader refuses
//! libraries built against it.
#![cfg(feature = "glibc")]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{fixture, fixture_with, flavor, runtime};
use fil_rust::Flavor;

fn tmp() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("abi-audit");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn glibc() -> bool {
    flavor() == Flavor::Glibc
}

/// Differences between the Fil-C glibc's headers and the host's, and why
/// each is safe.
const REVIEWED: &[(&str, &str)] = &[
    // Fil-C's isn't packed, so a pointer in the data stays aligned: the
    // natives convert every event.
    ("sizeof struct epoll_event", "converted by the epoll natives"),
    ("alignof struct epoll_event", "converted by the epoll natives"),
    ("field struct epoll_event.data", "converted by the epoll natives"),
];

const PROBE: &str = "tests/abi/layout_probe.c";

/// The host's layouts.
fn host_probe() -> Vec<String> {
    let exe = tmp().join("probe-host");
    let cc = ["FILC_TARGET_CC", "CC"].into_iter().find_map(|v| std::env::var(v).ok()).unwrap_or_else(|| "cc".into());
    let status = Command::new(cc).arg("-w").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(PROBE)).arg("-o").arg(&exe).status().unwrap();
    assert!(status.success(), "building the host's layout probe");
    let out = Command::new(&exe).output().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().map(String::from).collect()
}

const CHILD: &str = "FIL_RUST_ABI_AUDIT_CHILD";

#[test]
#[ignore = "run by the other tests, in a process of its own"]
fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let probe = fixture_with("../abi/layout_probe.c", &["-w", "-Dmain=layout_probe"]);
    let rt = runtime();
    let library = rt.load(probe).unwrap();
    rt.run(|fil| library.function::<fn() -> i32>(fil, "layout_probe").unwrap().call(fil, ()).unwrap());
    rt.flush().unwrap();
}

/// The Fil-C glibc's layouts, as compiled code sees them.
fn fil_probe() -> Vec<String> {
    let mut command = Command::new(std::env::current_exe().unwrap());
    // Quiet, so the harness writes nothing before the probe's first line.
    command.args(["--exact", "child", "--nocapture", "--ignored", "--test-threads=1", "--quiet"]).env(CHILD, "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout.lines().filter(|line| ["sizeof ", "alignof ", "field ", "const "].iter().any(|kind| line.starts_with(kind))).map(String::from).collect()
}

#[test]
fn fil_c_and_host_layouts_differ_only_in_reviewed_ways() {
    if !glibc() {
        return;
    }
    let (host, fil) = (host_probe(), fil_probe());
    assert_eq!(fil.len(), host.len());
    assert!(host.len() > 600, "the probe should cover the whole boundary");
    let key = |line: &str| line.rsplitn(if line.starts_with("field") { 3 } else { 2 }, ' ').last().unwrap().to_string();
    let mut unreviewed = Vec::new();
    for (f, h) in fil.iter().zip(&host) {
        assert_eq!(key(f), key(h));
        if f != h && !REVIEWED.iter().any(|(k, _)| *k == key(f)) {
            unreviewed.push(format!("Fil-C: {f}\nhost:  {h}"));
        }
    }
    assert!(unreviewed.is_empty(), "unreviewed differences:\n{}", unreviewed.join("\n"));
}

/// A library's defined `pizlonated_` symbols, with their versions.
fn exports(path: &Path) -> BTreeSet<String> {
    let output = Command::new("readelf").args(["--dyn-syms", "-W"]).arg(path).output().expect("running readelf");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Num, Value, Size, Type, Bind, Vis, Ndx, Name.
            let (&ndx, &name) = (fields.get(6)?, fields.get(7)?);
            (ndx != "UND" && name.starts_with("pizlonated_")).then(|| name.replace("@@", "@"))
        })
        .collect()
}

#[test]
fn the_stand_ins_export_what_the_distribution_does() {
    if !glibc() {
        return;
    }
    let lib_dir = common::config().lib_dir();
    let stand_ins = Flavor::Glibc.libc_stand_ins();
    assert_eq!(stand_ins.len(), 4);
    for stand_in in stand_ins {
        let copy = tmp().join(stand_in.soname);
        std::fs::write(&copy, stand_in.bytes).unwrap();
        let (ours, theirs) = (exports(&copy), exports(&lib_dir.join(stand_in.soname)));
        assert!(!theirs.is_empty(), "{} exports nothing", stand_in.soname);
        let missing: Vec<_> = theirs.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&theirs).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{}: missing {missing:?}, extra {extra:?}", stand_in.soname);
    }
}

fil_rust::foreign! {
    struct Sockets {
        fn pass_fd(fill: i32) -> i32;
        fn sem_roundtrip(nsems: i32) -> i32;
    }
}

#[test]
fn ancillary_data_with_dirty_padding() {
    if !glibc() {
        return;
    }
    runtime().run(|fil| {
        let s = Sockets::load(fil, fixture("sockets.c")).unwrap();
        assert_eq!(s.pass_fd(fil, 0).unwrap(), 0, "clean control buffer");
        assert_eq!(s.pass_fd(fil, 0xab).unwrap(), 0, "dirty cmsghdr padding");
    });
}

#[test]
fn sysv_semaphores() {
    if !glibc() {
        return;
    }
    runtime().run(|fil| {
        let s = Sockets::load(fil, fixture("sockets.c")).unwrap();
        assert_eq!(s.sem_roundtrip(fil, 3).unwrap(), 0);
    });
}
