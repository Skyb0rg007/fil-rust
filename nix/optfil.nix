# Fil-C's glibc-based "/opt/fil" binary distribution:
# https://github.com/pizlonator/fil-c/releases
#
# Everything in it (the compiler included) expects to live at /opt/fil, so
# the tree is unpacked as-is into $out/fil and the compiler is run through
# small wrappers that bind-mount it at /opt/fil in a private mount namespace
# (bubblewrap). fil-rust itself loads the libraries from $out/fil/lib
# directly and needs no wrapper.
{
  stdenvNoCC,
  fetchurl,
  writeShellScript,
  bubblewrap,
}:

let
  version = "0.687";
in
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "optfil";
  inherit version;

  src = fetchurl {
    url = "https://github.com/pizlonator/fil-c/releases/download/v${version}/optfil-${version}-linux-x86_64.tar.xz";
    sha256 = "1zjv0zxxa06z78pv87g7a7k7d80kgf9pl7xlq4f7qd63bmqrbhid";
  };

  dontConfigure = true;
  dontBuild = true;
  dontFixup = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    tar -xf fil.tar.xz -C $out
    for tool in filcc fil++; do
      cat > $out/bin/$tool <<WRAPPER
    #!/bin/sh
    # Runs /opt/fil/bin/$tool with this package mounted at /opt/fil.
    exec ${bubblewrap}/bin/bwrap --dev-bind / / --tmpfs /opt --bind $out/fil /opt/fil \\
      /opt/fil/bin/$tool "\$@"
    WRAPPER
      chmod +x $out/bin/$tool
    done
    runHook postInstall
  '';

  meta.platforms = [ "x86_64-linux" ];
})
