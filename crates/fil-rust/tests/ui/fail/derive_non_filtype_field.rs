// A struct can derive FilType only if every field has a C representation.
#[derive(fil_rust::FilType)]
struct Bad {
    id: i32,
    name: String,
}

fn main() {}
