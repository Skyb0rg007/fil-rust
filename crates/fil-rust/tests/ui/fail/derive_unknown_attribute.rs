// A misspelled check is an error, not a check that never runs.
#[derive(fil_rust::FilType)]
#[fil(sise = 8)]
struct Pair {
    a: i32,
    b: i32,
}

fn main() {}
