// A flexible array member has no fixed layout.
#[derive(fil_rust::FilType)]
struct Flexible {
    len: usize,
    data: [u8],
}

fn main() {}
