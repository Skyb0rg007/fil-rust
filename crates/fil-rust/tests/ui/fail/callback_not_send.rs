// Fil-C may call a callback from any of its threads.
use std::rc::Rc;
use fil_rust::Callback;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let counter = Rc::new(0);
        let _cb = Callback::<fn(i32) -> i32>::new(fil, move |_, (x,)| x + *counter).unwrap();
    });
}
