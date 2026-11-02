# Fil-C compiler, packaged from the pre-compiled "pizfix" (musl-based)
# binary distribution: https://github.com/pizlonator/fil-c/releases
#
# This does what the distribution's setup.sh does, but with $out in place of
# $PWD and the Nix linux headers in place of /usr/include.
{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  patchelf,
  linuxHeaders,
  binutils-unwrapped,
}:

let
  version = "0.687";
  arch =
    {
      x86_64-linux = "x86_64";
      aarch64-linux = "aarch64";
    }
    .${stdenv.hostPlatform.system}
      or (throw "fil-c: unsupported system ${stdenv.hostPlatform.system}");
  hashes = {
    x86_64 = "18cd530h6nx7b7hkl9lfhkaf36bbiknsyygfc6r7m93q3x03idda";
    aarch64 = "1h6b1d28jysi7mb0bkzzjsp731g7xgm1s50jfwygjr0n0ncpfczi";
  };
in
stdenv.mkDerivation {
  pname = "filc";
  inherit version;

  src = fetchurl {
    url = "https://github.com/pizlonator/fil-c/releases/download/v${version}/filc-${version}-linux-${arch}.tar.xz";
    sha256 = hashes.${arch};
  };

  nativeBuildInputs = [
    autoPatchelfHook
    patchelf
  ];

  # The host compiler (build/bin) links against glibc and gets patched below.
  # Everything under pizfix/ is Fil-C/musl code and must be left alone, so
  # skip the automatic passes that would otherwise rewrite or strip it.
  dontConfigure = true;
  dontBuild = true;
  dontStrip = true;
  dontPatchELF = true;
  dontAutoPatchelf = true;
  dontPatchShebangs = true;

  installPhase = ''
    runHook preInstall

    mkdir -p $out
    cp -r . $out
    cd $out

    rpath=$out/pizfix/lib64:$out/pizfix/lib
    for f in \
      pizfix/lib/libc.so \
      pizfix/lib/libpizlo.so \
      pizfix/lib/libc++.so.1.0 \
      pizfix/lib/libc++abi.so.1.0 \
      pizfix/bin/minilute \
      pizfix/lib_test/libpizlo.so \
      pizfix/lib_test_gcverify/libpizlo.so \
      pizfix/lib_gcverify/libpizlo.so
    do
      patchelf --set-rpath "$rpath" "$f"
    done
    patchelf --set-interpreter "$(echo $out/pizfix/lib/ld-fil1-*.so)" pizfix/bin/minilute

    if test -f pizfix/bin/sarcasm; then
      sed -i "1s|.*|#!$out/pizfix/bin/minilute|" pizfix/bin/sarcasm
    fi

    mkdir pizfix/os-include
    ln -s ${linuxHeaders}/include/linux pizfix/os-include/linux
    ln -s ${linuxHeaders}/include/asm pizfix/os-include/asm
    ln -s ${linuxHeaders}/include/asm-generic pizfix/os-include/asm-generic

    # clang looks for `ld` in its own directory before $PATH. Pin an unwrapped
    # linker there so the Nix cc-wrapper's `ld` (which injects glibc rpaths and
    # $NIX_LDFLAGS) never gets used for Fil-C links.
    ln -s ${binutils-unwrapped}/bin/ld build/bin/ld

    mkdir -p $out/bin
    for b in clang clang++ filcc fil++; do
      ln -s $out/build/bin/$b $out/bin/$b
    done

    runHook postInstall
  '';

  postFixup = ''
    autoPatchelf $out/build/bin
  '';

  meta = {
    description = "Fil-C: memory-safe C/C++ compiler (pizfix binary distribution)";
    homepage = "https://fil-c.org/";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
