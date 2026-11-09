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
#
# The glibc flavor is x86-64 only, so on aarch64 there are only musl and
# clippy.
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
    # The musl flavor has no libc, so only libraries that use none.
    musl = {
      description = "The runtime's tests and the libc-free ones, against the musl distribution";
      run = ''
        cargo test --offline ${target} -p fil-rust-rt
        cargo test --offline ${target} -p fil-rust --lib --test rt_bare --test rt_gc --test layout --test boot --test cpu
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
    glibc = {
      description = "Every test in the workspace, against the glibc distribution";
      env.FILC_FLAVOR = "glibc";
      run = ''
        cargo test --offline ${target} -p fil-rust --features glibc
      '';
    };

    glibc-gc-stress = {
      description = "The tests with the collector running constantly and scribbling over swept memory";
      env.FILC_FLAVOR = "glibc";
      run = ''
        FIL_RUST_GC_STRESS=1 cargo test --offline ${target} -p fil-rust-rt
        FIL_RUST_GC_STRESS=1 cargo test --offline ${target} -p fil-rust --features glibc --test rt_gc --test layout
        # Every 8th: with threads, each collection waits for all of them.
        FIL_RUST_GC_STRESS=8 cargo test --offline ${target} -p fil-rust --features glibc \
          --test rt_bare --test boot --test sys --test jumps --test signals --test maps --test sockets \
          --test system --test tables --test stdfil --test cpu --test smoke --test calls --test threads \
          --test callbacks --test process --test startup --test libc_core --test libc_signals \
          --test libc_stdio --test libc_threads
        # After every allocation, for what loops through big buffers:
        # collecting every 8th safepoint, cxx and edges didn't finish in 400
        # seconds each.
        FIL_RUST_GC_STRESS_ALLOCATIONS=1 PROPTEST_CASES=1000 cargo test --offline ${target} -p fil-rust --features glibc \
          --test memory --test threads --test callbacks --test cxx --test edges --test lifecycle \
          --test misuse --test properties --test stress --test libc_threads
      '';
    };

    # The examples' libraries use the libc.
    examples = {
      description = "Runs every example and the bindgen demo";
      env.FILC_FLAVOR = "glibc";
      run = ''
        for example in crates/fil-rust/examples/*.rs; do
          example=$(basename "$example" .rs)
          echo "=== example $example"
          cargo run --offline ${target} -p fil-rust --features glibc --example "$example"
        done
        echo "=== bindgen-demo"
        cargo run --offline ${target} -p bindgen-demo --features glibc
      '';
    };
  };
in
lib.mapAttrs mkTest suites
