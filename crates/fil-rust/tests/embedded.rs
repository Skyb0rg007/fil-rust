//! Libraries compiled with fil-rust-build and loaded from memory
//! (`EmbeddedLibrary`, `Fil::load_embedded`). The libraries are built when
//! the tests run rather than by a build script, so their bytes are leaked
//! into an `EmbeddedLibrary` here instead of coming from `include_library!`
//! (which bindgen-demo uses).

mod common;

use std::path::Path;

use common::{flavor, runtime};
use fil_rust::{EmbeddedLibrary, Error, Flavor};

fil_rust::foreign! {
    struct Counter {
        fn embed_bump() -> i32;
    }
}

fn build_flavor() -> fil_rust_build::Flavor {
    match flavor() {
        Flavor::Musl => fil_rust_build::Flavor::Musl,
        Flavor::Glibc => fil_rust_build::Flavor::Glibc,
    }
}

/// Builds `lib<name>.so` from fixtures for the configured flavor.
fn build(name: &str, files: &[&str], link_with: Option<&fil_rust_build::Output>) -> fil_rust_build::Output {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("embedded").join(build_flavor().name());
    let mut b = fil_rust_build::Build::new();
    b.out_dir(dir).cargo_metadata(false).flavor(build_flavor()).opt_level("1");
    for f in files {
        b.file(fixtures.join(f));
    }
    if let Some(dep) = link_with {
        b.link_with(dep);
    }
    b.compile(name)
}

/// `bytes` as the configured flavor's build of `name`.
fn embedded(name: &'static str, bytes: Vec<u8>) -> EmbeddedLibrary {
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    match flavor() {
        Flavor::Musl => EmbeddedLibrary::new(name, Some(bytes), None),
        Flavor::Glibc => EmbeddedLibrary::new(name, None, Some(bytes)),
    }
}

fn built(name: &'static str, files: &[&str]) -> EmbeddedLibrary {
    let out = build(name, files, None);
    embedded(name, std::fs::read(out.path(build_flavor()).unwrap()).unwrap())
}

fn load_error(result: fil_rust::Result<impl std::fmt::Debug>) -> String {
    match result {
        Err(Error::Load(msg)) => msg,
        other => panic!("expected a load error, got {other:?}"),
    }
}

#[test]
fn embedded_libraries_load_once_and_run() {
    let rt = runtime();
    let lib = built("embed_counter", &["embed_counter.c"]);
    assert_eq!(lib.soname(), "libembed_counter.so");
    let first = rt.run(|fil| Counter::load_embedded(fil, &lib)).unwrap();
    assert_eq!(rt.run(|fil| first.embed_bump(fil)).unwrap(), 1);
    // Loading it again gives the same library, with the same state.
    let again = rt.load_embedded(&lib).unwrap();
    let again = rt.run(|fil| Counter::from_library(fil, again)).unwrap();
    assert_eq!(rt.run(|fil| again.embed_bump(fil)).unwrap(), 2);
    assert_eq!(rt.run(|fil| first.embed_bump(fil)).unwrap(), 3);
}

#[test]
fn embedded_libraries_find_embedded_dependencies_loaded_first() {
    let rt = runtime();
    let base_out = build("embed_base", &["embed_base.c"], None);
    let user_out = build("embed_user", &["embed_user.c"], Some(&base_out));
    let base = embedded("embed_base", std::fs::read(base_out.path(build_flavor()).unwrap()).unwrap());
    let user = embedded("embed_user", std::fs::read(user_out.path(build_flavor()).unwrap()).unwrap());

    // Before its dependency, the loader can't find it.
    let err = load_error(rt.load_embedded(&user));
    assert!(err.contains("libembed_base.so"), "{err}");

    rt.load_embedded(&base).unwrap();
    let user = rt.load_embedded(&user).unwrap();
    let value = rt.run(|fil| -> fil_rust::Result<i32> {
        user.function::<fn() -> i32>(fil, "embed_user_value")?.call(fil, ())
    });
    assert_eq!(value.unwrap(), 42);
}

#[test]
fn a_flavor_without_a_build_is_reported() {
    let err = load_error(runtime().load_embedded(&EmbeddedLibrary::new("embed_none", None, None)));
    assert!(err.contains("libembed_none.so has no build for the"), "{err}");
}

#[test]
fn a_different_library_under_a_loaded_name_is_refused() {
    let rt = runtime();
    let counter = std::fs::read(build("embed_name_a", &["embed_counter.c"], None).path(build_flavor()).unwrap());
    let base = std::fs::read(build("embed_name_b", &["embed_base.c"], None).path(build_flavor()).unwrap());
    rt.load_embedded(&embedded("embed_name_a", counter.unwrap())).unwrap();
    let err = load_error(rt.load_embedded(&embedded("embed_name_a", base.unwrap())));
    assert!(err.contains("a different library named libembed_name_a.so"), "{err}");
}

#[test]
fn a_library_loaded_from_a_file_isnt_loaded_again_from_memory() {
    let rt = runtime();
    let out = build("embed_file", &["embed_counter.c"], None);
    let path = out.path(build_flavor()).unwrap();
    rt.load(path).unwrap();
    let err = load_error(rt.load_embedded(&embedded("embed_file", std::fs::read(path).unwrap())));
    assert!(err.contains("already loaded from a file"), "{err}");
}

#[test]
fn invalid_names_are_refused() {
    let err = load_error(runtime().load_embedded(&embedded("../escape", vec![0x7f])));
    assert!(err.contains("not a valid library name"), "{err}");
}
