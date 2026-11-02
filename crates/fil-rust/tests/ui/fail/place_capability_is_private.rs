// ...or swap the capability of one it was given...
use fil_rust::{Fil, Place};

fn swap<'f>(_fil: &Fil<'f>, at: Place<'f>, other: Place<'f>) -> Place<'f> {
    let mut at = at;
    at.raw = other.raw;
    at
}

fn main() {}
