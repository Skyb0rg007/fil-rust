# The workspace's test suites, as derivations of their own (the package
# itself doesn't run tests):
#
#   nix-build -A tests              # all of them
#   nix-build -A tests.glibc        # one
#
# Each rebuilds the workspace from the package's sources and dependencies and
# runs one configuration of the tests. They exist only when the build machine
# can run what's built (not when cross-compiling).
{
  lib,
  stdenv,
  package,
}:

let
  host = stdenv.hostPlatform;

  # `package`, running `checkPhase` instead of building and installing.
  mkTest =
    name:
    {
      description,
      flags ? [ "--workspace" ],
      env ? { },
      checkPhase ? null,
    }:
    package.overrideAttrs (
      old:
      {
        pname = "fil-rust-test-${name}";
        doCheck = true;
        dontCargoBuild = true;
        # Debug builds, as `cargo test` makes them: with debug assertions.
        cargoCheckType = "debug";
        cargoTestFlags = flags;
        env = old.env // env;
        dontCargoInstall = true;
        installPhase = "touch $out";
        postInstall = "";
        dontFixup = true;
        passthru = { };
        meta = old.meta // {
          inherit description;
        };
      }
      // lib.optionalAttrs (checkPhase != null) { inherit checkPhase; }
    );

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
      checkPhase = ''
        runHook preCheck
        for example in crates/fil-rust/examples/*.rs; do
          example=$(basename "$example" .rs)
          echo "=== example $example"
          cargo run --offline --example "$example"
        done
        echo "=== bindgen-demo"
        cargo run --offline -p bindgen-demo
        runHook postCheck
      '';
    };

    clippy = {
      description = "Clippy on every crate and target, with warnings as errors";
      checkPhase = ''
        runHook preCheck
        cargo clippy --offline --workspace --all-targets \
          ${lib.optionalString host.isx86_64 "--features fil-rust/glibc"} -- -D warnings
        runHook postCheck
      '';
    };
  }
  // lib.optionalAttrs host.isx86_64 {
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
lib.optionalAttrs (stdenv.buildPlatform.canExecute host) (lib.mapAttrs mkTest suites)
