# The Fil-C distributions, added to the package set. They're relative to the
# set's host platform, so a cross package set (see pkgs.nix) gets the
# target's. Only derivations go here: in a cross package set, other values
# (like attribute sets) get merged with the build machine's by splicing.
final: prev:

{
  # The musl ("pizfix") Fil-C distribution.
  filc = final.callPackage ./filc.nix { };

  # The glibc ("/opt/fil") Fil-C distribution, which exists for x86-64 only.
  # (Not null elsewhere: splicing would fill a null in with the build
  # machine's package. env.nix checks the platform.)
  optfil = final.callPackage ./optfil.nix { };

  # The Fil-C runtime sources the shim is compiled against.
  filc-runtime-src = final.callPackage ./filc-runtime-src.nix { };
}
