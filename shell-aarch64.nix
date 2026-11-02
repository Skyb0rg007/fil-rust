# Cross-building and testing fil-rust for aarch64 Linux on an x86-64 host.
# The test binaries, and the aarch64 Fil-C compiler, run through qemu-user
# (binfmt_misc). On NixOS:
#
#   boot.binfmt.emulatedSystems = [ "aarch64-linux" ];
#
# Then: nix-shell shell-aarch64.nix --run 'cargo test'
#
# Only the musl flavor exists for aarch64 (the glibc distribution is
# x86-64 only).
let
  nixpkgs = builtins.fetchTarball {
    url = "https://github.com/nixos/nixpkgs/archive/d357a2b54f0b6dc4aeb4e993190a52cf151b6f24.tar.gz";
    sha256 = "1mqszzvb5sizl79lgkz7r44mpaxw14l1i37kcls5yd7m9qwg86af";
  };
  rust-overlay = builtins.fetchTarball {
    url = "https://github.com/oxalica/rust-overlay/archive/c85ccd343087c57da6021cafa1647142ad03e32e.tar.gz";
    sha256 = "0irpfh3g1gbhw9mcvlh38nvy1kfxbad0rgdm92ny8mvddh8f6wxw";
  };
  pkgs = import nixpkgs { overlays = [ (import rust-overlay) ]; };
  # Native aarch64 packages (run under emulation), and the cross toolchain.
  aarch64 = import nixpkgs { system = "aarch64-linux"; };
  cross = pkgs.pkgsCross.aarch64-multiplatform;
  filc = aarch64.callPackage ./nix/filc.nix { };
  filcRuntimeSrc = pkgs.callPackage ./nix/filc-runtime-src.nix { };
  crossCc = "${cross.stdenv.cc}/bin/aarch64-unknown-linux-gnu-cc";
in
pkgs.mkShell {
  nativeBuildInputs = [
    (pkgs.rust-bin.stable.latest.default.override { targets = [ "aarch64-unknown-linux-gnu" ]; })
    cross.stdenv.cc
  ];

  CARGO_BUILD_TARGET = "aarch64-unknown-linux-gnu";
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = crossCc;

  FILC_ROOT = "${filc}";
  FILC_CC = "${filc}/bin/filcc";
  FILC_CXX = "${filc}/bin/fil++";

  # fil-rust-sys: the stand-ins are built with the cross compiler, the shim
  # with clang for the aarch64 target against aarch64 headers.
  FILC_TARGET_CC = crossCc;
  FILC_RUNTIME_SRC = "${filcRuntimeSrc}/libpas/src/libpas";
  FILC_MUSL_INCLUDE = "${aarch64.musl.dev}/include";
  FILC_LINUX_INCLUDE = "${aarch64.linuxHeaders}/include";
  FILC_HOST_CLANG = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
  FILC_SHIM_LINKER = "${cross.stdenv.cc.bintools.bintools}/bin/aarch64-unknown-linux-gnu-ld";

  # fil-rust-bindgen in build scripts: host libclang, parsing for the target.
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu = "-isystem ${cross.glibc.dev}/include";
}
