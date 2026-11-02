//! The type system's guarantees, checked by compiling code against them.
//!
//! Each file in `ui/fail` misuses the API in a way that would break memory
//! safety (a pointer outliving the scope that keeps its object alive, Fil-C
//! memory touched from the wrong thread or while exited, a forged
//! capability, ...) and must be rejected with the error in its `.stderr`
//! file. The files in `ui/pass` do the same things correctly and must
//! compile, so the failures are known to come from the misuse itself.
//!
//! The expected errors depend on the compiler version; after a toolchain bump,
//! review the differences and accept them with `TRYBUILD=overwrite`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail/*.rs");
    t.pass("tests/ui/pass/*.rs");
}
