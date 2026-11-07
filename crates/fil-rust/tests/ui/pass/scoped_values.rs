// What the compile-fail cases do wrong, done right: values that can leave a
// scope do.
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use fil_rust::{Callback, Ptr};

fn main() {
    let rt = fil_rust::Runtime::start_from_env().unwrap();

    // Plain data leaves a job; pointers leave it rooted, as handles.
    let n: usize = rt.run(|fil| fil.c_str("data").unwrap().read_string(fil).unwrap().len());
    let handle = rt.run(|fil| fil.root(fil.c_str("rooted").unwrap()).unwrap());
    std::thread::spawn(move || {
        let rt = fil_rust::Runtime::get().unwrap();
        rt.run(|fil| handle.get(fil).read_string(fil).unwrap());
    });

    // A nested scope can use the outer scope's pointers (with the outer
    // token), and return plain data or an outer pointer.
    rt.run(|fil| {
        let outer = fil.alloc::<i32>(&n.try_into().unwrap()).unwrap();
        let same = fil.scope(|inner| {
            let temp = inner.alloc::<i32>(&outer.read(fil).unwrap()).unwrap();
            assert_eq!(temp.read(inner).unwrap(), outer.read(fil).unwrap());
            outer
        });
        same.read(fil).unwrap();
        // While exited, code can block and use owned Rust data.
        let owned = String::from("owned");
        fil.exited(move || owned.len());
    });

    // Library functions outlive scopes and threads.
    let libc = rt.libc();
    let strlen = rt.run(|fil| libc.function::<fn(Ptr<c_char>) -> usize>(fil, "strlen").unwrap());
    std::thread::spawn(move || {
        let rt = fil_rust::Runtime::get().unwrap();
        rt.run(|fil| strlen.call(fil, (fil.c_str("x").unwrap(),)).unwrap());
    });

    // Callbacks capture shared, thread-safe state.
    let calls = Arc::new(Mutex::new(0));
    let cb = rt.run(|fil| {
        let calls = calls.clone();
        Callback::<fn(i32) -> i32>::new(fil, move |_, (x,)| {
            *calls.lock().unwrap() += 1;
            x
        })
        .unwrap()
    });
    drop(cb);

    // Background jobs own what they use and return owned results.
    let name = String::from("job");
    let job = rt.spawn(move |fil| fil.c_str(&name).unwrap().read_string(fil).unwrap());
    let _: String = job.join();

    // FilBufs are plain Rust-owned bytes until shared.
    let mut buf = rt.run(|fil| fil.buf(4).unwrap());
    buf[0] = 1;
    let buf = std::thread::spawn(move || buf).join().unwrap();
    rt.run(move |fil| buf.share(fil).read(fil).unwrap());
}
