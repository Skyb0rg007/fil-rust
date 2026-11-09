Fil-C's `filc/src` at `v0.687`, unmodified, copied by `update.sh`: the part of
the runtime that's Fil-C code (exception unwinding, the personality routine,
`zsnprintf` and friends, and the system-call wrappers built on the `z*`
natives). Each file states its license: BSD-style for the Fil-C files and for
`syscall.h`'s musl parts (MIT), and Patrick Powell's notice for `snprintf.c`.

`build.rs` compiles it with the glibc distribution's `filcc` into
`libfilrust_support.so`. It imports only helpers, `z*` natives and `memset`, so
it needs neither libpizlo nor a libc. The musl flavor links the same objects out
of the distribution's `libpizlo.a` instead.
