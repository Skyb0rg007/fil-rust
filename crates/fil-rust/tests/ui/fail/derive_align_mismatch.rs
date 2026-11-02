// `#[fil(align)]` checks the alignment the C side reports.
#[derive(fil_rust::FilType)]
#[fil(size = 8, align = 8)]
struct Pair {
    a: i32,
    b: i32,
}

fn main() {}
