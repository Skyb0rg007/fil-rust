// A background job may outlive the caller's stack frame.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let name = String::from("local");
    let job = rt.spawn(|fil| fil.c_str(&name).unwrap().addr());
    drop(name);
    let _ = job.join();
}
