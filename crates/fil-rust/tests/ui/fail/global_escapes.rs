// A pointer to a library global is scoped too.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let p = fil.scope(|inner| fil.libc().global::<i32>(inner, "errno").unwrap());
        let _ = p.read(fil);
    });
}
