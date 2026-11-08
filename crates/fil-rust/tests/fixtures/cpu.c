/* What Fil-C code can find out about the processor. */

#include <cpuid.h>
#include <stdfil.h>

/* Bit n set if __builtin_cpu_supports says yes to the nth of these, which
   tests/cpu.rs asks the processor about too. Nothing calls __builtin_cpu_init
   first, as a C program needn't. */
unsigned long supported(void)
{
    unsigned long result = 0;
    int n = 0;
#define FEATURE(name) result |= (unsigned long)!!__builtin_cpu_supports(name) << n++
    FEATURE("sse4.2");     /* in __cpu_model */
    FEATURE("avx2");
    FEATURE("popcnt");
    FEATURE("bmi2");
    FEATURE("adx");        /* in __cpu_features2[0] */
    FEATURE("sha");        /* in __cpu_features2[1] */
    FEATURE("gfni");       /* in __cpu_features2[0] */
    FEATURE("vaes");       /* in __cpu_features2[1] */
#undef FEATURE
    return result;
}

/* 1 for Intel, 2 for AMD, 0 for neither. */
int vendor(void)
{
    __builtin_cpu_init();
    return __builtin_cpu_is("intel") ? 1 : __builtin_cpu_is("amd") ? 2 : 0;
}

/* The highest basic leaf and the vendor string, from cpuid itself. */
unsigned cpuid_vendor(char* name)
{
    unsigned eax, ebx, ecx, edx;
    __cpuid(0, eax, ebx, ecx, edx);
    __builtin_memcpy(name, &ebx, 4);
    __builtin_memcpy(name + 4, &edx, 4);
    __builtin_memcpy(name + 8, &ecx, 4);
    name[12] = 0;
    return eax;
}

/* Leaf 7's EBX, through the counted form. */
unsigned cpuid_leaf7_ebx(void)
{
    unsigned eax, ebx, ecx, edx;
    __cpuid_count(7, 0, eax, ebx, ecx, edx);
    return ebx;
}

unsigned long xcr0(void)
{
    return zxgetbv();
}

/* Whether the processor has x86-64-v2 and v3, in __cpu_features2[2]. */
int levels(void)
{
    return !!__builtin_cpu_supports("x86-64-v2") | !!__builtin_cpu_supports("x86-64-v3") << 1;
}
