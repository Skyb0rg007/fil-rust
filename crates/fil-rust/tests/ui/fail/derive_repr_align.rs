// Over-aligned structs aren't supported either.
#[derive(fil_rust::FilType)]
#[repr(align(32))]
struct Wide {
    a: u64,
}

fn main() {}
