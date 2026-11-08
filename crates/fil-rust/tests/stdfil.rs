//! The parts of `stdfil.h` that ask the runtime about itself, seen from C.
//! Backend-neutral.

mod common;

use std::process::Command;
use std::sync::OnceLock;

use common::{fixture, runtime};
use fil_rust::Library;

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("stdfil.c")).unwrap())
}

fn check(name: &str) {
    let library = library();
    let line = runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap());
    assert_eq!(line, 0, "{name} failed at line {line} of stdfil.c");
}

#[test]
fn settings() {
    check("settings");
}

#[test]
fn describing_pointers() {
    check("describing_pointers");
}

#[test]
fn setting_capabilities() {
    check("setting_capabilities");
}

#[test]
fn requesting_collections() {
    check("requesting_collections");
}

#[test]
#[ignore = "run by the other test, in a process of its own"]
fn printing_alone() {
    check("printing");
}

#[test]
fn printing_and_dumping_stacks() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "printing_alone", "--ignored", "--nocapture"]);
    let output = common::without_core_dumps(&mut command).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("12345\n"), "{stderr}");
    assert!(stderr.contains("stdfil.c") && stderr.contains("printing"), "{stderr}");
}
