// Native functions over memory that Fil-C threads may be writing, in four variants:
//
// - restrict: noalias pointers, what a Rust `&[T]`/`&mut [T]` gives the optimizer. Only
//   safe if nobody else writes the memory during the call (the rseq pin's job).
// - plain: no restrict. NOT safe under concurrent writers either: the compiler still
//   assumes no data races, so it may reload a value it already checked. Shown only to
//   separate the cost of noalias from the cost of race tolerance.
// - atomic: race-tolerant. Every access to shared memory is a relaxed atomic, so each
//   value is read once and the compiler can't invent a reload. What safe Rust over
//   `&[AtomicU32]`-style views compiles to. Private state (accumulators, tables) is plain.
// - asmvec: race-tolerant and hand-vectorized. Loads and stores of shared memory are
//   16-byte inline-asm moves, so each is one opaque read whose value lives in a register.
//   SSE2 only, on every target.
//
// Compiled once per target with -DSUFFIX=..., so every name gets that suffix.

#include <emmintrin.h>
#include <stddef.h>
#include <stdint.h>

#ifndef SUFFIX
#error "define SUFFIX"
#endif

#define CAT_(a, b) a##b
#define CAT(a, b) CAT_(a, b)
#define K(name) CAT(name, SUFFIX)

#define ALD(p) __atomic_load_n((p), __ATOMIC_RELAXED)
#define AST(p, v) __atomic_store_n((p), (v), __ATOMIC_RELAXED)

static inline __m128i load_racy(const void *p) {
    __m128i v;
    asm volatile("movdqu %1, %0" : "=x"(v) : "m"(*(const char(*)[16])p));
    return v;
}

static inline void store_racy(void *p, __m128i v) {
    asm volatile("movdqu %1, %0" : "=m"(*(char(*)[16])p) : "x"(v));
}

#define KERNEL(name) uint64_t K(name)(void *dst_, const void *a_, const void *b_, size_t n)

// dst[i] = a[i] + b[i], u32.

KERNEL(add_restrict) {
    uint32_t *restrict d = dst_;
    const uint32_t *restrict a = a_, *restrict b = b_;
    for (size_t i = 0; i < n; i++)
        d[i] = a[i] + b[i];
    return 0;
}

KERNEL(add_plain) {
    uint32_t *d = dst_;
    const uint32_t *a = a_, *b = b_;
    for (size_t i = 0; i < n; i++)
        d[i] = a[i] + b[i];
    return 0;
}

KERNEL(add_atomic) {
    uint32_t *d = dst_;
    const uint32_t *a = a_, *b = b_;
    for (size_t i = 0; i < n; i++)
        AST(&d[i], ALD(&a[i]) + ALD(&b[i]));
    return 0;
}

KERNEL(add_asmvec) {
    uint32_t *d = dst_;
    const uint32_t *a = a_, *b = b_;
    size_t i = 0;
    for (; i + 4 <= n; i += 4)
        store_racy(d + i, _mm_add_epi32(load_racy(a + i), load_racy(b + i)));
    for (; i < n; i++)
        AST(&d[i], ALD(&a[i]) + ALD(&b[i]));
    return 0;
}

// Sum of u32 into u64. Read-only, so restrict changes nothing; atomics stop vectorization.

KERNEL(sum_restrict) {
    const uint32_t *restrict a = a_;
    uint64_t s = 0;
    for (size_t i = 0; i < n; i++)
        s += a[i];
    return s;
}

KERNEL(sum_plain) {
    const uint32_t *a = a_;
    uint64_t s = 0;
    for (size_t i = 0; i < n; i++)
        s += a[i];
    return s;
}

KERNEL(sum_atomic) {
    const uint32_t *a = a_;
    uint64_t s = 0;
    for (size_t i = 0; i < n; i++)
        s += ALD(&a[i]);
    return s;
}

KERNEL(sum_asmvec) {
    const uint32_t *a = a_;
    __m128i acc = _mm_setzero_si128(), zero = _mm_setzero_si128();
    size_t i = 0;
    for (; i + 4 <= n; i += 4) {
        __m128i v = load_racy(a + i);
        acc = _mm_add_epi64(acc, _mm_unpacklo_epi32(v, zero));
        acc = _mm_add_epi64(acc, _mm_unpackhi_epi32(v, zero));
    }
    uint64_t lanes[2];
    _mm_storeu_si128((__m128i *)lanes, acc);
    uint64_t s = lanes[0] + lanes[1];
    for (; i < n; i++)
        s += ALD(&a[i]);
    return s;
}

// Index of the first 0xff byte (memchr-like), or n.

KERNEL(find_restrict) {
    const uint8_t *restrict a = a_;
    for (size_t i = 0; i < n; i++)
        if (a[i] == 0xff)
            return i;
    return n;
}

KERNEL(find_plain) {
    const uint8_t *a = a_;
    for (size_t i = 0; i < n; i++)
        if (a[i] == 0xff)
            return i;
    return n;
}

KERNEL(find_atomic) {
    const uint8_t *a = a_;
    for (size_t i = 0; i < n; i++)
        if (ALD(&a[i]) == 0xff)
            return i;
    return n;
}

KERNEL(find_asmvec) {
    const uint8_t *a = a_;
    __m128i ff = _mm_set1_epi8((char)0xff);
    size_t i = 0;
    for (; i + 16 <= n; i += 16) {
        int mask = _mm_movemask_epi8(_mm_cmpeq_epi8(load_racy(a + i), ff));
        if (mask)
            return i + (size_t)__builtin_ctz((unsigned)mask);
    }
    for (; i < n; i++)
        if (ALD(&a[i]) == 0xff)
            return i;
    return n;
}

// Byte copy. The atomic variant moves aligned u64 words: the best scalar race-tolerant copy.

KERNEL(copy_restrict) {
    uint8_t *restrict d = dst_;
    const uint8_t *restrict a = a_;
    for (size_t i = 0; i < n; i++)
        d[i] = a[i];
    return 0;
}

KERNEL(copy_plain) {
    uint8_t *d = dst_;
    const uint8_t *a = a_;
    for (size_t i = 0; i < n; i++)
        d[i] = a[i];
    return 0;
}

KERNEL(copy_atomic) {
    uint64_t *d = dst_;
    const uint64_t *a = a_;
    for (size_t i = 0; i < n / 8; i++)
        AST(&d[i], ALD(&a[i]));
    return 0;
}

KERNEL(copy_asmvec) {
    uint8_t *d = dst_;
    const uint8_t *a = a_;
    size_t i = 0;
    for (; i + 16 <= n; i += 16)
        store_racy(d + i, load_racy(a + i));
    for (; i < n; i++)
        AST(&d[i], ALD(&a[i]));
    return 0;
}

// *out += a[i] * 3. With restrict, *out lives in a register. Without, it may alias a, so
// it's stored every iteration. The race-tolerant version keeps a private accumulator, as
// code written against views would, and stores once.

KERNEL(accum_restrict) {
    uint32_t *restrict out = dst_;
    const uint32_t *restrict a = a_;
    for (size_t i = 0; i < n; i++)
        *out += a[i] * 3;
    return *out;
}

KERNEL(accum_plain) {
    uint32_t *out = dst_;
    const uint32_t *a = a_;
    for (size_t i = 0; i < n; i++)
        *out += a[i] * 3;
    return *out;
}

KERNEL(accum_atomic) {
    uint32_t *out = dst_;
    const uint32_t *a = a_;
    uint32_t acc = ALD(out);
    for (size_t i = 0; i < n; i++)
        acc += ALD(&a[i]) * 3;
    AST(out, acc);
    return acc;
}

// Histogram into a private table: a value read from shared memory becomes an index. This
// is the pattern where a reload after a check would go out of bounds.

KERNEL(hist_restrict) {
    const uint8_t *restrict a = a_;
    uint32_t h[256] = {0};
    for (size_t i = 0; i < n; i++)
        h[a[i]]++;
    uint64_t s = 0;
    for (int i = 0; i < 256; i++)
        s += (uint64_t)h[i] * (uint64_t)i;
    return s;
}

KERNEL(hist_plain) {
    const uint8_t *a = a_;
    uint32_t h[256] = {0};
    for (size_t i = 0; i < n; i++)
        h[a[i]]++;
    uint64_t s = 0;
    for (int i = 0; i < 256; i++)
        s += (uint64_t)h[i] * (uint64_t)i;
    return s;
}

KERNEL(hist_atomic) {
    const uint8_t *a = a_;
    uint32_t h[256] = {0};
    for (size_t i = 0; i < n; i++)
        h[ALD(&a[i])]++;
    uint64_t s = 0;
    for (int i = 0; i < 256; i++)
        s += (uint64_t)h[i] * (uint64_t)i;
    return s;
}
