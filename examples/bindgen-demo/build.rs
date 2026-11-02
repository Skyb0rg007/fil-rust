// Generates Rust bindings for c/inventory.h at build time.
fn main() {
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
