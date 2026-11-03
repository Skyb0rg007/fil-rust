//! Builds with the real Fil-C compilers (FILC_CC, and FILC_GLIBC_CC if set;
//! run in nix-shell). Whether the results load and run is tested by
//! fil-rust's tests/embedded.rs.

use std::path::{Path, PathBuf};

use fil_rust_build::{Build, Flavor};

fn c(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c").join(file)
}

fn out_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fil-rust-build").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn build(test: &str) -> (Build, PathBuf) {
    let dir = out_dir(test);
    let mut b = Build::new();
    b.out_dir(&dir).cargo_metadata(false).include(c("include")).define("SCALE", Some("3"));
    (b, dir)
}

#[test]
fn c_and_cxx_sources_become_one_library_per_flavor() {
    let (mut b, dir) = build("mixed");
    let out = b.file(c("greet.c")).file(c("twice.cpp")).compile("mixed");
    assert_eq!(out.name(), "mixed");
    let flavors: Vec<Flavor> = out.flavors().collect();
    assert!(flavors.contains(&Flavor::Musl), "{flavors:?}");
    for flavor in Flavor::ALL {
        let include = std::fs::read_to_string(dir.join(format!("mixed.{flavor}.rs"))).unwrap();
        match out.path(flavor) {
            Some(lib) => {
                assert_eq!(lib, dir.join(flavor.name()).join("libmixed.so"));
                let bytes = std::fs::read(lib).unwrap();
                assert!(bytes.starts_with(b"\x7fELF"));
                // The soname, and Fil-C's getters for both languages' exports.
                for needle in ["libmixed.so", "pizlonated_greet_len", "pizlonated_twice_scaled", "libpizlo.so"] {
                    assert!(bytes.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle} in {flavor}");
                }
                assert!(include.contains("include_bytes!") && include.contains(lib.to_str().unwrap()), "{include}");
            }
            None => assert!(include.contains("None"), "{include}"),
        }
    }
}

#[test]
fn included_headers_are_inputs() {
    let (mut b, _) = build("inputs");
    let out = b.file(c("greet.c")).compile("inputs");
    let inputs = out.inputs();
    assert!(inputs.contains(&c("greet.c")), "{inputs:?}");
    assert!(inputs.iter().any(|p| p.ends_with("include/greet.h")), "{inputs:?}");
    // System headers aren't.
    assert!(!inputs.iter().any(|p| p.ends_with("string.h")), "{inputs:?}");
}

#[test]
fn compiler_errors_come_back_with_the_compiler_output() {
    let (mut b, _) = build("broken");
    let err = b.file(c("broken.c")).try_compile("broken").unwrap_err().to_string();
    assert!(err.contains("compiling") && err.contains("broken.c"), "{err}");
    assert!(err.contains("undeclared_name"), "{err}");
}

#[test]
fn libraries_link_with_earlier_ones() {
    let (mut base, _) = build("link");
    let base = base.file(c("greet.c")).compile("linkbase");
    let (mut user, _) = build("link-user");
    let user = user.file(c("twice.cpp")).link_with(&base).compile("linkuser");
    for flavor in user.flavors() {
        let bytes = std::fs::read(user.path(flavor).unwrap()).unwrap();
        // The dependency is recorded by soname.
        assert!(bytes.windows(14).any(|w| w == b"liblinkbase.so"), "{flavor}");
    }
}

#[test]
fn bad_names_and_empty_builds_are_refused() {
    let (mut b, _) = build("bad");
    assert!(b.file(c("greet.c")).try_compile("../evil").is_err());
    let (b, _) = build("empty");
    let err = b.try_compile("empty").unwrap_err().to_string();
    assert!(err.contains("no source files"), "{err}");
}
