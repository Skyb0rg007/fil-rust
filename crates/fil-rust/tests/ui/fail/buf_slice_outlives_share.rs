// Nor can a slice borrowed from it survive the share.
fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    let buf = rt.run(|fil| fil.buf(8).unwrap());
    let bytes: &[u8] = &buf;
    rt.run(move |fil| {
        let _shared = buf.share(fil);
    });
    let _ = bytes[0];
}
