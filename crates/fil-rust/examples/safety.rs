//! What happens when something goes wrong.
//!
//! Rust-side mistakes (reading out of bounds, using freed memory, looking up a
//! missing symbol) are `Err`s. Fil-C-side memory bugs are caught by Fil-C and
//! end the process with a diagnostic, as they would in a Fil-C program; this
//! example shows that in a child process.
//!
//!     cargo run --example safety

mod support;

use std::process::Command;

use fil_rust::{Library, Ptr};

fil_rust::foreign! {
    struct UnsafeC {
        fn make_array(n: i32) -> Ptr<i32>;
        fn release(p: Ptr);
        fn nth(a: Ptr<i32>, i: i32) -> i32;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = support::runtime();
    let path = support::build("unsafe_c.c");

    if std::env::args().nth(1).as_deref() == Some("--c-bug") {
        // Child: C indexes past the end of its own allocation.
        rt.run(|fil| -> fil_rust::Result<()> {
            let lib = UnsafeC::load(fil, &path)?;
            let a = lib.make_array(fil, 4)?;
            println!("nth(a, 2) = {}", lib.nth(fil, a, 2)?);
            println!("nth(a, 10) = ...");
            fil.flush()?;
            lib.nth(fil, a, 10)?;
            unreachable!("Fil-C stops the out-of-bounds read");
        })?;
    }

    rt.run(|fil| -> fil_rust::Result<()> {
        let lib = UnsafeC::load(fil, &path)?;
        let a = lib.make_array(fil, 4)?;
        println!("C allocated: {:?}", a.read_array(fil, 4)?);

        // Every pointer carries its bounds.
        println!("reading 5 elements: {:?}", a.read_array(fil, 5).unwrap_err());
        println!("reading index -1: {:?}", a.byte_offset(-4).read(fil).unwrap_err());

        // And its object's liveness: after C frees it, reads fail.
        lib.release(fil, a.cast())?;
        println!("after free: {:?}", a.read(fil).unwrap_err());

        // A wrong guess at a symbol is an error too.
        let missing: fil_rust::Result<Library> = fil.load("/nonexistent/libnope.so");
        println!("loading a missing library: {:?}", missing.unwrap_err());
        println!("missing symbol: {:?}", lib.library().function::<fn()>(fil, "no_such_fn").unwrap_err());
        Ok(())
    })?;

    // C-side bugs end the process: run one in a child and show what it said.
    let out = Command::new(std::env::current_exe().unwrap()).arg("--c-bug").output().unwrap();
    println!("\nchild exited with {}", out.status);
    println!("child stdout:\n{}", String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("child stderr (first lines):");
    for line in stderr.lines().take(4) {
        println!("    {line}");
    }
    Ok(())
}
