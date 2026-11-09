//! Calls a Fil-C library that the build script compiled and embedded in
//! this program, through bindings it generated from the library's header
//! (see build.rs): a C dependency that stays memory-safe, with no library
//! to install, no hand-written declarations, and no `unsafe`.
//!
//!     cargo run -p bindgen-demo
//!     FILC_FLAVOR=glibc cargo run -p bindgen-demo --features glibc

use fil_rust::{Callback, Config, EmbeddedLibrary, Libc, Runtime};

include!(concat!(env!("OUT_DIR"), "/inventory.rs"));
use inventory_ffi::*;

/// c/inventory.c, built for each flavor.
static INVENTORY: EmbeddedLibrary = fil_rust::include_library!("inventory");

fn main() -> fil_rust::Result<()> {
    // With the Fil-C libc: the library is loaded inside `Runtime::run`.
    let rt = Runtime::start(Config::from_env()?.libc(Libc::Always))?;

    rt.run(|fil| -> fil_rust::Result<()> {
        let inv_lib = Inventory::load_embedded(fil, &INVENTORY)?;
        let inv = inv_lib.inv_open(fil)?;
        for (name, cat, qty, price) in
            [("hammer", CAT_TOOL, 3, 12.5), ("apple", CAT_FOOD, 40, 0.3), ("bolt", CAT_PART, 500, 0.05)]
        {
            assert_eq!(inv_lib.inv_put(fil, inv, fil.c_str(name)?, cat, qty, price)?, 0);
        }
        // C rejects a name that doesn't fit its fixed-size array.
        let long = "x".repeat(INV_MAX_NAME as usize);
        println!("put({}...) = {}", &long[..8], inv_lib.inv_put(fil, inv, fil.c_str(&long)?, CAT_TOOL, 1, 1.0)?);

        // `struct item` (with its `char name[32]`) read back from C memory.
        let hammer: item = inv_lib.inv_find(fil, inv, fil.c_str("hammer")?)?.read(fil)?;
        // (`c_char` is `i8` on x86-64 and `u8` on aarch64.)
        let name: Vec<u8> = hammer.name.iter().take_while(|&&c| c != 0).map(|&c| c.to_ne_bytes()[0]).collect();
        println!("found {} x{} at {}", String::from_utf8_lossy(&name), hammer.qty, hammer.unit_price);

        // `item_fn` is a plain `fn` type, so a Rust closure can be one.
        let cheap = Callback::<item_fn>::new(fil, |fil, (it, _ctx)| {
            let it = it.read(fil).unwrap();
            (it.unit_price < 0.1) as i32 // stop at the first cheap item
        })?;
        let visited = inv_lib.inv_each(fil, inv, cheap.get(fil), fil_rust::FilPtr::null())?;
        println!("first cheap item is #{visited}");

        // A struct returned by value.
        let r: report = inv_lib.inv_report(fil, inv)?;
        println!("{} items, {} units, worth {:.2}", r.items, r.units, r.value);
        inv_lib.inv_close(fil, inv)?;
        Ok(())
    })
}
