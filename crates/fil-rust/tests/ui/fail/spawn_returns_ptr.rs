// A background job can't hand a scoped pointer to its joiner.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let job = rt.spawn(|fil| fil.alloc_bytes(16).unwrap());
    let _ = job.join();
}
