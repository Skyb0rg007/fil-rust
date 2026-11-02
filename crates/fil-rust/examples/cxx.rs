//! A C++ library: objects behind opaque pointers, and exceptions.
//!
//!     cargo run --example cxx

mod support;

use std::ffi::c_char;

use fil_rust::{Error, Ptr};

fil_rust::foreign! {
    struct Inventory {
        fn inventory_new() -> Ptr;
        fn inventory_free(inv: Ptr);
        fn inventory_add(inv: Ptr, item: Ptr<c_char>, n: i32);
        fn inventory_remove(inv: Ptr, item: Ptr<c_char>, n: i32);
        fn inventory_try_remove(inv: Ptr, item: Ptr<c_char>, n: i32) -> i32;
        fn inventory_count(inv: Ptr, item: Ptr<c_char>) -> i32;
        fn inventory_report(inv: Ptr) -> Ptr<c_char>;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = support::runtime();
    let path = support::build("inventory.cpp");
    rt.run(|fil| -> fil_rust::Result<()> {
        let lib = Inventory::load(fil, &path)?;
        let inv = lib.inventory_new(fil)?;
        let s = |text: &str| fil.c_str(text);

        lib.inventory_add(fil, inv, s("apples")?, 5)?;
        lib.inventory_add(fil, inv, s("pears")?, 2)?;
        lib.inventory_remove(fil, inv, s("apples")?, 3)?;
        println!("inventory: {}", lib.inventory_report(fil, inv)?.read_string(fil)?);

        // An exception caught inside C++ is just a return value.
        println!("try_remove(plums) = {}", lib.inventory_try_remove(fil, inv, s("plums")?, 1)?);

        // An exception that escapes to Rust becomes `Error::Exception`. The
        // C++ frames unwind (running destructors) and the runtime carries on.
        for (item, n) in [("plums", 1), ("pears", 10)] {
            match lib.inventory_remove(fil, inv, s(item)?, n) {
                Err(Error::Exception) => println!("remove({item}, {n}): C++ threw"),
                other => println!("remove({item}, {n}): {other:?}"),
            }
        }
        match lib.inventory_add(fil, inv, s("figs")?, 0) {
            Err(e) => println!("add(figs, 0): {e}"),
            Ok(()) => println!("add(figs, 0) succeeded"),
        }

        // The object is intact after the exceptions.
        println!("pears left: {}", lib.inventory_count(fil, inv, s("pears")?)?);
        lib.inventory_free(fil, inv)?;
        Ok(())
    })
}
