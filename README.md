# fil-rust

Call [Fil-C](https://fil-c.org/) shared libraries from safe Rust.

Fil-C is a memory-safe implementation of C and C++. fil-rust loads the Fil-C
runtime into an ordinary glibc-linked Rust process, opens Fil-C libraries, and
calls their functions without giving up Fil-C's guarantees. Every pointer Rust
hands to Fil-C carries a real capability, and every access Rust makes to Fil-C
memory is checked for bounds and liveness.

```rust
use std::ffi::c_char;
use fil_rust::{FilPtr, FilType, Ptr, Runtime};

#[derive(FilType, Clone, Copy)]
struct Node<'f> {
    value: i32,
    next: FilPtr<'f, Node<'static>>,
}

fil_rust::foreign! {
    struct Demo {
        fn greet(name: Ptr<c_char>) -> Ptr<c_char>;
        fn squares(n: i32) -> Ptr<Node<'static>>;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = Runtime::start_from_env()?;
    rt.run(|fil| {
        let demo = Demo::load(fil, "libdemo.so")?;
        let s = demo.greet(fil, fil.c_str("Rust")?)?;
        println!("{}", s.read_string(fil)?);

        let mut node = demo.squares(fil, 5)?;
        while !node.is_null() {
            let n = node.read(fil)?;
            println!("{}", n.value);
            node = n.next;
        }
        Ok(())
    })
}
```

## Features

- **Typed calls.** Integers, floats, pointers, structs by value (including
  large structs and `sret` returns), varargs, and function pointers, all
  described by Rust `fn` signatures. `#[derive(FilType)]` maps C structs, and
  `foreign!` declares a whole library interface.
- **Checked memory.** `FilPtr<'f, T>` is a Fil-C pointer whose lifetime is
  tied to the current scope. Reads and writes are checked copies, and
  mistakes come back as `Err`. `FilHandle` roots objects across scopes and
  threads, and `FilBuf` is Fil-C memory that Rust owns and can use as a slice.
- **Callbacks.** `Callback<fn(..) -> R>` turns a Rust closure into a C
  function pointer that Fil-C can call from any thread.
- **Threads.** Rust threads run Fil-C work on an elastic pool of Fil-C
  executor threads (`Runtime::run`, `Runtime::spawn`). Fil-C code can create
  its own threads too.
- **C++.** C++ libraries work, including `std::thread`, and exceptions that
  escape to Rust become `Error::Exception`.
- **Bindings from headers.** `fil-rust-bindgen` turns a C header into
  `FilType` structs and a `foreign!` interface, from a build script or the
  command line (see `crates/bindgen-demo`).
- **Both flavors, two architectures.** Libraries built with the musl ("pizfix") or the glibc
  ("optfil") Fil-C distribution can be loaded. Enable the `glibc` feature for
  the latter. The Rust program always stays linked to the system glibc.
  x86-64 and aarch64 (musl only) are supported.

Safety violations in Fil-C code are Fil-C panics: they end the process with a
diagnostic, just as in a pure Fil-C program. See the crate docs and
[docs/DESIGN.md](docs/DESIGN.md) for how it works and why it is safe.

## Examples

The examples build their C/C++ sources with the Fil-C compiler at run time.
Run them from the dev shell:

    cargo run --example hello      # scalars, strings, arrays, libc
    cargo run --example structs    # derive(FilType), structs by value, linked lists
    cargo run --example callbacks  # qsort with a Rust comparator, callbacks from C threads
    cargo run --example threads    # concurrent calls, background jobs, FilBuf, FilHandle
    cargo run --example cxx        # C++ objects and exceptions
    cargo run --example safety     # what errors look like, and a Fil-C panic

    FILC_FLAVOR=glibc cargo run --features glibc --example hello

`crates/bindgen-demo` is a separate crate whose build script generates its
bindings from a header with `fil-rust-bindgen`:

    cargo run -p bindgen-demo

## Generating bindings

    cargo run -p fil-rust-bindgen -- include/api.h --interface Api -o src/api.rs

or, from a build script, `fil_rust_bindgen::Builder::new("include/api.h").generate()`.
C structs become `#[derive(FilType)]` structs (their layout checked against
clang's), pointers become `Ptr<T>` / `FilPtr<'f, T>`, function-pointer
typedefs become `fn` types usable with `Callback`, opaque structs become
marker types, and functions and globals go into a `foreign!` struct.
Constructs fil-rust can't express (variadic functions, bitfields, unions and
flexible arrays by value, `long double`) are left out and listed in the
generated module's docs.

## Development

The repository is a Cargo workspace; the crates live under `crates/`
(`fil-rust`, `fil-rust-sys`, `fil-rust-macros`, `fil-rust-bindgen` and
`bindgen-demo`), and every dependency is declared once, in the top-level
`Cargo.toml`. `cargo` commands at the top level act on `fil-rust` unless told
otherwise (`--workspace`, `-p`).

The dev shell (`nix-shell`, or `direnv allow` with the provided `.envrc`)
provides a Rust toolchain, both Fil-C distributions, and everything the build
script needs:

- `FILC_ROOT`, `FILC_CC`, `FILC_CXX`: the musl distribution (`nix/filc.nix`)
  and its compilers. `filcc` / `fil++` on `PATH` are these.
- `FILC_GLIBC_ROOT`, `FILC_GLIBC_CC`, `FILC_GLIBC_CXX`: the glibc
  distribution (`nix/optfil.nix`). Its compilers expect to live at
  `/opt/fil`, so they run inside `bwrap` with the distribution mounted there.
- `FILC_RUNTIME_SRC` and the header paths that `crates/fil-rust-sys/build.rs`
  uses to compile the runtime shim.
- `LIBCLANG_PATH`, for `fil-rust-bindgen`.

Tests compile their fixtures with the compiler of the selected flavor:

    cargo test                                       # musl
    FILC_FLAVOR=glibc cargo test --features glibc    # glibc
    FUGC_MIN_THRESHOLD=0 cargo test                  # with constant garbage collection
    cargo test --workspace                           # every crate, including the generator
                                                     # (FIL_RUST_BLESS=1 updates tests/generated)

`nix-build` builds the workspace, runs its tests against the musl
distribution, and installs the `fil-rust-bindgen` command. The pinned
nixpkgs and rust-overlay live only in `nix/pkgs.nix`; `default.nix` and
`shell.nix` share the rest through `nix/fil-rust.nix`.

### Cross-compiling

fil-rust supports x86-64 and aarch64 Linux (aarch64 with the musl flavor
only; the glibc distribution is x86-64 only). Both `default.nix` and
`shell.nix` take a `crossSystem` and then use the same pinned package set,
cross-compiling to it: the Fil-C distribution, headers and C toolchain are
the target's, and the Rust toolchain, clang and libclang run on the build
machine.

    nix-shell --argstr crossSystem aarch64-unknown-linux-gnu --run 'cargo test'
    nix-build --argstr crossSystem aarch64-unknown-linux-gnu

In the shell, the tests, the examples and the target's Fil-C compiler run
through qemu-user, which needs `binfmt_misc` for the target (on NixOS,
`boot.binfmt.emulatedSystems = [ "aarch64-linux" ];`). `nix-build` skips the
tests when cross-compiling.

### Bumping Fil-C

Change `version` in `nix/filc.nix` (and `nix/optfil.nix`,
`nix/filc-runtime-src.nix`) and update the hashes:

    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-x86_64.tar.xz
    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-aarch64.tar.xz
