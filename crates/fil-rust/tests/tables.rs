//! Pointer tables, weak references, weak maps and finalizer queues, seen from
//! C. Backend-neutral, but for one upstream bug: libpizlo's concurrent collector
//! and the Rust one have to agree on what a full collection leaves behind.

mod common;

use std::sync::{Mutex, OnceLock};

use common::{fixture, runtime};
use fil_rust::Library;

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| runtime().load(fixture("tables.c")).unwrap())
}

/// One at a time: some of these look at what a single collection leaves, which
/// another test's collection would change. Taken outside `run`, as waiting
/// inside would hold up a collection.
fn check(name: &str) {
    static TURN: Mutex<()> = Mutex::new(());
    let library = library();
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let line = runtime().run(|fil| library.function::<fn() -> i32>(fil, name).unwrap().call(fil, ()).unwrap());
    assert_eq!(line, 0, "{name} failed at line {line} of tables.c");
}

#[test]
fn ptr_tables_name_pointers_and_reuse_freed_names() {
    check("ptr_tables");
}

#[test]
fn ptr_tables_keep_what_they_name() {
    check("ptr_tables_keep_what_they_name");
}

#[test]
fn exact_tables_strong_and_weak() {
    check("exact_tables");
}

#[test]
fn weak_references_clear_when_their_object_goes() {
    check("weak_references");
}

#[test]
fn weak_map_values_live_as_long_as_their_keys() {
    check("weak_maps");
}

#[test]
fn finalizers_get_what_would_have_been_collected() {
    check("finalizers");
}

#[test]
fn finalizers_without_a_queue() {
    check("finalizers_without_a_queue");
}

/// libpizlo hands these back after collecting them, issue 6 in
/// docs/upstream-filc-issues.md.
#[test]
fn weak_things_reached_by_finalizable_objects() {
    if runtime().config().backend() != fil_rust::Backend::Rust {
        return;
    }
    check("weak_things_reached_by_finalizable_objects");
}
