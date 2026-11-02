# The fil-rust workspace: builds every crate, installs the fil-rust-bindgen
# command, and (when the result can run here) runs the workspace's tests
# against the musl distribution.
{
  lib,
  stdenv,
  makeRustPlatform,
  makeBinaryWrapper,
  libclang,
  toolchain,
  filEnv,
}:

let
  rustPlatform = makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
  workspace = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "fil-rust";
  inherit (workspace.workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      # Without core dumps (from tests that crash a child process on purpose).
      (lib.fileset.fileFilter (file: !file.hasExt "core") ../crates)
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  cargoBuildFlags = [ "--workspace" ];
  cargoTestFlags = [ "--workspace" ];

  env = filEnv;

  nativeBuildInputs = [ makeBinaryWrapper ];

  postInstall = ''
    # The demo finds its C sources through the source tree; it isn't an
    # installable program.
    rm $out/bin/bindgen-demo
    # bindgen loads libclang at run time.
    wrapProgram $out/bin/fil-rust-bindgen \
      --set-default LIBCLANG_PATH ${libclang}/lib
  '';

  meta = {
    description = "Call Fil-C shared libraries from safe Rust";
    mainProgram = "fil-rust-bindgen";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
