use std::path::{Path, PathBuf};

use fil_rust_bindgen::Builder;

/// The fil-rust crate, whose tests own the fixtures and generated files.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("fil-rust")
}

/// Generates bindings for `tests/fixtures/<name>.h` and checks them against
/// `tests/generated/<name>.rs`, which fil-rust's `tests/bindgen.rs` compiles
/// (and, for `api`, runs). Set `FIL_RUST_BLESS=1` to update them.
fn golden(name: &str, builder: impl FnOnce(Builder) -> Builder) -> fil_rust_bindgen::Bindings {
    let header = repo().join(format!("tests/fixtures/{name}.h"));
    let bindings = builder(Builder::new(&header)).generate().unwrap();
    let path = repo().join(format!("tests/generated/{name}.rs"));
    if std::env::var_os("FIL_RUST_BLESS").is_some() {
        bindings.write_to_file(&path).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(committed == bindings.to_string(), "{} is stale; rerun with FIL_RUST_BLESS=1", path.display());
    bindings
}

#[test]
fn api_bindings_are_up_to_date() {
    let b = golden("api", |b| b.interface_name("Api"));
    assert_eq!(b.skipped().len(), 1, "{:?}", b.skipped());
}

fn skipped(b: &fil_rust_bindgen::Bindings, needle: &str) -> bool {
    b.skipped().iter().any(|s| s.contains(needle))
}

#[test]
fn unrepresentable_items_are_left_out_with_a_reason() {
    let b = golden("edge", |b| b);
    let code = b.to_string();
    assert!(skipped(&b, "`flags` is only usable behind a pointer: bitfields"), "{:?}", b.skipped());
    assert!(skipped(&b, "`num` is only usable behind a pointer: unions aren't supported"));
    assert!(skipped(&b, "`packet` is only usable behind a pointer: flexible array"));
    assert!(skipped(&b, "`holder` is only usable behind a pointer: field `n`"));
    assert!(skipped(&b, "function `take_flags`: parameter `f`"));
    assert!(skipped(&b, "function `take_union`"));
    assert!(skipped(&b, "function `precise`: return type: 128-bit"));
    assert!(skipped(&b, "function `logf_`: variadic"));
    assert!(skipped(&b, "typedef `vfn`: variadic function pointer"));
    assert!(skipped(&b, "function `with_vfn`"));
    // Pointers to them are still fine.
    assert!(code.contains("fn take_flags_ptr(f: Ptr<flags>);"), "{code}");
    assert!(code.contains("fn take_holder_ptr(h: Ptr<holder>);"));
    // A typedef of an unrepresentable struct is left out; pointers to it
    // become untyped.
    assert!(skipped(&b, "typedef `holder_t`"));
    assert!(code.contains("fn take_holder_t_ptr(h: Ptr);"), "{code}");
    assert!(code.contains("pub enum num {}"));
    assert!(code.contains("fn fine(o: ok, n: Ptr<num>) -> c_int;"));
    // And the skipped items are listed in the module docs.
    assert!(code.contains("/// * function `logf_`: variadic"));
}

#[test]
fn typedefs_and_pointer_shapes() {
    let b = golden("edge", |b| b);
    let code = b.to_string();
    // A typedef of a pointer takes the lifetime. (bindgen renames `str`.)
    assert!(code.contains("pub type str_<'f> = FilPtr<'f, c_char>;"), "{code}");
    assert!(code.contains("pub struct node<'f>"));
    assert!(code.contains("pub label: str_<'f>,"));
    assert!(code.contains("pub kids: [FilPtr<'f, node<'static>>; 2],"));
    assert!(code.contains("pub type node_t<'f> = node<'f>;"));
    assert!(code.contains("pub type cb = fn(Ptr, Ptr<node_t<'static>>);"));
    assert!(code.contains("pub on_visit: FilFn<'f, cb>,"));
    assert!(code.contains("pub raw: FilFn<'f, fn()>,"));
    assert!(code.contains("pub user: FilPtr<'f>,"));
    assert!(code.contains("static names: [Ptr<c_char>; 3];"));
    assert!(code.contains("fn split(s: str_<'static>, count: Ptr<c_int>) -> Ptr<str_<'static>>;"));
    // Parameters can't be called `fil`, which the methods take first.
    assert!(code.contains("fn fil(fil_: c_int, self_: c_int) -> c_int;"));
}
