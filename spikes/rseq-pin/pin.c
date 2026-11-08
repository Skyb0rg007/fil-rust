// Measures the two ways native code could safely use memory that Fil-C threads may be
// writing:
//
// 1. rseq pins: every Fil-C store becomes an rseq critical section that tests a PINNED
//    bit. Native code pins an object, issues membarrier(PRIVATE_EXPEDITED_RSEQ), and then
//    has real noalias access. Cost: on every Fil-C store, plus a pin and sometimes a
//    membarrier per native call.
// 2. Race tolerance: Fil-C is unchanged, and native code reads and writes shared memory
//    with relaxed atomics or opaque asm moves, giving up noalias. Cost: in native code.
//
//   pin stores    Fil-C store overhead: unchecked, Fil-C today, Fil-C with rseq (both entries)
//   pin native    native kernels: restrict, plain, atomic, asm-vector
//   pin barrier   pin/unpin and membarrier latency, with other threads busy or asleep
//   pin verify MODE [WIDEN [cmp]]   check the protocol: MODE is full, nobarrier or nopin;
//                 cmp uses the compare-first section entry
//   pin all       stores, native, barrier, and verify in each mode

#define _GNU_SOURCE
#include <linux/membarrier.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#include "filc_ops.h"

__thread unsigned long restarts;
__thread unsigned long pinned_waits;

void check_failed(void) {
    fputs("Fil-C check failed\n", stderr);
    abort();
}

void poll_slow(void) {
}

void wait_unpinned(uint8_t *lower) {
    while (__atomic_load_n(flag_byte(lower), __ATOMIC_ACQUIRE) & PINNED)
        __builtin_ia32_pause();
}

// What pollcheck reads; never set, as in a thread nobody is stopping.
static uint8_t thread_state[64] __attribute__((aligned(64)));

static int membarrier(int cmd) {
    return (int)syscall(__NR_membarrier, cmd, 0, 0);
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec * 1e-9;
}

static void spin_for(double seconds) {
    double until = now() + seconds;
    while (now() < until)
        __builtin_ia32_pause();
}

static void pin_cpu(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    if (sched_setaffinity(0, sizeof set, &set) != 0) {
        perror("sched_setaffinity");
        exit(1);
    }
}

static uint8_t *new_object(size_t size) {
    uint8_t *base = aligned_alloc(64, size + 64);
    if (!base)
        abort();
    memset(base, 0, size + 64);
    uint8_t *lower = base + 64;
    ((uint64_t *)lower)[-2] = (uint64_t)(uintptr_t)(lower + size); // upper; aux and flags 0
    return lower;
}

static int compare_doubles(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

#define REPS 9
#define TARGET_OPS (1ul << 25)

static const size_t sizes[] = {4 << 10, 256 << 10, 16 << 20};
static const char *size_names[] = {"4 KiB", "256 KiB", "16 MiB"};
#define NSIZES (sizeof sizes / sizeof sizes[0])

// --- Fil-C stores --------------------------------------------------------------------

#define ST8_native(rs, lower, p, v) native_store8(p, v)
#define ST8_filc(rs, lower, p, v)                                                          \
    do {                                                                                   \
        filc_store8(lower, p, v);                                                          \
        pollcheck(thread_state);                                                           \
    } while (0)
#define ST8_rseq(rs, lower, p, v)                                                          \
    do {                                                                                   \
        rseq_store8(rs, lower, p, v);                                                      \
        pollcheck(thread_state);                                                           \
    } while (0)
#define ST32_native(rs, lower, p, v) native_store32(p, v)
#define ST32_filc(rs, lower, p, v)                                                         \
    do {                                                                                   \
        filc_store32(lower, p, v);                                                         \
        pollcheck(thread_state);                                                           \
    } while (0)
#define ST32_rseq(rs, lower, p, v)                                                         \
    do {                                                                                   \
        rseq_store32(rs, lower, p, v);                                                     \
        pollcheck(thread_state);                                                           \
    } while (0)
#define LD32_native(lower, p) (*(volatile uint32_t *)(p))
#define LD32_filc(lower, p) filc_load32(lower, p)
#define LD32_rseq(lower, p) filc_load32(lower, p)
#define ST8_rseqc(rs, lower, p, v)                                                         \
    do {                                                                                   \
        rseqc_store8(rs, lower, p, v);                                                     \
        pollcheck(thread_state);                                                           \
    } while (0)
#define ST32_rseqc(rs, lower, p, v)                                                        \
    do {                                                                                   \
        rseqc_store32(rs, lower, p, v);                                                    \
        pollcheck(thread_state);                                                           \
    } while (0)
#define LD32_rseqc(lower, p) filc_load32(lower, p)

#define LCG(x) ((x) = (x) * 6364136223846793005ull + 1442695040888963407ull)

#define WORKLOADS(V)                                                                       \
    static void __attribute__((noinline)) seq8_##V(uint8_t *lower, size_t bytes, size_t passes) { \
        struct rseq *rs = rseq_area();                                                     \
        (void)rs;                                                                          \
        for (size_t r = 0; r < passes; r++)                                                \
            for (size_t i = 0; i < bytes; i++)                                             \
                ST8_##V(rs, lower, lower + i, (uint8_t)i);                                 \
    }                                                                                      \
    static void __attribute__((noinline)) seq32_##V(uint8_t *lower, size_t bytes, size_t passes) { \
        struct rseq *rs = rseq_area();                                                     \
        (void)rs;                                                                          \
        uint32_t *w = (uint32_t *)lower;                                                   \
        for (size_t r = 0; r < passes; r++)                                                \
            for (size_t i = 0; i < bytes / 4; i++)                                         \
                ST32_##V(rs, lower, w + i, (uint32_t)i);                                   \
    }                                                                                      \
    static void __attribute__((noinline)) rand32_##V(uint8_t *lower, size_t bytes, size_t passes) { \
        struct rseq *rs = rseq_area();                                                     \
        (void)rs;                                                                          \
        uint32_t *w = (uint32_t *)lower;                                                   \
        unsigned shift = 64 - (unsigned)__builtin_ctzl(bytes / 4);                         \
        uint64_t x = 1;                                                                    \
        for (size_t r = 0; r < passes; r++)                                                \
            for (size_t i = 0; i < bytes / 4; i++) {                                       \
                LCG(x);                                                                    \
                ST32_##V(rs, lower, w + (x >> shift), (uint32_t)i);                        \
            }                                                                              \
    }                                                                                      \
    static void __attribute__((noinline)) rmw32_##V(uint8_t *lower, size_t bytes, size_t passes) { \
        struct rseq *rs = rseq_area();                                                     \
        (void)rs;                                                                          \
        uint32_t *w = (uint32_t *)lower;                                                   \
        for (size_t r = 0; r < passes; r++)                                                \
            for (size_t i = 0; i < bytes / 4; i++) {                                       \
                uint32_t v = LD32_##V(lower, w + i);                                       \
                ST32_##V(rs, lower, w + i, v + 1);                                         \
            }                                                                              \
    }

WORKLOADS(native)
WORKLOADS(filc)
WORKLOADS(rseq)
WORKLOADS(rseqc)

typedef void (*workload_fn)(uint8_t *, size_t, size_t);

static const struct {
    const char *name;
    unsigned elem;
    workload_fn fn[4];
} workloads[] = {
    {"sequential u8 stores", 1, {seq8_native, seq8_filc, seq8_rseq, seq8_rseqc}},
    {"sequential u32 stores", 4, {seq32_native, seq32_filc, seq32_rseq, seq32_rseqc}},
    {"random u32 stores", 4, {rand32_native, rand32_filc, rand32_rseq, rand32_rseqc}},
    {"u32 load then store", 4, {rmw32_native, rmw32_filc, rmw32_rseq, rmw32_rseqc}},
};

static double time_workload(workload_fn fn, uint8_t *lower, size_t bytes, unsigned elem) {
    size_t ops = bytes / elem;
    size_t passes = TARGET_OPS / ops ? TARGET_OPS / ops : 1;
    double t[REPS];
    fn(lower, bytes, 1);
    for (int r = 0; r < REPS; r++) {
        double start = now();
        fn(lower, bytes, passes);
        t[r] = (now() - start) * 1e9 / (double)(ops * passes);
    }
    qsort(t, REPS, sizeof t[0], compare_doubles);
    return t[REPS / 2];
}

static void run_stores(void) {
    printf("\n== Fil-C stores: ns per store (median of %d) ==\n", REPS);
    printf("(+rseq stores the descriptor every time; +rseq cmp compares first and stores only if it differs)\n");
    printf("%-24s %-8s %9s %9s %9s %9s %9s %9s\n", "workload", "size", "unchecked", "Fil-C", "+rseq", "+rseq cmp",
           "cost", "cmp cost");
    unsigned long before = restarts;
    for (size_t w = 0; w < sizeof workloads / sizeof workloads[0]; w++)
        for (size_t s = 0; s < NSIZES; s++) {
            uint8_t *lower = new_object(sizes[s]);
            double t[4];
            for (int v = 0; v < 4; v++)
                t[v] = time_workload(workloads[w].fn[v], lower, sizes[s], workloads[w].elem);
            printf("%-24s %-8s %9.3f %9.3f %9.3f %9.3f %+8.1f%% %+8.1f%%\n", workloads[w].name, size_names[s],
                   t[0], t[1], t[2], t[3], (t[2] / t[1] - 1) * 100, (t[3] / t[1] - 1) * 100);
            free(lower - 64);
        }
    printf("(restarts during the rseq runs: %lu)\n", restarts - before);
}

// --- native kernels ------------------------------------------------------------------

typedef uint64_t (*kernel_fn)(void *, const void *, const void *, size_t);

#define KERNELS(X, S)                                                                      \
    X(add, S, 4) X(sum, S, 4) X(find, S, 4) X(copy, S, 4) X(accum, S, 3) X(hist, S, 3)
#define DECLARE4(k, S)                                                                     \
    uint64_t k##_restrict##S(void *, const void *, const void *, size_t);                  \
    uint64_t k##_plain##S(void *, const void *, const void *, size_t);                     \
    uint64_t k##_atomic##S(void *, const void *, const void *, size_t);                    \
    uint64_t k##_asmvec##S(void *, const void *, const void *, size_t);
#define DECLARE3(k, S)                                                                     \
    uint64_t k##_restrict##S(void *, const void *, const void *, size_t);                  \
    uint64_t k##_plain##S(void *, const void *, const void *, size_t);                     \
    uint64_t k##_atomic##S(void *, const void *, const void *, size_t);
#define DECLARE(k, S, n) DECLARE##n(k, S)
KERNELS(DECLARE, _v1)
KERNELS(DECLARE, _v3)

#define ROW4(k, S) {k##_restrict##S, k##_plain##S, k##_atomic##S, k##_asmvec##S}
#define ROW3(k, S) {k##_restrict##S, k##_plain##S, k##_atomic##S, NULL}
#define ROW(k, S, n) ROW##n(k, S)

static const struct {
    const char *name;
    unsigned elem;
    kernel_fn fn[2][4];
} kernels[] = {
    {"add u32", 4, {ROW(add, _v1, 4), ROW(add, _v3, 4)}},
    {"sum u32", 4, {ROW(sum, _v1, 4), ROW(sum, _v3, 4)}},
    {"find byte", 1, {ROW(find, _v1, 4), ROW(find, _v3, 4)}},
    {"copy bytes", 1, {ROW(copy, _v1, 4), ROW(copy, _v3, 4)}},
    {"accumulate u32", 4, {ROW(accum, _v1, 3), ROW(accum, _v3, 3)}},
    {"histogram bytes", 1, {ROW(hist, _v1, 3), ROW(hist, _v3, 3)}},
};
static const char *variant_names[] = {"restrict", "plain", "atomic", "asmvec"};
static const char *target_names[] = {"x86-64 (baseline, as Rust builds by default)", "x86-64-v3 (AVX2)"};

static double time_kernel(kernel_fn fn, void *dst, const void *a, const void *b, size_t n) {
    size_t passes = TARGET_OPS / n ? TARGET_OPS / n : 1;
    double t[REPS];
    fn(dst, a, b, n);
    for (int r = 0; r < REPS; r++) {
        double start = now();
        for (size_t p = 0; p < passes; p++) {
            fn(dst, a, b, n);
            asm volatile("" : : : "memory");
        }
        t[r] = (now() - start) * 1e9 / (double)(n * passes);
    }
    qsort(t, REPS, sizeof t[0], compare_doubles);
    return t[REPS / 2];
}

static void run_native(void) {
    size_t max = sizes[NSIZES - 1];
    uint8_t *a = aligned_alloc(64, max), *b = aligned_alloc(64, max), *dst = aligned_alloc(64, max);
    uint64_t x = 42;
    for (size_t i = 0; i < max; i++) {
        LCG(x);
        a[i] = (uint8_t)((x >> 40) & 0x7f);
        b[i] = (uint8_t)(x >> 48);
    }
    for (int target = 0; target < 2; target++) {
        printf("\n== native kernels, %s: ns per element, others as a multiple of restrict ==\n",
               target_names[target]);
        printf("%-16s %-8s %9s %8s %8s %8s\n", "kernel", "size", "restrict", "plain", "atomic", "asmvec");
        for (size_t k = 0; k < sizeof kernels / sizeof kernels[0]; k++)
            for (size_t s = 0; s < NSIZES; s++) {
                size_t n = sizes[s] / kernels[k].elem;
                a[sizes[s] - 1] = 0xff; // what find looks for: the last byte
                // Every variant must compute the same thing.
                uint64_t expect = 0;
                for (int v = 0; v < 4; v++) {
                    kernel_fn fn = kernels[k].fn[target][v];
                    if (!fn)
                        continue;
                    memset(dst, 0, sizes[s]);
                    uint64_t got = fn(dst, a, b, n);
                    uint64_t check = got;
                    for (size_t i = 0; i < sizes[s]; i += 4093)
                        check = check * 31 + dst[i];
                    if (v == 0)
                        expect = check;
                    else if (check != expect) {
                        fprintf(stderr, "%s %s disagrees with restrict\n", kernels[k].name, variant_names[v]);
                        exit(1);
                    }
                }
                double t[4];
                for (int v = 0; v < 4; v++)
                    t[v] = kernels[k].fn[target][v] ? time_kernel(kernels[k].fn[target][v], dst, a, b, n) : 0;
                printf("%-16s %-8s %9.4f", kernels[k].name, size_names[s], t[0]);
                for (int v = 1; v < 4; v++)
                    if (t[v])
                        printf(" %7.2fx", t[v] / t[0]);
                    else
                        printf(" %8s", "-");
                printf("\n");
                a[sizes[s] - 1] = 0;
            }
    }
    free(a);
    free(b);
    free(dst);
}

// --- membarrier ----------------------------------------------------------------------

static volatile int stop_threads;

struct busy {
    int cpu;
    unsigned long stores, restarts;
};

// Other threads running Fil-C-like store loops, so they're often inside a section.
static void *busy_main(void *arg) {
    struct busy *b = arg;
    pin_cpu(b->cpu);
    uint8_t *obj = new_object(4096);
    uint32_t *w = (uint32_t *)obj;
    struct rseq *rs = rseq_area();
    uint64_t x = (uint64_t)b->cpu + 1;
    unsigned long stores = 0;
    while (!stop_threads) {
        for (int i = 0; i < 1024; i++) {
            LCG(x);
            rseq_store32(rs, obj, w + (x >> 54), (uint32_t)i);
        }
        stores += 1024;
    }
    b->stores = stores;
    b->restarts = restarts;
    return NULL;
}

static pthread_mutex_t idle_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t idle_cond = PTHREAD_COND_INITIALIZER;

static void *idle_main(void *arg) {
    (void)arg;
    pthread_mutex_lock(&idle_lock);
    while (!stop_threads)
        pthread_cond_wait(&idle_cond, &idle_lock);
    pthread_mutex_unlock(&idle_lock);
    return NULL;
}

static void time_membarrier(const char *label, int nthreads, int busy) {
    pthread_t threads[16];
    struct busy state[16];
    stop_threads = 0;
    for (int i = 0; i < nthreads; i++) {
        state[i] = (struct busy){.cpu = 1 + i};
        pthread_create(&threads[i], NULL, busy ? busy_main : idle_main, &state[i]);
    }
    spin_for(0.05);
    enum { CALLS = 5000 };
    static double t[CALLS];
    for (int i = 0; i < CALLS; i++) {
        double start = now();
        if (membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ) != 0) {
            perror("membarrier");
            exit(1);
        }
        t[i] = (now() - start) * 1e6;
    }
    stop_threads = 1;
    pthread_mutex_lock(&idle_lock);
    pthread_cond_broadcast(&idle_cond);
    pthread_mutex_unlock(&idle_lock);
    unsigned long stores = 0, aborted = 0;
    for (int i = 0; i < nthreads; i++) {
        pthread_join(threads[i], NULL);
        stores += state[i].stores;
        aborted += state[i].restarts;
    }
    qsort(t, CALLS, sizeof t[0], compare_doubles);
    printf("%-34s %8.2f %8.2f %8.2f", label, t[CALLS / 2], t[CALLS * 99 / 100], t[CALLS - 1]);
    if (busy && nthreads)
        printf("   %lu restarts in %.0fM stores", aborted, (double)stores / 1e6);
    printf("\n");
}

static void run_barrier(void) {
    printf("\n== pinning ==\n");
    uint8_t *obj = new_object(64);
    enum { PAIRS = 10000000 };
    double start = now();
    for (int i = 0; i < PAIRS; i++) {
        __atomic_fetch_or(flag_byte(obj), PINNED, __ATOMIC_SEQ_CST);
        __atomic_fetch_and(flag_byte(obj), (uint8_t)~PINNED, __ATOMIC_RELEASE);
    }
    printf("pin and unpin, uncontended: %.2f ns per pair\n", (now() - start) * 1e9 / PAIRS);

    printf("\n== membarrier(PRIVATE_EXPEDITED_RSEQ): microseconds per call ==\n");
    printf("%-34s %8s %8s %8s\n", "other threads", "median", "p99", "max");
    char label[64];
    time_membarrier("none", 0, 0);
    static const int counts[] = {1, 2, 5, 13};
    for (size_t i = 0; i < sizeof counts / sizeof counts[0]; i++) {
        snprintf(label, sizeof label, "%d running store loops", counts[i]);
        time_membarrier(label, counts[i], 1);
    }
    time_membarrier("13 asleep", 13, 0);
    printf("(CPUs 1-5 are performance cores, 6-13 efficiency cores; this thread is on CPU 0)\n");
}

// --- verify ----------------------------------------------------------------------------

enum mode { FULL, NOBARRIER, NOPIN };
static const char *mode_names[] = {"full", "nobarrier", "nopin"};

struct writer {
    int cpu;
    uint8_t *obj;
    uint32_t widen;
    int cmp;
    unsigned long stores, restarts, waits;
};

static void *writer_main(void *arg) {
    struct writer *w = arg;
    pin_cpu(w->cpu);
    struct rseq *rs = rseq_area();
    uint32_t *buf = (uint32_t *)w->obj;
    uint64_t x = (uint64_t)w->cpu * 7 + 1;
    uint32_t value = (uint32_t)w->cpu << 24;
    unsigned long stores = 0;
    while (!stop_threads) {
        for (int i = 0; i < 256; i++) {
            LCG(x);
            uint32_t *p = buf + (x >> 54);
            value++;
            if (w->cmp && w->widen)
                rseqc_store32_widened(rs, w->obj, p, value, w->widen);
            else if (w->cmp)
                rseqc_store32(rs, w->obj, p, value);
            else if (w->widen)
                rseq_store32_widened(rs, w->obj, p, value, w->widen);
            else
                rseq_store32(rs, w->obj, p, value);
        }
        stores += 256;
    }
    w->stores = stores;
    w->restarts = restarts;
    w->waits = pinned_waits;
    return NULL;
}

// Writers store continuously to a 4 KiB object. The borrower pins it, takes a snapshot,
// holds it for a while, and checks nothing changed: a change is a store that landed while
// native code had exclusive access. Only `full` should never see one.
static int run_verify(enum mode mode, uint32_t widen, int cmp, int borrows) {
    enum { WRITERS = 4, WORDS = 1024 };
    uint8_t *obj = new_object(WORDS * 4);
    uint32_t *buf = (uint32_t *)obj;
    pthread_t threads[WRITERS];
    struct writer writers[WRITERS];
    stop_threads = 0;
    for (int i = 0; i < WRITERS; i++) {
        writers[i] = (struct writer){.cpu = 1 + i, .obj = obj, .widen = widen, .cmp = cmp};
        pthread_create(&threads[i], NULL, writer_main, &writers[i]);
    }
    spin_for(0.02);
    static uint32_t snapshot[WORDS];
    int violating = 0;
    unsigned long changed = 0;
    for (int b = 0; b < borrows; b++) {
        if (mode != NOPIN)
            __atomic_fetch_or(flag_byte(obj), PINNED, __ATOMIC_SEQ_CST);
        if (mode == FULL && membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ) != 0) {
            perror("membarrier");
            exit(1);
        }
        memcpy(snapshot, buf, sizeof snapshot);
        spin_for(2e-6);
        if (memcmp(snapshot, buf, sizeof snapshot) != 0) {
            violating++;
            for (int i = 0; i < WORDS; i++)
                changed += snapshot[i] != buf[i];
        }
        if (mode != NOPIN)
            __atomic_fetch_and(flag_byte(obj), (uint8_t)~PINNED, __ATOMIC_RELEASE);
        spin_for(2e-6);
    }
    stop_threads = 1;
    unsigned long stores = 0, aborted = 0, waits = 0;
    for (int i = 0; i < WRITERS; i++) {
        pthread_join(threads[i], NULL);
        stores += writers[i].stores;
        aborted += writers[i].restarts;
        waits += writers[i].waits;
    }
    printf("%-5s %-10s widen %-4u %6d of %d borrows saw a store (%lu words); writers: %.1fM stores, "
           "%lu restarts, %lu waits\n",
           cmp ? "cmp" : "store", mode_names[mode], widen, violating, borrows, changed, (double)stores / 1e6,
           aborted, waits);
    free(obj - 64);
    return violating;
}

// --- main ----------------------------------------------------------------------------

static void setup(void) {
    if (__rseq_size == 0) {
        fputs("glibc didn't register rseq for this thread (glibc.pthread.rseq=0?)\n", stderr);
        exit(1);
    }
    if (membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ) != 0) {
        perror("membarrier register");
        exit(1);
    }
    pin_cpu(0);
    if ((int)rseq_area()->cpu_id != sched_getcpu()) {
        fputs("the rseq area doesn't track this thread's CPU\n", stderr);
        exit(1);
    }
    // One section, to fail early if the kernel rejects it.
    uint8_t *obj = new_object(16);
    rseq_store32(rseq_area(), obj, (uint32_t *)obj, 1);
    free(obj - 64);
}

int main(int argc, char **argv) {
    const char *what = argc > 1 ? argv[1] : "all";
    setup();
    int all = strcmp(what, "all") == 0;
    if (all || strcmp(what, "stores") == 0)
        run_stores();
    if (all || strcmp(what, "native") == 0)
        run_native();
    if (all || strcmp(what, "barrier") == 0)
        run_barrier();
    if (all) {
        printf("\n== verify: does a store ever land during a borrow? ==\n");
        int full_failures = 0;
        for (int cmp = 0; cmp <= 1; cmp++)
            for (int widen = 0; widen <= 200; widen += 200)
                for (int mode = FULL; mode <= NOPIN; mode++) {
                    int v = run_verify((enum mode)mode, (uint32_t)widen, cmp, 20000);
                    if (mode == FULL)
                        full_failures += v;
                }
        if (full_failures) {
            printf("FAIL: the full protocol let stores through\n");
            return 1;
        }
    } else if (strcmp(what, "verify") == 0) {
        enum mode mode = FULL;
        for (int m = FULL; m <= NOPIN; m++)
            if (argc > 2 && strcmp(argv[2], mode_names[m]) == 0)
                mode = (enum mode)m;
        uint32_t widen = argc > 3 ? (uint32_t)atoi(argv[3]) : 0;
        int cmp = argc > 4 && strcmp(argv[4], "cmp") == 0;
        return run_verify(mode, widen, cmp, 20000) && mode == FULL;
    }
    return 0;
}
