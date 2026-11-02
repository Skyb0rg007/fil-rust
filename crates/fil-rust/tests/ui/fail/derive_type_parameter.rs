// Field types must be concrete, so the layout is known.
#[derive(fil_rust::FilType)]
struct Wrapper<T> {
    value: T,
}

fn main() {}
