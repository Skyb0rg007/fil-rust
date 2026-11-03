// A pointer allocated in a nested scope is released when the scope ends.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let inner = fil.scope(|inner| inner.alloc_bytes(16).unwrap());
        let _ = inner.read_vec(fil, 1);
    });
}
