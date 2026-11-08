# The workspace's test suites, as derivations of their own (the package
# itself doesn't run tests):
#
#   nix-build -A tests              # all of them
#   nix-build -A tests.glibc        # one
#
# Each rebuilds the workspace from the package's sources and dependencies and
# runs one configuration of the tests. When cross-compiling, the tests (and
# the target's Fil-C compiler) run under emulation, which needs binfmt_misc
# for the target on the build machine, available in the sandbox (on NixOS,
# `boot.binfmt.emulatedSystems = [ "aarch64-linux" ];` does both):
#
#   nix-build --argstr crossSystem aarch64-unknown-linux-gnu -A tests.musl
{
  lib,
  stdenv,
  package,
}:

let
  host = stdenv.hostPlatform;
  isCross = !stdenv.buildPlatform.canExecute host;
  # For cargo commands run by hand; cargoCheckHook passes it itself.
  target = lib.optionalString isCross "--target ${host.rust.rustcTarget}";

  # `package`, running tests (`run`, or `cargo test` with `flags`) instead of
  # building and installing. They run in the install phase rather than the
  # check phase, which stdenv skips when cross-compiling, even with emulation.
  mkTest =
    name:
    {
      description,
      flags ? [ "--workspace" ],
      env ? { },
      run ? "cargoCheckHook",
    }:
    package.overrideAttrs (old: {
      pname = "fil-rust-test-${name}";
      doCheck = false;
      dontCargoBuild = true;
      # Debug builds, as `cargo test` makes them: with debug assertions.
      cargoCheckType = "debug";
      cargoTestFlags = flags;
      env = old.env // env;
      dontCargoInstall = true;
      installPhase = ''
        runHook preInstall
        ${run}
        touch $out
        runHook postInstall
      '';
      postInstall = "";
      dontFixup = true;
      passthru = { };
      meta = old.meta // {
        inherit description;
      };
    });

  # The configurations, by name.
  suites = {
    musl = {
      description = "Every test in the workspace, against the musl distribution";
    };

    gc-stress = {
      description = "The memory, concurrency and property tests with the collector running constantly";
      flags = [
        "-p"
        "fil-rust"
      ]
      ++ lib.concatMap
        (t: [
          "--test"
          t
        ])
        [
          "memory"
          "threads"
          "callbacks"
          "cxx"
          "edges"
          "lifecycle"
          "misuse"
          "properties"
          "stress"
        ];
      env = {
        # Collect after every allocation, and scribble over freed memory.
        FUGC_MIN_THRESHOLD = "0";
        FUGC_SCRIBBLE = "1";
        FIL_RUST_STRESS = "8";
        PROPTEST_CASES = "1000";
      };
    };

    examples = {
      description = "Runs every example and the bindgen demo";
      run = ''
        for example in crates/fil-rust/examples/*.rs; do
          example=$(basename "$example" .rs)
          echo "=== example $example"
          cargo run --offline ${target} --example "$example"
        done
        echo "=== bindgen-demo"
        cargo run --offline ${target} -p bindgen-demo
      '';
    };

    clippy = {
      description = "Clippy on every crate and target, with warnings as errors";
      run = ''
        cargo clippy --offline ${target} --workspace --all-targets \
          ${lib.optionalString host.isx86_64 "--features fil-rust/glibc"} -- -D warnings
      '';
    };
  }
  // lib.optionalAttrs host.isx86_64 {
    rust-rt = {
      description = "The Rust runtime's tests, and what's neutral about the backends on both";
      run = ''
        cargo test --offline ${target} -p fil-rust-rt
        FIL_RUST_RUNTIME=rust cargo test --offline ${target} -p fil-rust \
          --test rt_bare --test rt_gc --test rt_libc --test sys --test layout --test boot --test edges \
          --test jumps --test cxx --test signals --test maps --test sockets --test system \
          --test smoke --test calls --test memory --test threads --test lifecycle --test stress \
          --test startup --test callbacks --test embedded --test process --test properties \
          --test misuse --test bindgen --test ui --test abi_audit
        FIL_RUST_RUNTIME=pizlo cargo test --offline ${target} -p fil-rust --test rt_bare --test sys --test boot --test jumps
      '';
    };

    rust-rt-gc-stress = {
      description = "The same with the Rust collector running constantly and scribbling over swept memory";
      run = ''
        FIL_RUST_GC_STRESS=1 cargo test --offline ${target} -p fil-rust-rt
        FIL_RUST_GC_STRESS=1 cargo test --offline ${target} -p fil-rust --test rt_gc --test layout
        # Every 8th: with threads, each collection waits for all of them.
        FIL_RUST_GC_STRESS=8 cargo test --offline ${target} -p fil-rust --test rt_bare --test rt_libc --test sys --test jumps --test signals --test maps --test sockets --test system \
          --test smoke --test calls --test memory --test threads --test callbacks --test properties \
          --test process --test startup
      '';
    };

    glibc = {
      description = "fil-rust's tests against the glibc distribution";
      flags = [
        "-p"
        "fil-rust"
        "--features"
        "glibc"
      ];
      env.FILC_FLAVOR = "glibc";
    };
  };
in
lib.mapAttrs mkTest suites
