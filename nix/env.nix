# The environment fil-rust's build scripts and tests read, for a package set
# whose host platform is the one being built for. `fil` is what every build
# needs; `cargo` points a plain `cargo` invocation (in the dev shell) at the
# host platform when cross-compiling, which buildRustPackage does by itself.
pkgs:

let
  inherit (pkgs) lib stdenv buildPackages;
  host = stdenv.hostPlatform;
  isCross = stdenv.buildPlatform.config != host.config;
  rustTarget = host.rust.rustcTarget;
  targetVar = lib.replaceStrings [ "-" ] [ "_" ] rustTarget;
  targetCc = "${stdenv.cc}/bin/${stdenv.cc.targetPrefix}cc";

  # libclang for programs that run on the host and use fil-rust-bindgen. When
  # cross-compiling, it comes from the same nixpkgs built natively for the
  # host (which the binary cache has), rather than cross-compiling clang.
  hostLibclang =
    if isCross then
      (import pkgs.path { localSystem = host.system; }).llvmPackages.libclang.lib
    else
      pkgs.llvmPackages.libclang.lib;
in
{
  inherit hostLibclang;

  fil = {
    # The musl distribution and its compilers. When cross-compiling these are
    # the target's, and run through binfmt_misc emulation.
    FILC_ROOT = "${pkgs.filc}";
    FILC_CC = "${pkgs.filc}/bin/filcc";
    FILC_CXX = "${pkgs.filc}/bin/fil++";

    # musl's and Linux's headers, for the audit of what the musl flavor's
    # natives pass to the host glibc (tests/abi_audit.rs).
    FILC_MUSL_INCLUDE = "${pkgs.musl.dev}/include";
    FILC_LINUX_INCLUDE = "${pkgs.linuxHeaders}/include";

    # libclang for fil-rust-bindgen (which runs here).
    LIBCLANG_PATH = "${buildPackages.llvmPackages.libclang.lib}/lib";
  }
  // lib.optionalAttrs host.isx86_64 {
    # The glibc distribution, for the `glibc` flavor (x86-64 only).
    FILC_GLIBC_ROOT = "${pkgs.optfil}/fil";
    FILC_GLIBC_CC = "${pkgs.optfil}/bin/filcc";
    FILC_GLIBC_CXX = "${pkgs.optfil}/bin/fil++";
  }
  // lib.optionalAttrs isCross {
    # The tests run under qemu-user, whose emulation of fork (and of the
    # vfork-style clone behind posix_spawn) stops every emulated thread and
    # can freeze or crash a process with a busy Fil-C runtime; the test that
    # starts processes from such a process checks this and skips.
    FIL_RUST_EMULATED = "1";
    # The stand-ins are built with the target's C compiler.
    FILC_TARGET_CC = targetCc;
    # fil-rust-bindgen in build scripts parses headers for the target.
    "BINDGEN_EXTRA_CLANG_ARGS_${targetVar}" = "-isystem ${stdenv.cc.libc.dev}/include";
    # Build scripts run here and load this machine's libclang (above); the
    # target's executables (fil-rust-bindgen's tests) need the host's.
    # Cargo runs only target executables through the runner.
    "CARGO_TARGET_${lib.toUpper targetVar}_RUNNER" = "${buildPackages.writeShellScript "with-host-libclang" ''
      LIBCLANG_PATH=${hostLibclang}/lib exec "$@"
    ''}";
  };

  cargo = lib.optionalAttrs isCross {
    CARGO_BUILD_TARGET = rustTarget;
    "CARGO_TARGET_${lib.toUpper targetVar}_LINKER" = targetCc;
  };
}
