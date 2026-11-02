// The Fil token proves the *current* thread is inside the runtime.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        std::thread::scope(|s| {
            s.spawn(|| fil.pollcheck());
        });
    });
}
