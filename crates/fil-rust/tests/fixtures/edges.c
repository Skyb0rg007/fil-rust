/* Edge cases: variadic promotions, large copies, recursion through Rust
   callbacks, and fork. */
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* ---- varargs, read after C's default promotions ---- */

long long va_ints(int n, ...) {
    va_list ap;
    va_start(ap, n);
    long long sum = 0;
    for (int i = 0; i < n; i++) sum = sum * 1000 + va_arg(ap, int);
    va_end(ap);
    return sum;
}

unsigned long long va_uints(int n, ...) {
    va_list ap;
    va_start(ap, n);
    unsigned long long sum = 0;
    for (int i = 0; i < n; i++) sum += va_arg(ap, unsigned int);
    va_end(ap);
    return sum;
}

double va_doubles(int n, ...) {
    va_list ap;
    va_start(ap, n);
    double sum = 0;
    for (int i = 0; i < n; i++) sum += va_arg(ap, double);
    va_end(ap);
    return sum;
}

/* Alternating `char *` and `long long`, as a format string would ask. */
long long va_mixed(int n, ...) {
    va_list ap;
    va_start(ap, n);
    long long total = 0;
    for (int i = 0; i < n; i++) {
        const char *s = va_arg(ap, const char *);
        total += (long long)strlen(s) * va_arg(ap, long long);
    }
    va_end(ap);
    return total;
}

/* ---- large copies ---- */

uint64_t fnv1a(const unsigned char *p, size_t n) {
    uint64_t h = 0xcbf29ce484222325ull;
    for (size_t i = 0; i < n; i++) h = (h ^ p[i]) * 0x100000001b3ull;
    return h;
}

void fill_pattern(unsigned char *p, size_t n, unsigned seed) {
    for (size_t i = 0; i < n; i++) p[i] = (unsigned char)(i * 31 + seed + (i >> 16));
}

char *long_string(size_t n) {
    char *s = malloc(n + 1);
    for (size_t i = 0; i < n; i++) s[i] = 'a' + i % 26;
    s[n] = 0;
    return s;
}

void *block(size_t n) { return calloc(n, 1); }
void release(void *p) { free(p); }

/* ---- recursion through a Rust callback ---- */

static int (*stored)(int);
void store_callback(int (*cb)(int)) { stored = cb; }

/* down(n) = stored(n - 1) + 1, where the Rust callback calls down again. */
int down(int n) {
    volatile char frame[256];
    frame[0] = (char)n;
    if (n <= 0) return frame[0];
    return stored(n - 1) + 1;
}

/* ---- fork from Fil-C ---- */

int fork_and_exit(int code) {
    pid_t pid = fork();
    if (pid < 0) return -1;
    if (pid == 0) _exit(code);
    int status = 0;
    if (waitpid(pid, &status, 0) != pid) return -2;
    return WIFEXITED(status) ? WEXITSTATUS(status) : -3;
}

/* ---- a global, to tell copies of the library apart ---- */

int shared_counter;
int bump_shared(void) { return ++shared_counter; }
