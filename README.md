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
  command line.
- **C dependencies built in.** `fil-rust-build` compiles C and C++ with Fil-C
  from a build script, and `include_library!` embeds the result in the
  program, so a C dependency ships inside the binary and stays memory-safe
  (see `crates/bindgen-demo`).
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

`crates/bindgen-demo` is a separate crate whose build script compiles its C
library with `fil-rust-build` and generates the bindings from its header
with `fil-rust-bindgen`; the program embeds the library, so it needs only the
Fil-C distribution's runtime to run:

    cargo run -p bindgen-demo

## Building C into the program

A build script compiles the C (once per Fil-C flavor whose compiler it
finds, see the `fil-rust-build` docs):

```rust
// build.rs
fn main() {
    fil_rust_build::Build::new()
        .files(["c/zap.c", "c/zap_util.c"])
        .include("c")
        .define("ZAP_FAST", None)
        .compile("zap");
}
```

and the program embeds `libzap.so` and loads it from memory, with the same
checks as a library loaded from a file:

```rust
static ZAP: fil_rust::EmbeddedLibrary = fil_rust::include_library!("zap");

let zap = rt.load_embedded(&ZAP)?;
```

Libraries built this way can link against each other (`Build::link_with`);
load the dependency first.

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
(`fil-rust`, `fil-rust-sys`, `fil-rust-macros`, `fil-rust-bindgen`,
`fil-rust-build` and `bindgen-demo`), and every dependency is declared once, in the top-level
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

`PROPTEST_CASES` sets how many cases each property test tries, and
`FIL_RUST_STRESS` multiplies the stress tests' iterations. The compile-fail
tests (`tests/ui`) compare compiler messages; after a toolchain bump,
`TRYBUILD=overwrite cargo test --test ui` regenerates them for review.

`nix-build` builds the workspace and installs the `fil-rust-bindgen` command.
The tests are separate derivations in `passthru.tests` (`nix/tests.nix`):

    nix-build -A tests              # all of them
    nix-build -A tests.musl         # the whole workspace, musl flavor
    nix-build -A tests.glibc        # fil-rust against the glibc flavor (x86-64)
    nix-build -A tests.gc-stress    # memory/thread/property/stress tests, constant GC
    nix-build -A tests.examples     # runs every example and bindgen-demo
    nix-build -A tests.clippy       # clippy, warnings as errors

The pinned
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
`boot.binfmt.emulatedSystems = [ "aarch64-linux" ];`). The Nix test suites
work the same way, so they need that emulation inside the build sandbox
(which the NixOS option also sets up):

    nix-build --argstr crossSystem aarch64-unknown-linux-gnu -A tests.musl

### Bumping Fil-C

Change `version` in `nix/filc.nix` (and `nix/optfil.nix`,
`nix/filc-runtime-src.nix`) and update the hashes:

    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-x86_64.tar.xz
    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-aarch64.tar.xz
