// A pointer word is scoped like the pointer it came from.
use fil_rust::{FilPtr, FilType};

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let word = fil.scope(|inner| {
            let p = inner.alloc_bytes(1).unwrap();
            <FilPtr<'static, u8> as FilType>::to_word(&p).unwrap()
        });
        let _ = <FilPtr<'static, u8> as FilType>::from_word(fil, word);
    });
}
