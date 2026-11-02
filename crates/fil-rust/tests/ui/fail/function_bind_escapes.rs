// `Function` outlives scopes, but `bind` gives a scoped function pointer.
use fil_rust::Ptr;
use std::ffi::c_char;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let strlen = rt.run(|fil| fil.libc().function::<fn(Ptr<c_char>) -> usize>(fil, "strlen").unwrap());
    rt.run(|fil| {
        let f = fil.scope(|inner| strlen.bind(inner));
        let _ = f.is_null();
    });
}
