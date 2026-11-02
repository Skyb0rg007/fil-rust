/* Fixture for tests/misuse.rs: C code that hands Rust questionable values. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

int small_global = 7;
const int const_global = 3;
const char *const_string = "constant";
int table[4] = {1, 2, 3, 4};

int twice(int x) { return 2 * x; }
int call_it(int (*f)(int), int x) { return f(x); }

void free_it(void *p) { free(p); }
void *alloc_n(size_t n) { return calloc(1, n); }

/* Integers cast to pointers carry no capability. */
void *int_to_ptr(uintptr_t x) { return (void *)x; }
int (*int_to_fn(uintptr_t x))(int) { return (int (*)(int))x; }

/* A pointer whose bytes were copied as an integer: no capability either. */
struct holder {
    void *p;
    long n;
};
void holder_launder(struct holder *h) {
    uintptr_t bits;
    memcpy(&bits, &h->p, sizeof bits);
    h->p = (void *)bits;
}

/* Out-of-bounds and freed pointers handed back to Rust. */
int *past_the_end(void) {
    int *a = malloc(4 * sizeof(int));
    return a + 10;
}
char *freed_string(void) {
    char *s = strdup("gone");
    free(s);
    return s;
}
/* (16 bytes: smaller allocations are rounded up, and the slack is zero.) */
char *unterminated(void) {
    char *s = malloc(16);
    memcpy(s, "abcdefghijklmnop", 16);
    return s;
}
