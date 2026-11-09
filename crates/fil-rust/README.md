# fil-rust

Call [Fil-C](https://fil-c.org/) shared libraries from safe Rust.

`Runtime::start` loads the Fil-C runtime (a Rust library, `fil-rust-rt`) into
the process. `Runtime::run` gives the calling thread a `Fil` token, and
through it you load libraries, call their functions with typed signatures,
and read and write Fil-C memory with checked copies.

```rust
use std::ffi::c_char;
use fil_rust::{Ptr, Runtime};

fil_rust::foreign! {
    struct Demo {
        fn greet(name: Ptr<c_char>) -> Ptr<c_char>;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = Runtime::start_from_env()?;
    rt.run(|fil| {
        let demo = Demo::load(fil, "libdemo.so")?;
        let s = demo.greet(fil, fil.c_str("Rust")?)?;
        println!("{}", s.read_string(fil)?);
        Ok(())
    })
}
```

- `FilPtr<'f, T>` is a pointer tied to the current scope; `FilHandle` keeps an
  object alive across scopes and threads; `FilBuf` is Fil-C memory Rust owns.
- `#[derive(FilType)]` maps C structs, `foreign!` declares a library's
  interface, and `Callback` turns a closure into a C function pointer.
- C++ exceptions that escape to Rust become `Error::Exception`.

Features: `glibc` (libraries built with Fil-C's glibc distribution, which can
use libc; x86-64), `musl` (default; libraries that use no libc), `derive`
(default). Select the flavor at run time with `FILC_FLAVOR` and the
distribution's root (`FILC_GLIBC_ROOT` or `FILC_ROOT`).

See the [workspace README](../../README.md) and
[docs/DESIGN.md](../../docs/DESIGN.md).
