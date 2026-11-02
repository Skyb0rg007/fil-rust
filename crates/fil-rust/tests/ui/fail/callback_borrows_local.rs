// A callback can be called after the function that created it returns.
use fil_rust::Callback;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let offset = 5;
    rt.run(|fil| {
        let _cb = Callback::<fn(i32) -> i32>::new(fil, |_, (x,)| x + offset).unwrap();
    });
}
