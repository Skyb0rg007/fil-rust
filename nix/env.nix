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
in
{
  fil = {
    # The musl distribution and its compilers. When cross-compiling these are
    # the target's, and run through binfmt_misc emulation.
    FILC_ROOT = "${pkgs.filc}";
    FILC_CC = "${pkgs.filc}/bin/filcc";
    FILC_CXX = "${pkgs.filc}/bin/fil++";

    # fil-rust-sys's C shims: the runtime's sources, the target's headers, and
    # a clang that runs here.
    FILC_RUNTIME_SRC = "${pkgs.filc-runtime-src}/libpas/src/libpas";
    FILC_MUSL_INCLUDE = "${pkgs.musl.dev}/include";
    FILC_LINUX_INCLUDE = "${pkgs.linuxHeaders}/include";
    FILC_HOST_CLANG = "${buildPackages.llvmPackages.clang-unwrapped}/bin/clang";

    # libclang for fil-rust-bindgen (which runs here).
    LIBCLANG_PATH = "${buildPackages.llvmPackages.libclang.lib}/lib";
  }
  // lib.optionalAttrs host.isx86_64 {
    # The glibc distribution, for the `glibc` flavor (x86-64 only).
    FILC_GLIBC_ROOT = "${pkgs.optfil}/fil";
    FILC_GLIBC_CC = "${pkgs.optfil}/bin/filcc";
    FILC_GLIBC_CXX = "${pkgs.optfil}/bin/fil++";
    FILC_GLIBC_INCLUDE = "${pkgs.glibc.dev}/include";
  }
  // lib.optionalAttrs isCross {
    # The compat stand-ins are built with the target's C compiler, and the
    # shim with clang for the target, linked by the target's linker.
    FILC_TARGET_CC = targetCc;
    FILC_SHIM_LINKER = "${stdenv.cc.bintools.bintools}/bin/${stdenv.cc.targetPrefix}ld";
    # fil-rust-bindgen in build scripts parses headers for the target.
    "BINDGEN_EXTRA_CLANG_ARGS_${targetVar}" = "-isystem ${stdenv.cc.libc.dev}/include";
  };

  cargo = lib.optionalAttrs isCross {
    CARGO_BUILD_TARGET = rustTarget;
    "CARGO_TARGET_${lib.toUpper targetVar}_LINKER" = targetCc;
  };
}
