
# fil-rust

A Rust ffi for fil-c shared libraries.

## What is this

This crate provides a way to call functions that are defined in fil-c shared
libraries from Rust.

## Development

The dev shell (`nix-shell`, or `direnv allow` with the provided `.envrc`)
provides a Rust toolchain and the Fil-C compiler. The compiler comes from
the pre-compiled "pizfix" (musl-based) binary release and is packaged in
`nix/filc.nix`.

Inside the shell:

- `filcc` / `fil++` (also `clang` / `clang++`) are the Fil-C compilers.
- `FILC_ROOT` points at the distribution root (`build/` and `pizfix/`).
- `FILC_CC` and `FILC_CXX` point at the C and C++ compilers.

To bump Fil-C, change `version` in `nix/filc.nix` and update the hashes:

    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-x86_64.tar.xz
    nix-prefetch-url https://github.com/pizlonator/fil-c/releases/download/v<version>/filc-<version>-linux-aarch64.tar.xz
