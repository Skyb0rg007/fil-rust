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

  package = pkgs.callPackage ./package.nix {
    inherit toolchain;
    # For the installed fil-rust-bindgen, which runs on the host.
    libclang = env.hostLibclang;
    filEnv = env.fil;
  };
}
