// Test fixture: a grab bag of C functions exercising the calling convention,
// memory, globals, variadics and callbacks.

#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// ---- scalars ----

int add(int a, int b) { return a + b; }
unsigned char add_u8(unsigned char a, unsigned char b) { return a + b; }
short neg_i16(short a) { return -a; }
long long mul_i64(long long a, long long b) { return a * b; }
unsigned long long max_u64(void) { return UINT64_MAX; }
double scale(double x, float factor) { return x * factor; }
float halve(float x) { return x / 2; }
_Bool is_even(int x) { return x % 2 == 0; }
void do_nothing(void) {}

long sum12(signed char a, short b, int c, long d, float e, double f, unsigned char g,
           unsigned short h, unsigned i, unsigned long j, int k, long l) {
    return a + b + c + d + (long)e + (long)f + g + h + i + j + k + l;
}

double mix(int a, double b, long c, float d, signed char e, double f) {
    return a + b + c + d + e + f;
}

// ---- strings and buffers ----

size_t my_strlen(const char *s) { return strlen(s); }

char *dup_upper(const char *s) {
    char *r = strdup(s);
    for (char *p = r; *p; p++)
        if (*p >= 'a' && *p <= 'z') *p -= 32;
    return r;
}

void fill(char *buf, size_t n, char c) { memset(buf, c, n); }

int sum_bytes(const unsigned char *buf, size_t n) {
    int s = 0;
    for (size_t i = 0; i < n; i++) s += buf[i];
    return s;
}

void reverse_ints(int *a, size_t n) {
    for (size_t i = 0; i < n / 2; i++) {
        int t = a[i];
        a[i] = a[n - 1 - i];
        a[n - 1 - i] = t;
    }
}

int read_at(const int *a, long i) { return a[i]; }

const char *string_literal(void) { return "a string literal"; }

int format_into(char *buf, size_t n, int x, const char *s) {
    return snprintf(buf, n, "%d:%s", x, s);
}

// ---- pointers to pointers ----

char **split_words(const char *s, int *count) {
    char *copy = strdup(s);
    int cap = 4, n = 0;
    char **out = malloc(sizeof(char *) * cap);
    char *save;
    for (char *tok = strtok_r(copy, " ", &save); tok; tok = strtok_r(NULL, " ", &save)) {
        if (n + 1 >= cap) out = realloc(out, sizeof(char *) * (cap *= 2));
        out[n++] = strdup(tok);
    }
    out[n] = NULL;
    free(copy);
    *count = n;
    return out;
}

size_t total_len(char **strs) {
    size_t t = 0;
    for (; *strs; strs++) t += strlen(*strs);
    return t;
}

// ---- structs ----

struct pt { int x, y; };
struct pt mkpt(int x, int y) { struct pt p = {x, y}; return p; }
int pt_sum(struct pt p) { return p.x + p.y; }
struct pt pt_add(struct pt a, struct pt b) { struct pt r = {a.x + b.x, a.y + b.y}; return r; }
void pt_scale(struct pt *p, int k) { p->x *= k; p->y *= k; }

struct vec2 { double x, y; };
struct vec2 vec2_rot90(struct vec2 v) { struct vec2 r = {-v.y, v.x}; return r; }

struct mixed { double d; int i; };
struct mixed mixed_make(double d, int i) { struct mixed m = {d, i}; return m; }
double mixed_total(struct mixed m) { return m.d + m.i; }

struct big { long a, b, c, d; };
struct big big_make(long base) { struct big r = {base, base + 1, base + 2, base + 3}; return r; }
long big_sum(struct big b) { return b.a + b.b + b.c + b.d; }
struct big big_twice(struct big b) { struct big r = {b.a * 2, b.b * 2, b.c * 2, b.d * 2}; return r; }

struct named { const char *name; int len; };
struct named named_make(const char *s) { struct named n = {s, (int)strlen(s)}; return n; }
int named_check(struct named n) { return n.name && (int)strlen(n.name) == n.len; }

struct inner { short a; signed char b; };
struct inner inner_bump(struct inner v, int k) { v.a += k; v.b += k; return v; }

// Shapes whose lowering differs between x86-64 and aarch64: homogeneous
// float aggregates (in FP registers on aarch64, even when larger than 16
// bytes) and a float sharing an eightbyte with an int.
struct rgb { float r, g, b; };
struct rgb rgb_scale(struct rgb c, float k) { struct rgb r = {c.r * k, c.g * k, c.b * k}; return r; }
struct quad { float a, b, c, d; };
float quad_dot(struct quad x, struct quad y) { return x.a * y.a + x.b * y.b + x.c * y.c + x.d * y.d; }
struct vec3 { double x, y, z; };
struct vec3 vec3_cross(struct vec3 a, struct vec3 b) {
    struct vec3 r = {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x};
    return r;
}
struct fpair { float f; int i; };
struct fpair fpair_swap(struct fpair p) { struct fpair r = {(float)p.i, (int)p.f}; return r; }
struct outer { char tag; struct inner in; int arr[3]; double d; const char *label; };
struct outer outer_make(const char *label) {
    struct outer o = {'T', {-7, 'q'}, {1, 2, 3}, 2.5, label};
    return o;
}
double outer_sum(const struct outer *o) {
    return o->tag + o->in.a + o->in.b + o->arr[0] + o->arr[1] + o->arr[2] + o->d +
           (o->label ? strlen(o->label) : 0);
}

// ---- linked data ----

struct node { int value; struct node *next; };

struct node *list_build(int n) {
    struct node *head = NULL;
    for (int i = n; i > 0; i--) {
        struct node *nd = malloc(sizeof *nd);
        nd->value = i;
        nd->next = head;
        head = nd;
    }
    return head;
}

long list_sum(const struct node *n) {
    long s = 0;
    for (; n; n = n->next) s += n->value;
    return s;
}

void list_free(struct node *n) {
    while (n) {
        struct node *next = n->next;
        free(n);
        n = next;
    }
}

// ---- globals ----

int global_counter;
const char *global_name = "fixture";
struct pt global_point = {3, 4};
int global_array[4] = {10, 20, 30, 40};

int bump(void) { return ++global_counter; }
int get_counter(void) { return global_counter; }

// ---- freeing ----

void free_it(void *p) { free(p); }
void *alloc_bytes(size_t n) { return calloc(1, n); }

static void *kept;
void keep(void *p) { kept = p; }
void *get_kept(void) { return kept; }

// ---- variadics ----

int sum_ints(int count, ...) {
    va_list ap;
    va_start(ap, count);
    int s = 0;
    for (int i = 0; i < count; i++) s += va_arg(ap, int);
    va_end(ap);
    return s;
}

double sum_doubles(int count, ...) {
    va_list ap;
    va_start(ap, count);
    double s = 0;
    for (int i = 0; i < count; i++) s += va_arg(ap, double);
    va_end(ap);
    return s;
}

int my_snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}

// ---- function pointers ----

int apply(int (*f)(int), int x) { return f(x); }
int apply_twice(int (*f)(int), int x) { return f(f(x)); }
double apply_d(double (*f)(double, double), double a, double b) { return f(a, b); }
void each(const int *a, size_t n, void (*f)(int index, int value)) {
    for (size_t i = 0; i < n; i++) f((int)i, a[i]);
}
struct pt apply_pt(struct pt (*f)(struct pt), struct pt p) { return f(p); }
struct big apply_big(struct big (*f)(struct big), struct big b) { return f(b); }
struct vec3 apply_vec3(struct vec3 (*f)(struct vec3), struct vec3 v) { return f(v); }
const char *apply_str(const char *(*f)(const char *), const char *s) { return f(s); }

int (*get_add_fn(void))(int, int) { return add; }

static int cmp_with(const void *a, const void *b, void *f) {
    int (*cmp)(const int *, const int *) = (int (*)(const int *, const int *))f;
    return cmp((const int *)a, (const int *)b);
}

static int (*g_cmp)(const int *, const int *);
static int cmp_global(const void *a, const void *b) { return g_cmp(a, b); }

void sort_ints(int *a, size_t n, int (*cmp)(const int *, const int *)) {
    (void)cmp_with;
    g_cmp = cmp;
    qsort(a, n, sizeof(int), cmp_global);
}

// ---- threads ----

struct thread_arg { int (*f)(int); int id; int iterations; long result; };

static void *thread_body(void *p) {
    struct thread_arg *arg = p;
    long s = 0;
    for (int i = 0; i < arg->iterations; i++) s += arg->f(arg->id * 1000 + i);
    arg->result = s;
    return NULL;
}

long call_from_threads(int (*f)(int), int nthreads, int iterations) {
    pthread_t *threads = malloc(sizeof(pthread_t) * nthreads);
    struct thread_arg *args = calloc(nthreads, sizeof *args);
    for (int i = 0; i < nthreads; i++) {
        args[i] = (struct thread_arg){f, i, iterations, 0};
        pthread_create(&threads[i], NULL, thread_body, &args[i]);
    }
    long total = 0;
    for (int i = 0; i < nthreads; i++) {
        pthread_join(threads[i], NULL);
        total += args[i].result;
    }
    free(threads);
    free(args);
    return total;
}

static pthread_mutex_t counter_lock = PTHREAD_MUTEX_INITIALIZER;
static long shared_total;

long locked_add(long x) {
    pthread_mutex_lock(&counter_lock);
    shared_total += x;
    long r = shared_total;
    pthread_mutex_unlock(&counter_lock);
    return r;
}

long locked_total(void) {
    pthread_mutex_lock(&counter_lock);
    long r = shared_total;
    pthread_mutex_unlock(&counter_lock);
    return r;
}

// ---- allocation churn ----

long churn(int rounds, int size) {
    long check = 0;
    for (int r = 0; r < rounds; r++) {
        char **ptrs = malloc(sizeof(char *) * 64);
        for (int i = 0; i < 64; i++) {
            ptrs[i] = malloc(size);
            memset(ptrs[i], i, size);
        }
        for (int i = 0; i < 64; i++) check += ptrs[i][size - 1];
        free(ptrs);  // the strings become garbage
    }
    return check;
}

// ---- printing ----

int say(const char *s) { return printf("%s\n", s); }
