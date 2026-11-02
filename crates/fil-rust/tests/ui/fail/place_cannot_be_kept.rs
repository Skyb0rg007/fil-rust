// ...or keep one past the call it was given in.
use std::cell::RefCell;
use fil_rust::Place;

thread_local! {
    static KEPT: RefCell<Option<Place<'static>>> = const { RefCell::new(None) };
}

fn keep<'f>(at: Place<'f>) {
    KEPT.with(|k| *k.borrow_mut() = Some(at));
}

fn main() {}
