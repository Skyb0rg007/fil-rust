// Arguments are checked against the declared signature.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let abs = fil.libc().function::<fn(i32) -> i32>(fil, "abs").unwrap();
        let _ = abs.call(fil, ("seven",));
    });
}
