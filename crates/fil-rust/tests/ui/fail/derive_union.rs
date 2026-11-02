// Unions would let a pointer be read back as an integer and vice versa.
#[derive(fil_rust::FilType)]
union Either {
    i: u64,
    f: f64,
}

fn main() {}
