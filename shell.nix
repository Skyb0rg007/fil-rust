# The development shell:
#
#   nix-shell
#   nix-shell --argstr crossSystem aarch64-unknown-linux-gnu   # cross-compiling
#
# Cross-compiled tests, examples and Fil-C compilers run through qemu-user,
# which needs binfmt_misc for the target (on NixOS,
# `boot.binfmt.emulatedSystems = [ "aarch64-linux" ];`).
{
  crossSystem ? null,
  system ? builtins.currentSystem,
}:

let
  fil-rust = import ./nix/fil-rust.nix { inherit crossSystem system; };
  inherit (fil-rust) pkgs isCross;
  inherit (pkgs) lib stdenv;
in
# A shell for the build machine: cargo's build scripts run here, and the
# target's compiler is used only where the environment says so.
pkgs.buildPackages.mkShell (
  {
    nativeBuildInputs = [
      fil-rust.toolchain
    ]
    # filcc / fil++ on PATH, when they run here.
    ++ lib.optional (!isCross) pkgs.filc
    # The target's C toolchain (FILC_TARGET_CC, the linker).
    ++ lib.optional isCross stdenv.cc;
  }
  // fil-rust.env.fil
  // fil-rust.env.cargo
)
