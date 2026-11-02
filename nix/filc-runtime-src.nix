# Fil-C runtime sources (libpas) at the tag matching nix/filc.nix. The fil-rust
# shim compiles against these headers, since the binary distributions don't
# ship them and the runtime's internal layout is version-specific.
{ fetchgit }:

fetchgit {
  url = "https://github.com/pizlonator/fil-c.git";
  rev = "refs/tags/v0.687";
  sparseCheckout = [ "libpas/src/libpas" ];
  hash = "sha256-oHUR8oJPYFZz9jyTC/7AJIuStxCCg+/88ajjnuYAGvk=";
}
