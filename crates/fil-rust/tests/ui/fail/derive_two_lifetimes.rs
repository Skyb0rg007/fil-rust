// One lifetime: the scope the struct's pointers live in.
#[derive(fil_rust::FilType)]
struct Two<'a, 'b> {
    first: fil_rust::FilPtr<'a, u8>,
    second: fil_rust::FilPtr<'b, u8>,
}

fn main() {}
