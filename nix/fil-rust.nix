# What default.nix and shell.nix share: the pinned package set (cross-
# compiling to `crossSystem`, if given), the Rust toolchain, the build
# environment, and the workspace package.
{
  crossSystem ? null,
  system ? builtins.currentSystem,
}:

let
  pkgs = import ./pkgs.nix { inherit crossSystem system; };
  inherit (pkgs) stdenv;
in
rec {
  inherit pkgs;

  isCross = stdenv.buildPlatform.config != stdenv.hostPlatform.config;

  # Runs on the build machine, targets the host platform.
  toolchain = pkgs.buildPackages.rust-bin.stable.latest.default.override {
    targets = [ stdenv.hostPlatform.rust.rustcTarget ];
  };

  env = import ./env.nix pkgs;

  # libclang for the installed fil-rust-bindgen, which runs on the host. When
  # cross-compiling, it comes from the same nixpkgs built natively for the
  # host (which the binary cache has), rather than cross-compiling clang.
  hostLibclang =
    if isCross then
      (import pkgs.path { localSystem = stdenv.hostPlatform.system; }).llvmPackages.libclang.lib
    else
      pkgs.llvmPackages.libclang.lib;

  package = pkgs.callPackage ./package.nix {
    inherit toolchain;
    libclang = hostLibclang;
    filEnv = env.fil;
  };
}
