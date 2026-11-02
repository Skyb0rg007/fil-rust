// The runtime's raw pointer type (whose capability field is public) isn't
// accepted where a Place is expected.
use fil_rust::FilType;
use fil_rust_sys::RawPtr;

fn main() {
    let rt = fil_rust::Runtime::get().unwrap();
    rt.run(|fil| {
        let forged = RawPtr { ptr: 0x1000 as *mut _, lower: 0x1000 as *mut _ };
        let _ = <u64 as FilType>::read(fil, forged);
    });
}
