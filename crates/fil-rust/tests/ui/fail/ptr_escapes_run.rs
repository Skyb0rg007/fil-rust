// A pointer can't outlive the job that allocated it: after `run` returns,
// nothing keeps the object alive.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let leaked = rt.run(|fil| fil.alloc::<i32>(&1).unwrap());
    let _ = leaked;
}
