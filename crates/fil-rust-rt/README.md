# fil-rust-rt

The Fil-C runtime, in Rust, and the glibc flavor's libc.

Compiled Fil-C code calls a runtime for what it can't do inline: allocation,
pointer checks, globals, threads, exceptions, signals, system calls. The stock
runtime, libpizlo, runs a concurrent collector on its own threads and assumes
it owns the process. This one is a library, a port of libpizlo's object model,
helpers and natives:

- It starts no threads. Fil-C code runs on the thread that calls it.
- Its collector is stop-the-world, non-moving and precise, and runs only at
  safepoints, when the host lets it (`gc::set_collection`).
- For the glibc flavor, it serves the libc that libraries import (`src/glibc/`)
  as natives over the host's glibc and Rust's std, checking every argument
  before the host sees it.

`install(flavor)` loads the stubs from `fil-rust-sys` and fills their slots.
Most programs use it through [fil-rust](../fil-rust), which builds the safe
API on top. How it was built, and why, is in
[docs/RUNTIME.md](../../docs/RUNTIME.md).

`FIL_RUST_GC_STRESS=n` collects at every nth safepoint and scribbles over
swept memory; `FIL_RUST_GC_STRESS_ALLOCATIONS=1` also collects after
allocations.
