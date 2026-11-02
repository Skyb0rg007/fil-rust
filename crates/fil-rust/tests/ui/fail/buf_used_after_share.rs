// Sharing a FilBuf with Fil-C gives up Rust's exclusive access to it.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let mut buf = rt.run(|fil| fil.buf(8).unwrap());
    rt.run(|fil| {
        let _shared = buf.share(fil);
    });
    buf[0] = 1;
}
