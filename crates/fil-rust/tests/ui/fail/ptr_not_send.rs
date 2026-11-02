// A FilPtr is tied to its Fil-C thread's scope: it can't move to another thread.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let p = fil.alloc::<i32>(&1).unwrap();
        std::thread::scope(|s| {
            s.spawn(move || p.addr());
        });
    });
}
