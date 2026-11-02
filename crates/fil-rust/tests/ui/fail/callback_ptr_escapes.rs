// The function pointer of a Callback is scoped like any other pointer.
use fil_rust::Callback;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let cb = rt.run(|fil| Callback::<fn(i32) -> i32>::new(fil, |_, (x,)| x).unwrap());
    rt.run(|fil| {
        let f = fil.scope(|inner| cb.get(inner));
        let _ = f.call(fil, (1,));
    });
}
