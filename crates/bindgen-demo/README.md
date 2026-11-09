# bindgen-demo

A small program showing [fil-rust-build](../fil-rust-build) and
[fil-rust-bindgen](../fil-rust-bindgen) together. Its build script compiles
`c/inventory.c` with Fil-C and generates bindings from `c/inventory.h`; the
program embeds the library and loads it from memory, so there's no `.so` to
ship beside it.

    cargo run -p bindgen-demo --features glibc

(from the dev shell, with `FILC_FLAVOR=glibc`).
