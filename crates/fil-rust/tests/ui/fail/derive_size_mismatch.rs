// `#[fil(size)]` checks the layout matches what the C side expects.
#[derive(fil_rust::FilType)]
#[fil(size = 4)]
struct Pair {
    a: i32,
    b: i32,
}

fn main() {
    let _ = Pair { a: 1, b: 2 };
}
