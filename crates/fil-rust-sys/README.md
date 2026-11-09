# fil-rust-sys

The native libraries Fil-C libraries bind to by name, built by `build.rs` and
embedded with `include_bytes!`, so nothing has to be installed beside the
program:

- the **runtime stub**, `libpizlo.so`: one jump slot per helper and native,
  generated from `abi/runtime_symbols_<flavor>.txt`;
- the **libc stand-ins**: for glibc, `libc.so.6666`, `libm.so.6666`,
  `libpthread.so.6660` and `libdl.so.6662`, with a slot for every export and
  version of the distribution's (`abi/glibc_*.txt`); for musl, a soname only;
- **compat** libraries with the sonames and version nodes of the
  distribution's yolo libc;
- the **support library**, Fil-C's own `filc/src` (vendored in `csrc/filc/`),
  compiled with Fil-C.

[fil-rust-rt](../fil-rust-rt) loads them from memfds and fills the slots.

When the pinned Fil-C version changes, regenerate the symbol lists and the
vendored source from a Fil-C checkout:

    abi/update-symbols.sh <checkout>
    csrc/filc/update.sh <checkout> v<version>

Features `musl` and `glibc` choose which flavors are built.
