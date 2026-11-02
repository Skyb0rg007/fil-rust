// Only types with a Fil-C representation can appear in signatures.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let f = fil.libc().function::<fn(String) -> i32>(fil, "puts").unwrap();
        let _ = f;
    });
}
