// The token can't be used while exited.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        fil.exited(|| fil.pollcheck());
    });
}
