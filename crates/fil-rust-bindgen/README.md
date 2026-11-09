# fil-rust-bindgen

Generates [fil-rust](../fil-rust) bindings from C headers: structs become
`#[derive(FilType)]` structs with their layout checked against clang's,
pointers become `Ptr<T>` / `FilPtr<'f, T>`, function-pointer typedefs become
`fn` types for `Callback`, and functions and globals go into a `foreign!`
interface.

From the command line:

    cargo run -p fil-rust-bindgen -- include/api.h --interface Api -o src/api.rs

From a build script:

```rust
let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
fil_rust_bindgen::Builder::new("include/api.h")
    .interface_name("Api")
    .generate()
    .expect("generating bindings")
    .write_to_file(out.join("api.rs"))
    .expect("writing bindings");
```

What fil-rust can't express (variadic functions, bitfields, unions and
flexible arrays by value, `long double`) is left out, with the reason in the
generated module's docs. Needs `LIBCLANG_PATH`.
