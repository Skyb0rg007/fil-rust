// A packed C struct needs a layout the derive doesn't compute: refused, not
// silently laid out with padding.
#[derive(fil_rust::FilType)]
#[repr(C, packed)]
struct Packed {
    a: u8,
    b: u32,
}

fn main() {}
