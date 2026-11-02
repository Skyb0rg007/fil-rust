// Enums have no single C layout to derive.
#[derive(fil_rust::FilType)]
enum Kind {
    A,
    B,
}

fn main() {}
