//! Calls a Fil-C library through bindings that fil-rust-bindgen generated
//! from its header at build time (see build.rs). No hand-written
//! declarations, and no `unsafe`.
//!
//!     cargo run -p bindgen-demo
//!     FILC_FLAVOR=glibc cargo run -p bindgen-demo --features glibc

use std::path::Path;
use std::process::Command;

use fil_rust::{Callback, Config, Flavor, Runtime};

include!(concat!(env!("OUT_DIR"), "/inventory.rs"));
use inventory_ffi::*;

/// Compiles c/inventory.c with the Fil-C compiler of the configured flavor.
fn build_library(flavor: Flavor) -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let var = if flavor == Flavor::Glibc { "FILC_GLIBC_CC" } else { "FILC_CC" };
    let cc = std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set (run in nix-shell)"));
    let out = dir.join("../../target/example-libs").join(format!("{flavor:?}").to_lowercase());
    std::fs::create_dir_all(&out).unwrap();
    let lib = out.join("libinventory.so");
    let ok = Command::new(cc)
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&lib)
        .arg(dir.join("c/inventory.c"))
        .status()
        .expect("running the Fil-C compiler")
        .success();
    assert!(ok, "compiling inventory.c failed");
    lib
}

fn main() -> fil_rust::Result<()> {
    let rt = Runtime::start(Config::from_env()?)?;
    let path = build_library(rt.flavor());

    rt.run(|fil| -> fil_rust::Result<()> {
        let inv_lib = Inventory::load(fil, &path)?;
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
