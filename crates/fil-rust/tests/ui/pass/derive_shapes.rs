// What the derive accepts: `repr(C)` (the same layout either way), tuple
// structs, lifetimes on pointer-holding structs, and nesting.
use fil_rust::{FilPtr, FilType};

#[derive(FilType, Clone, Copy)]
#[repr(C)]
#[fil(size = 16, align = 8)]
struct WithRepr {
    a: u8,
    b: u64,
}

#[derive(FilType, Clone, Copy)]
#[fil(size = 16, align = 8)]
struct WithoutRepr {
    a: u8,
    b: u64,
}

#[derive(FilType, Clone, Copy)]
#[fil(size = 6, align = 2)]
struct Tuple(u16, u8, u16);

#[derive(FilType, Clone, Copy)]
#[fil(size = 24, align = 8)]
struct Holder<'f> {
    tag: u32,
    inner: Tuple,
    p: FilPtr<'f, u8>,
}

fn main() {
    assert_eq!(<WithRepr as FilType>::SIZE, <WithoutRepr as FilType>::SIZE);
    assert_eq!(<Holder<'static> as FilType>::SIZE, 24);
}
