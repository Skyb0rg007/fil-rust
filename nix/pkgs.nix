# The pinned package set everything else is built from: nixpkgs and
# rust-overlay are pinned here and nowhere else. Pass `crossSystem` (for
# example "aarch64-unknown-linux-gnu") to get the same set, cross-compiling
# from this machine to that system.
{
  crossSystem ? null,
  system ? builtins.currentSystem,
}:

let
  nixpkgs = builtins.fetchTarball {
    url = "https://github.com/nixos/nixpkgs/archive/d357a2b54f0b6dc4aeb4e993190a52cf151b6f24.tar.gz";
    sha256 = "1mqszzvb5sizl79lgkz7r44mpaxw14l1i37kcls5yd7m9qwg86af";
  };
  rust-overlay = builtins.fetchTarball {
    url = "https://github.com/oxalica/rust-overlay/archive/c85ccd343087c57da6021cafa1647142ad03e32e.tar.gz";
    sha256 = "0irpfh3g1gbhw9mcvlh38nvy1kfxbad0rgdm92ny8mvddh8f6wxw";
  };
in
import nixpkgs {
  localSystem = system;
  crossSystem = if crossSystem == null then system else crossSystem;
  overlays = [
    (import rust-overlay)
    (import ./overlay.nix)
  ];
}
