//! Rust closures as C function pointers: a comparator for `qsort`, and event
//! handlers that C calls from its own threads.
//!
//!     cargo run --example callbacks

mod support;

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use fil_rust::{Callback, Ptr};

type StrCmp = fn(Ptr<c_char>, Ptr<c_char>) -> i32;
type Handler = fn(i32, i32);

fil_rust::foreign! {
    struct Events {
        fn sort_strings(items: Ptr<Ptr<c_char>>, n: usize, cmp: StrCmp);
        fn subscribe(h: Handler) -> i32;
        fn unsubscribe_all();
        fn dispatch(workers: i32, total: i32);
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = support::runtime();
    let path = support::build("events.c");
    let events = rt.run(|fil| Events::load(fil, &path))?;

    // Sort strings with a Rust comparator: by length, then alphabetically.
    let sorted = rt.run(|fil| -> fil_rust::Result<Vec<String>> {
        let words = ["pear", "fig", "banana", "kiwi", "apple", "date"];
        let ptrs = words.iter().map(|w| fil.c_str(w)).collect::<fil_rust::Result<Vec<_>>>()?;
        let array = fil.alloc_array::<Ptr<c_char>>(&ptrs)?;

        // The closure gets its own `Fil` for the callback's scope, and reads
        // the C strings through checked pointers.
        let by_len = Callback::<StrCmp>::new(fil, |fil, (a, b)| {
            let (a, b) = (a.read_string(fil).unwrap(), b.read_string(fil).unwrap());
            (a.len(), &a).cmp(&(b.len(), &b)) as i32
        })?;
        events.sort_strings(fil, array, words.len(), by_len.get(fil))?;

        array.read_array(fil, words.len())?.into_iter().map(|p| p.read_string(fil)).collect()
    })?;
    println!("sorted: {sorted:?}");

    // Handlers that C calls concurrently from four of its own threads. The
    // closures must be `Send + Sync`; shared state goes behind a lock.
    let per_worker = Arc::new(Mutex::new(BTreeMap::<i32, usize>::new()));
    let sum = Arc::new(Mutex::new(0i64));
    rt.run(|fil| -> fil_rust::Result<()> {
        let counts = per_worker.clone();
        let count = Callback::<Handler>::new(fil, move |_, (worker, _event)| {
            *counts.lock().unwrap().entry(worker).or_default() += 1;
        })?;
        let total = sum.clone();
        let add = fil.callback::<Handler, _>(move |_, (_worker, event)| {
            *total.lock().unwrap() += event as i64;
        })?;
        events.subscribe(fil, count.get(fil))?;
        events.subscribe(fil, add.get(fil))?;

        // `dispatch` joins its threads before returning, so the callbacks
        // are done by the time we drop them.
        events.dispatch(fil, 4, 10_000)?;
        events.unsubscribe_all(fil)?;
        Ok(())
    })?;
    println!("events per C thread: {:?}", per_worker.lock().unwrap());
    println!("sum of event ids: {} (expected {})", sum.lock().unwrap(), (0..10_000i64).sum::<i64>());
    Ok(())
}
