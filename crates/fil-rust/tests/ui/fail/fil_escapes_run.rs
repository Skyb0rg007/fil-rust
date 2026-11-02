// The token can't leave the job.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let mut saved = None;
    rt.run(|fil| saved = Some(fil));
    let _ = saved;
}
