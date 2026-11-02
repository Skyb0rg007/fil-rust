//! Calling Fil-C functions: integers, strings, arrays, and libc.
//!
//!     cargo run --example hello

mod support;

use std::ffi::c_char;

use fil_rust::{Ptr, Runtime};

fil_rust::foreign! {
    struct Hello {
        fn add(a: i32, b: i32) -> i32;
        fn greet(name: Ptr<c_char>) -> Ptr<c_char>;
        fn shout(s: Ptr<c_char>);
        fn average(values: Ptr<f64>, n: usize) -> f64;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt: Runtime = support::runtime();
    let path = support::build("hello.c");

    // Everything that touches Fil-C happens inside `run`, on a Fil-C thread.
    rt.run(|fil| -> fil_rust::Result<()> {
        let hello = Hello::load(fil, &path)?;

        println!("2 + 3 = {}", hello.add(fil, 2, 3)?);

        // `c_str` copies a Rust string into a fresh Fil-C allocation; the
        // returned pointer carries a capability, so C may read it.
        let greeting = hello.greet(fil, fil.c_str("Rust")?)?;
        // Reads from Fil-C memory are checked against that capability.
        println!("{}", greeting.read_string(fil)?);

        // C may also write to memory we allocate.
        let buf = fil.c_str("quiet please")?;
        hello.shout(fil, buf)?;
        println!("{}", buf.read_string(fil)?);

        let values = [1.5, 2.5, 3.5, 4.5];
        let arr = fil.alloc_array::<f64>(&values)?;
        println!("average = {}", hello.average(fil, arr, values.len())?);

        // The Fil-C libc is a library like any other.
        let printf = fil.libc().function::<fn(Ptr<c_char>, i32) -> i32>(fil, "printf")?;
        printf.call(fil, (fil.c_str("printf from Fil-C: %d\n")?, 42))?;
        fil.flush()?;
        Ok(())
    })
}
