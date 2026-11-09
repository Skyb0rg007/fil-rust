//! What Fil-C code can find out about the processor: `cpuid`, `xgetbv` and
//! `__builtin_cpu_supports`, checked against what Rust finds out itself.
//! Only for x86-64.
#![cfg(target_arch = "x86_64")]

mod common;

use std::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv};
use std::ffi::c_char;

use common::runtime;
use fil_rust::Ptr;

fil_rust::foreign! {
    struct Cpu {
        fn supported() -> u64;
        fn vendor() -> i32;
        fn cpuid_vendor(name: Ptr<c_char>) -> u32;
        fn cpuid_leaf7_ebx() -> u32;
        fn xcr0() -> u64;
        fn levels() -> i32;
    }
}

fn cpu() -> &'static Cpu {
    static CPU: std::sync::OnceLock<Cpu> = std::sync::OnceLock::new();
    CPU.get_or_init(|| runtime().run(|fil| Cpu::load(fil, common::fixture("cpu.c")).unwrap()))
}

#[test]
fn cpuid_and_xgetbv_are_the_processors() {
    let cpu = cpu();
    let leaf0 = __cpuid(0);
    let mut expected = Vec::new();
    for word in [leaf0.ebx, leaf0.edx, leaf0.ecx] {
        expected.extend_from_slice(&word.to_le_bytes());
    }
    runtime().run(|fil| {
        let name = fil.alloc_zeroed::<c_char>(16).unwrap();
        assert_eq!(cpu.cpuid_vendor(fil, name).unwrap(), leaf0.eax);
        let got: Vec<u8> = (0..12).map(|i| name.add(i).read(fil).unwrap() as u8).collect();
        assert_eq!(got, expected);
        assert_eq!(cpu.cpuid_leaf7_ebx(fil).unwrap(), __cpuid_count(7, 0).ebx);
        assert_eq!(cpu.xcr0(fil).unwrap(), unsafe { _xgetbv(0) });
    });
}

/// Stock libpizlo says no to everything unless `__builtin_cpu_init` was
/// called, and to what's past the first word of `__cpu_features2` even then:
/// issue 7 in docs/upstream-filc-issues.md.
#[test]
fn builtin_cpu_supports_agrees_with_the_processor() {
    let expected = [
        is_x86_feature_detected!("sse4.2"),
        is_x86_feature_detected!("avx2"),
        is_x86_feature_detected!("popcnt"),
        is_x86_feature_detected!("bmi2"),
        is_x86_feature_detected!("adx"),
        is_x86_feature_detected!("sha"),
        is_x86_feature_detected!("gfni"),
        is_x86_feature_detected!("vaes"),
    ];
    let expected = expected.iter().enumerate().fold(0u64, |bits, (n, &on)| bits | (on as u64) << n);
    let got = runtime().run(|fil| cpu().supported(fil).unwrap());
    assert_eq!(got, expected, "got {got:#b}, expected {expected:#b}");
}

#[test]
fn builtin_cpu_is_knows_the_vendor() {
    let leaf0 = __cpuid(0);
    let expected = match (leaf0.ebx, leaf0.edx, leaf0.ecx) {
        (0x756e_6547, 0x4965_6e69, 0x6c65_746e) => 1,
        (0x6874_7541, 0x6974_6e65, 0x444d_4163) => 2,
        _ => 0,
    };
    assert_eq!(runtime().run(|fil| cpu().vendor(fil).unwrap()), expected);
}

/// Stock libpizlo panics on these, as they're past the end of its
/// `__cpu_features2`: issue 7.
#[test]
fn microarchitecture_levels() {
    let v2 = ["cmpxchg16b", "popcnt", "sse3", "sse4.1", "sse4.2", "ssse3"].iter().all(|&f| match f {
        "cmpxchg16b" => is_x86_feature_detected!("cmpxchg16b"),
        "popcnt" => is_x86_feature_detected!("popcnt"),
        "sse3" => is_x86_feature_detected!("sse3"),
        "sse4.1" => is_x86_feature_detected!("sse4.1"),
        "sse4.2" => is_x86_feature_detected!("sse4.2"),
        _ => is_x86_feature_detected!("ssse3"),
    });
    let v3 = v2
        && is_x86_feature_detected!("avx")
        && is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("bmi1")
        && is_x86_feature_detected!("bmi2")
        && is_x86_feature_detected!("f16c")
        && is_x86_feature_detected!("fma")
        && is_x86_feature_detected!("lzcnt")
        && is_x86_feature_detected!("movbe")
        && is_x86_feature_detected!("xsave");
    let got = runtime().run(|fil| cpu().levels(fil).unwrap());
    assert_eq!(got, v2 as i32 | (v3 as i32) << 1);
}
