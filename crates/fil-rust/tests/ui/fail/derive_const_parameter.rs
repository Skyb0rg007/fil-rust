#[derive(fil_rust::FilType)]
struct Buffer<const N: usize> {
    bytes: [u8; N],
}

fn main() {}
