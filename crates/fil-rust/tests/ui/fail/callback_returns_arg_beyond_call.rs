// Pointers a callback receives are valid only during the call.
use std::cell::RefCell;
use fil_rust::{Callback, FilPtr, Ptr};

thread_local! {
    static SEEN: RefCell<Option<FilPtr<'static, u8>>> = const { RefCell::new(None) };
}

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let _cb = Callback::<fn(Ptr<u8>)>::new(fil, |_, (p,)| {
            SEEN.with(|s| *s.borrow_mut() = Some(p));
        })
        .unwrap();
    });
}
