// A handle roots its object, but the pointer `get` returns is still scoped.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let handle = rt.run(|fil| fil.root(fil.alloc_bytes(1).unwrap()).unwrap());
    rt.run(|fil| {
        let p = fil.scope(|inner| handle.get(inner));
        let _ = p.read(fil);
    });
}
