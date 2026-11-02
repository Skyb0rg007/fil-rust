// A pointer from a nested scope can't be used with the outer scope's token
// after the nested scope is gone (Fil's lifetime is invariant).
use fil_rust::{Fil, FilPtr};

fn smuggle<'o, 'i>(_outer: &Fil<'o>, p: FilPtr<'i, u8>) -> FilPtr<'o, u8> {
    p
}

fn main() {}
