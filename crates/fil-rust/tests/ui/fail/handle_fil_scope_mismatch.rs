// `Fil::root` takes a pointer of the token's own scope.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|outer| {
        let p = outer.scope(|inner| inner.alloc_bytes(1).unwrap());
        let _ = outer.root(p);
    });
}
