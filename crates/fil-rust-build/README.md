# fil-rust-build

Compiles C and C++ with [Fil-C](https://fil-c.org/) in a Cargo build script,
into a shared library the program embeds, so the C code ships inside the
binary and runs memory-safe.

```rust
// build.rs
fn main() {
    fil_rust_build::Build::new()
        .files(["c/zap.c", "c/zap_util.c"])
        .include("c")
        .define("ZAP_FAST", None)
        .compile("zap");
}
```

```rust
// src/main.rs
static ZAP: fil_rust::EmbeddedLibrary = fil_rust::include_library!("zap");
let zap = rt.load_embedded(&ZAP)?;
```

It builds once per Fil-C flavor whose compiler it finds (`FILC_GLIBC_CC` or
`/opt/fil` for glibc, `FILC_CC` or `FILC_ROOT` for musl), or the flavors
named with `Build::flavor`. Libraries can link against each other with
`Build::link_with`; load the dependency first.
