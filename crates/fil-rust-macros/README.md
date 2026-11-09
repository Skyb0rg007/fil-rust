# fil-rust-macros

`#[derive(FilType)]` for [fil-rust](../fil-rust): maps a Rust struct to a C
struct, with C's layout.

```rust
#[derive(fil_rust::FilType, Clone, Copy)]
#[fil(size = 16, align = 8)] // optional, checked at compile time
struct Node<'f> {
    value: i32,
    next: fil_rust::FilPtr<'f, Node<'static>>,
}
```

Fields must be `FilType`s. A struct holding pointers takes one lifetime
parameter. Use it through fil-rust's `derive` feature (on by default), not
directly.
