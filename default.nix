# Builds the fil-rust workspace:
#
#   nix-build
#   nix-build --argstr crossSystem aarch64-unknown-linux-gnu   # cross-compiled
#
# The pinned package set is exposed too, for the Fil-C distributions:
#
#   nix-build -A pkgs.filc
{
  crossSystem ? null,
  system ? builtins.currentSystem,
}:

let
  fil-rust = import ./nix/fil-rust.nix { inherit crossSystem system; };
in
fil-rust.package // { inherit (fil-rust) pkgs; }
