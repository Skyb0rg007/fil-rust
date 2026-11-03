// Compiles c/inventory.c with Fil-C (for each flavor with a compiler) and
// generates Rust bindings for c/inventory.h. The library is embedded in the
// program with `fil_rust::include_library!`.
fn main() {
    fil_rust_build::Build::new().file("c/inventory.c").compile("inventory");

    println!("cargo:rerun-if-changed=c/inventory.h");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bindings = fil_rust_bindgen::Builder::new("c/inventory.h")
        .interface_name("Inventory")
        .generate()
        .expect("generating bindings for c/inventory.h");
    for skipped in bindings.skipped() {
        println!("cargo:warning=fil-rust-bindgen skipped {skipped}");
    }
    bindings.write_to_file(out.join("inventory.rs")).unwrap();
}
