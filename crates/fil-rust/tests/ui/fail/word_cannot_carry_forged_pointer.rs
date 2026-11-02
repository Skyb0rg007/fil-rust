// A calling-convention word holding a pointer can only come from the crate.
use fil_rust::{FilPtr, FilType, Word};

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let word = Word::from_raw(0x1000);
        let _ = <FilPtr<'static, u8> as FilType>::from_word(fil, word);
    });
}
