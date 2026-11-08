// The access checks Fil-C 0.687 emits on x86-64, written out by hand so both variants
// below are exactly the instructions being compared:
//
// - filc_store*: what Fil-C emits today. Test READONLY|FREE in the header's flag byte,
//   check bounds, store.
// - rseq_store*: the same, but inside an rseq critical section whose last instruction is
//   the store, testing a PINNED bit as well. If the kernel restarts the section
//   (preemption, signal, or another thread's membarrier), it runs again from the test.
// - rseqc_store*: the same section, entered by comparing the thread's rseq slot with the
//   section's descriptor first and storing only on a mismatch.
//
// An object is laid out as Fil-C lays it out: a 16-byte header (upper, then aux with the
// flags in its top 16 bits) immediately before `lower`, where the payload starts.

#pragma once

#include <stddef.h>
#include <stdint.h>
#include <sys/rseq.h>

#define STR_(x) #x
#define STR(x) STR_(x)

#define STORE_MASK 0x06 // READONLY | FREE: what Fil-C tests today
#define PINNED 0x40 // the unused bit of that flag byte
#define PINNED_STORE_MASK 0x46

extern __thread unsigned long restarts;
extern __thread unsigned long pinned_waits;
void check_failed(void) __attribute__((noreturn, cold));
void poll_slow(void) __attribute__((cold));
void wait_unpinned(uint8_t *lower) __attribute__((cold));

static inline uint8_t *flag_byte(uint8_t *lower) {
    return lower - 2;
}

static inline struct rseq *rseq_area(void) {
    return (struct rseq *)((char *)__builtin_thread_pointer() + __rseq_offset);
}

// Fil-C's pollcheck, as emitted on each loop back edge.
static inline __attribute__((always_inline)) void pollcheck(const uint8_t *thread) {
    asm goto("testb $0xe, 8(%[t])\n\t"
             "jne %l[slow]\n\t"
             :
             : [t] "r"(thread)
             : "cc"
             : slow);
    return;
slow:
    poll_slow();
}

static inline __attribute__((always_inline)) void native_store8(uint8_t *p, uint8_t v) {
    asm volatile("movb %b[v], (%[p])" : : [p] "r"(p), [v] "r"(v) : "memory");
}

static inline __attribute__((always_inline)) void native_store32(uint32_t *p, uint32_t v) {
    asm volatile("movl %k[v], (%[p])" : : [p] "r"(p), [v] "r"(v) : "memory");
}

static inline __attribute__((always_inline)) void filc_store8(uint8_t *lower, uint8_t *p, uint8_t v) {
    asm goto("testq %[lower], %[lower]\n\t"
             "je %l[fail]\n\t"
             "testb $" STR(STORE_MASK) ", -2(%[lower])\n\t"
             "jne %l[fail]\n\t"
             "cmpq %[lower], %[p]\n\t"
             "jb %l[fail]\n\t"
             "cmpq -16(%[lower]), %[p]\n\t"
             "jae %l[fail]\n\t"
             "movb %b[v], (%[p])\n\t"
             :
             : [lower] "r"(lower), [p] "r"(p), [v] "r"(v)
             : "memory", "cc"
             : fail);
    return;
fail:
    check_failed();
}

static inline __attribute__((always_inline)) void filc_store32(uint8_t *lower, uint32_t *p, uint32_t v) {
    asm goto("testq %[lower], %[lower]\n\t"
             "je %l[fail]\n\t"
             "testb $" STR(STORE_MASK) ", -2(%[lower])\n\t"
             "jne %l[fail]\n\t"
             "cmpq %[lower], %[p]\n\t"
             "jb %l[fail]\n\t"
             "leaq 4(%[p]), %%rax\n\t"
             "cmpq -16(%[lower]), %%rax\n\t"
             "ja %l[fail]\n\t"
             "movl %k[v], (%[p])\n\t"
             :
             : [lower] "r"(lower), [p] "r"(p), [v] "r"(v)
             : "rax", "memory", "cc"
             : fail);
    return;
fail:
    check_failed();
}

// Loads only check bounds in Fil-C, and pinning doesn't change them.
static inline __attribute__((always_inline)) uint32_t filc_load32(uint8_t *lower, uint32_t *p) {
    asm goto("testq %[lower], %[lower]\n\t"
             "je %l[fail]\n\t"
             "cmpq %[lower], %[p]\n\t"
             "jb %l[fail]\n\t"
             "leaq 4(%[p]), %%rax\n\t"
             "cmpq -16(%[lower]), %%rax\n\t"
             "ja %l[fail]\n\t"
             :
             : [lower] "r"(lower), [p] "r"(p)
             : "rax", "cc"
             : fail);
    return *(volatile uint32_t *)p;
fail:
    check_failed();
}

// The pieces of an rseq critical section, after librseq's x86-64 macros. Label 3 is the
// struct rseq_cs, 1 the start, 2 just past the committing store, 4 the abort handler, 6
// an out-of-line slow path. The four bytes before the abort handler must be the signature
// glibc registered; they're hidden in a ud1 instruction, as librseq does.
#define RSEQ_CS_TABLE                                                                      \
    ".pushsection __rseq_cs, \"aw\"\n\t"                                                   \
    ".balign 32\n\t"                                                                       \
    "3:\n\t"                                                                               \
    ".long 0x0, 0x0\n\t"                                                                   \
    ".quad 1f, (2f - 1f), 4f\n\t"                                                          \
    ".popsection\n\t"

// Two ways into a section. RSEQ_ENTER stores the descriptor into the thread's rseq area
// every time, as librseq does. RSEQ_ENTER_CMP first compares, inside the section, and
// stores only if the slot holds something else. That's safe: if the kernel cleared the
// slot before the section, the compare sees it; once the compare passes, the kernel
// protects the rest. In a loop with one store site the store becomes an L1 load.
#define RSEQ_ENTER                                                                         \
    "leaq 3b(%%rip), %%rax\n\t"                                                            \
    "movq %%rax, %[cs]\n\t"                                                                \
    "1:\n\t"
#define RSEQ_SET_SLOW ""

#define RSEQ_ENTER_CMP                                                                     \
    "leaq 3b(%%rip), %%rax\n\t"                                                            \
    "1:\n\t"                                                                               \
    "cmpq %%rax, %[cs]\n\t"                                                                \
    "jne 6f\n\t"
#define RSEQ_SET_SLOW_CMP                                                                  \
    "6:\n\t"                                                                               \
    "movq %%rax, %[cs]\n\t"                                                                \
    "jmp 1b\n\t"

// The end of a section, and out of line, the abort handler (with the signature right
// before it) plus any slow path the entry needs.
#define RSEQ_EXIT(SET_SLOW)                                                                \
    "2:\n\t"                                                                               \
    ".pushsection __rseq_failure, \"ax\"\n\t" SET_SLOW                                     \
    ".byte 0x0f, 0xb9, 0x3d\n\t"                                                           \
    ".long " STR(RSEQ_SIG) "\n\t"                                                          \
    "4:\n\t"                                                                               \
    "jmp %l[restart]\n\t"                                                                  \
    ".popsection\n\t"

#define RSEQ_TAIL(slow_counter)                                                            \
    return;                                                                                \
restart:                                                                                   \
    restarts++;                                                                            \
    goto again;                                                                            \
slow:                                                                                      \
    slow_counter;                                                                          \
    wait_unpinned(lower);                                                                  \
    goto again;                                                                            \
fail:                                                                                      \
    check_failed();

#define RSEQ_PIN_TEST                                                                      \
    "testq %[lower], %[lower]\n\t"                                                         \
    "je %l[fail]\n\t"                                                                      \
    "testb $" STR(PINNED_STORE_MASK) ", -2(%[lower])\n\t"                                  \
    "jne %l[slow]\n\t"

#define RSEQ_BOUNDS32                                                                      \
    "cmpq %[lower], %[p]\n\t"                                                              \
    "jb %l[fail]\n\t"                                                                      \
    "leaq 4(%[p]), %%rax\n\t"                                                              \
    "cmpq -16(%[lower]), %%rax\n\t"                                                        \
    "ja %l[fail]\n\t"

// prefix_store8, prefix_store32, and prefix_store32_widened: the latter with `widen`
// pause instructions between the flag test and the store, to make the window the
// protocol has to close wide enough to observe (widen must be >= 1).
#define DEFINE_RSEQ_STORES(prefix, ENTER, SET_SLOW)                                        \
    static inline __attribute__((always_inline)) void prefix##_store8(struct rseq *rs, uint8_t *lower,  \
                                                                      uint8_t *p, uint8_t v) {          \
    again:                                                                                 \
        asm goto(RSEQ_CS_TABLE ENTER RSEQ_PIN_TEST                                         \
                 "cmpq %[lower], %[p]\n\t"                                                 \
                 "jb %l[fail]\n\t"                                                         \
                 "cmpq -16(%[lower]), %[p]\n\t"                                            \
                 "jae %l[fail]\n\t"                                                        \
                 "movb %b[v], (%[p])\n\t" RSEQ_EXIT(SET_SLOW)                              \
                 :                                                                         \
                 : [cs] "m"(rs->rseq_cs), [lower] "r"(lower), [p] "r"(p), [v] "r"(v)       \
                 : "rax", "memory", "cc"                                                   \
                 : fail, slow, restart);                                                   \
        RSEQ_TAIL((void)0)                                                                 \
    }                                                                                      \
    static inline __attribute__((always_inline)) void prefix##_store32(struct rseq *rs, uint8_t *lower, \
                                                                       uint32_t *p, uint32_t v) {       \
    again:                                                                                 \
        asm goto(RSEQ_CS_TABLE ENTER RSEQ_PIN_TEST RSEQ_BOUNDS32                           \
                 "movl %k[v], (%[p])\n\t" RSEQ_EXIT(SET_SLOW)                              \
                 :                                                                         \
                 : [cs] "m"(rs->rseq_cs), [lower] "r"(lower), [p] "r"(p), [v] "r"(v)       \
                 : "rax", "memory", "cc"                                                   \
                 : fail, slow, restart);                                                   \
        RSEQ_TAIL(pinned_waits++)                                                          \
    }                                                                                      \
    static inline __attribute__((always_inline)) void prefix##_store32_widened(            \
        struct rseq *rs, uint8_t *lower, uint32_t *p, uint32_t v, uint32_t widen) {        \
    again:                                                                                 \
        asm goto(RSEQ_CS_TABLE ENTER RSEQ_PIN_TEST                                         \
                 "movl %[widen], %%ecx\n\t"                                                \
                 "5: pause\n\t"                                                            \
                 "decl %%ecx\n\t"                                                          \
                 "jnz 5b\n\t" RSEQ_BOUNDS32                                                \
                 "movl %k[v], (%[p])\n\t" RSEQ_EXIT(SET_SLOW)                              \
                 :                                                                         \
                 : [cs] "m"(rs->rseq_cs), [lower] "r"(lower), [p] "r"(p), [v] "r"(v),      \
                   [widen] "r"(widen)                                                      \
                 : "rax", "rcx", "memory", "cc"                                            \
                 : fail, slow, restart);                                                   \
        RSEQ_TAIL(pinned_waits++)                                                          \
    }

DEFINE_RSEQ_STORES(rseq, RSEQ_ENTER, RSEQ_SET_SLOW)
DEFINE_RSEQ_STORES(rseqc, RSEQ_ENTER_CMP, RSEQ_SET_SLOW_CMP)
