let
  nixpkgs = builtins.fetchTarball {
    url = "https://github.com/nixos/nixpkgs/archive/d357a2b54f0b6dc4aeb4e993190a52cf151b6f24.tar.gz";
    sha256 = "1mqszzvb5sizl79lgkz7r44mpaxw14l1i37kcls5yd7m9qwg86af";
  };
  rust-overlay = builtins.fetchTarball {
    url = "https://github.com/oxalica/rust-overlay/archive/c85ccd343087c57da6021cafa1647142ad03e32e.tar.gz";
    sha256 = "0irpfh3g1gbhw9mcvlh38nvy1kfxbad0rgdm92ny8mvddh8f6wxw";
  };
  pkgs = import nixpkgs {
    overlays = [ (import rust-overlay) ];
  };
  filc = pkgs.callPackage ./nix/filc.nix { };
in
pkgs.mkShell {
  nativeBuildInputs = [
    pkgs.rust-bin.stable.latest.default
    filc
  ];

  # Root of the Fil-C distribution (contains build/ and pizfix/).
  FILC_ROOT = "${filc}";
  FILC_CC = "${filc}/bin/filcc";
  FILC_CXX = "${filc}/bin/fil++";
}
