// A FilType implementation can't make up a location to read from...
use fil_rust::{Fil, FilType, Place};

fn forge<'f>(_fil: &Fil<'f>, addr: usize) -> Place<'f> {
    Place { addr }
}

fn main() {}
