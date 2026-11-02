mod common;

use fil_rust::Ptr;
use std::ffi::c_char;

#[test]
fn smoke() {
    let rt = common::runtime();
    let lib = rt.load(common::fixture("basic.c")).unwrap();
    let (sum, len, up) = rt.run(|fil| {
        let add = lib.function::<fn(i32, i32) -> i32>(fil, "add").unwrap();
        let strlen = lib.function::<fn(Ptr<c_char>) -> usize>(fil, "my_strlen").unwrap();
        let upper = lib.function::<fn(Ptr<c_char>) -> Ptr<c_char>>(fil, "dup_upper").unwrap();
        let s = fil.c_str("hello").unwrap();
        (
            add.call(fil, (2, 3)).unwrap(),
            strlen.call(fil, (s,)).unwrap(),
            upper.call(fil, (s,)).unwrap().read_string(fil).unwrap(),
        )
    });
    assert_eq!((sum, len, up.as_str()), (5, 5, "HELLO"));
}
