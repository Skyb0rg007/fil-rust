// Fil-C memory is off limits while the thread is outside the runtime.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let p = fil.alloc::<i32>(&1).unwrap();
        fil.exited(|| p.addr());
    });
}
