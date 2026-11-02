// Stashing a nested scope's pointer in a collection of the outer scope.
use fil_rust::FilPtr;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let mut kept: Vec<FilPtr<'_, u8>> = vec![fil.alloc_bytes(1).unwrap()];
        fil.scope(|inner| kept.push(inner.alloc_bytes(1).unwrap()));
        let _ = kept;
    });
}
