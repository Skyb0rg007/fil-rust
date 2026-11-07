#include <stdarg.h>
#include <stddef.h>
#include <stdlib.h>
struct node { struct node* next; long value; };
static long counter = 7;
struct node* head;
long add(long a, long b) { return a + b + counter; }
long sum_list(struct node* n) { long s = 0; while (n) { s += n->value; n = n->next; } return s; }
void push(long v) { struct node* n = malloc(sizeof *n); n->value = v; n->next = head; head = n; }
long total(void) { return sum_list(head); }
static long apply(long (*f)(long), long v) { return f(v); }
static long twice(long v) { return v * 2; }
long fnptr(long v) { return apply(twice, v); }
long escape(void) { long local[4] = {1, 2, 3, 4}; long* volatile p = local; return p[2]; }
long vsum(int n, ...) { va_list ap; va_start(ap, n); long s = 0; for (int i = 0; i < n; i++) s += va_arg(ap, long); va_end(ap); return s; }
long vcall(void) { return vsum(3, 1L, 2L, 3L); }
long oob_read(long i) { long* p = malloc(4 * sizeof(long)); return p[i]; }
long use_after_free(void) { long* volatile p = malloc(sizeof(long)); *p = 1; free(p); return *p; }
void double_free(void) { void* volatile p = malloc(8); free(p); free(p); }
long wild_pointer(long address) { return *(long* volatile)address; }
long wrong_signature(void) { long (*volatile f)(long, long, long) = (long (*)(long, long, long))add; return f == 0 ? 0 : ((long (*)(long))f)(1); }
long recurse(long n) { volatile long frame[64]; frame[0] = n; return n ? recurse(n - 1) + frame[0] : 0; }
