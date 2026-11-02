// `Ptr<T>` (FilPtr<'static, T>) is a marker for signatures; a scoped pointer
// can't become one.
use fil_rust::Ptr;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let p: Ptr<u8> = fil.alloc_bytes(1).unwrap();
        let _ = p;
    });
}
