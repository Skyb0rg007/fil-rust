#include <stdarg.h>
#include <stddef.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <string.h>

struct pair { long* a; long* b; };

static long storage[4] = {10, 20, 30, 40};

/* Constant relocations: pointers in initialized data. */
static long* table[3] = {&storage[0], &storage[2], &storage[3]};
static struct pair pairs[2] = {{&storage[0], &storage[1]}, {&storage[2], &storage[3]}};
long relocated(void) { return *table[0] + *table[1] + *table[2] + *pairs[1].a; }

/* A global that refers to itself. */
struct cycle { struct cycle* next; long value; };
extern struct cycle second;
struct cycle first = {&second, 1};
struct cycle second = {&first, 2};
long cycle_sum(void) { return first.next->value + first.next->next->value; }

/* Copying pointers keeps their capabilities. */
long copy_pointers(long n) {
    long** from = malloc(n * sizeof(long*));
    long** to = malloc(n * sizeof(long*));
    for (long i = 0; i < n; i++) {
        from[i] = malloc(sizeof(long));
        *from[i] = i;
    }
    memcpy(to, from, n * sizeof(long*));
    long sum = 0;
    for (long i = 0; i < n; i++)
        sum += *to[i];
    return sum;
}

/* An overlapping move, in both directions. */
long overlap(long up) {
    long a = 1, b = 2, c = 3;
    long** words = malloc(4 * sizeof(long*));
    words[0] = &a; words[1] = &b; words[2] = &c; words[3] = NULL;
    if (up) {
        memmove(words + 1, words, 3 * sizeof(long*));
        return *words[1] * 100 + *words[2] * 10 + *words[3];
    }
    memmove(words, words + 1, 2 * sizeof(long*));
    return *words[0] * 10 + *words[1];
}

/* memset over part of a pointer takes its capability away. The size is
   volatile to keep this a real memset: a single byte store leaves the
   capability alone, and the changed address is what fails then. */
long torn(void) {
    long value = 5;
    long** slot = malloc(sizeof(long*));
    volatile size_t one = 1;
    *slot = &value;
    memset((char*)slot + 2, 0, one);
    return **(long* volatile*)slot;
}

/* Bytes copied out of phase with the word don't make a pointer either. */
long out_of_phase(void) {
    long value = 5;
    char* buffer = malloc(4 * sizeof(long*));
    long** words = (long**)buffer;
    words[0] = &value;
    memmove(buffer + 3 + sizeof(long*), buffer, sizeof(long*));
    memmove(buffer + 2 * sizeof(long*), buffer + 3 + sizeof(long*), sizeof(long*));
    return *words[2];
}

/* Structs with pointers, through the stack and through small memcpys. */
struct big { long* p[7]; long n; };
static struct big make_big(long* v) {
    struct big b;
    for (int i = 0; i < 7; i++) b.p[i] = v + i;
    b.n = 7;
    return b;
}
static long sum_big(struct big b) {
    long s = 0;
    for (long i = 0; i < b.n; i++) s += *b.p[i];
    return s;
}
long by_value(void) {
    long v[7] = {1, 2, 3, 4, 5, 6, 7};
    struct big b = make_big(v);
    struct big* heap = malloc(sizeof b);
    memcpy(heap, &b, sizeof b);
    struct big back = *heap;
    return sum_big(back);
}

/* Pointers in varargs. */
static long sum_ptrs(int n, ...) {
    va_list ap;
    va_start(ap, n);
    long s = 0;
    for (int i = 0; i < n; i++) s += *va_arg(ap, long*);
    va_end(ap);
    return s;
}
long vararg_pointers(void) { long a = 3, b = 4, c = 5; return sum_ptrs(3, &a, &b, &c); }

/* Pointer atomics, which go through boxes. */
long atomics(void) {
    long a = 1, b = 2, c = 3;
    _Atomic(long*)* slot = malloc(sizeof *slot);
    atomic_store(slot, &a);
    long s = *atomic_load(slot);
    long* expected = &a;
    s = s * 10 + atomic_compare_exchange_strong(slot, &expected, &b);
    s = s * 10 + *atomic_load(slot);
    expected = &a;
    s = s * 10 + atomic_compare_exchange_strong(slot, &expected, &c);
    s = s * 10 + *expected;
    s = s * 10 + *atomic_exchange(slot, &c);
    s = s * 10 + *atomic_load(slot);
    /* A plain copy of the word still carries the capability. */
    long** copy = malloc(sizeof *copy);
    memcpy(copy, slot, sizeof *copy);
    return s * 10 + **copy;
}

/* Larger allocations, past the sizes compiled code handles inline. */
long sizes(void) {
    long total = 0;
    size_t size = 8;
    for (int i = 0; i < 18; i++) {
        long* p = malloc(size);
        p[size / sizeof(long) - 1] = i;
        total += p[size / sizeof(long) - 1];
        free(p);
        size *= 2;
    }
    return total;
}

long aligned(void) {
    long* p;
    _Alignas(256) static long over[2];
    over[1] = 9;
    if ((size_t)&over[0] % 256) return -1;
    return over[1];
}

/* Thread locals. */
static _Thread_local long tls_value;
static _Thread_local long* tls_pointer;
long thread_local_variables(long value) {
    tls_value = value;
    tls_pointer = &tls_value;
    return *tls_pointer + 1;
}

/* Constructors run, and they run once. */
static long ctor_runs;
static long* ctor_pointer;
__attribute__((constructor)) static void note_ctor(void) { ctor_runs++; ctor_pointer = &ctor_runs; }
long ctors(void) { return *ctor_pointer; }

/* Arguments past the inline part of the buffer. */
long many_args(long a00, long a01, long a02, long a03, long a04, long a05, long a06, long a07,
               long a08, long a09, long a10, long a11, long a12, long a13, long a14, long a15,
               long a16, long a17, long a18, long a19, long a20, long a21, long a22, long a23,
               long a24, long a25, long a26, long a27, long a28, long a29, long a30, long a31,
               long a32, long a33, long a34, long a35, long a36, long a37, long a38, long a39) {
    return a00 + a10 + a20 + a30 + a39;
}
struct huge { long v[60]; };
struct huge huge_return(long seed) {
    struct huge h;
    for (int i = 0; i < 60; i++) h.v[i] = seed + i;
    return h;
}
long huge_call(void) { struct huge h = huge_return(1); return h.v[59]; }
