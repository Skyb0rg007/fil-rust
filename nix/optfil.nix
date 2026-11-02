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
  bash,
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
    #!${bash}/bin/bash
    # Runs /opt/fil/bin/$tool with this package mounted at /opt/fil, in a root
    # that has everything else in / but not /opt (which might not exist, and
    # can't be made in a read-only root such as the Nix build sandbox's).
    args=(--tmpfs /)
    for entry in /*; do
      if [ "\$entry" = /opt ]; then continue; fi
      if [ -L "\$entry" ]; then
        args+=(--symlink "\$(readlink "\$entry")" "\$entry")
      else
        args+=(--dev-bind "\$entry" "\$entry")
      fi
    done
    exec ${bubblewrap}/bin/bwrap "\''${args[@]}" --dir /opt --bind $out/fil /opt/fil \\
      /opt/fil/bin/$tool "\$@"
    WRAPPER
      chmod +x $out/bin/$tool
    done
    runHook postInstall
  '';

  meta.platforms = [ "x86_64-linux" ];
})
