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
long escape(void) { long local[4] = {1, 2, 3, 4}; long* volatile p = local; head = (struct node*)0; return p[2]; }
long vsum(int n, ...) { va_list ap; va_start(ap, n); long s = 0; for (int i = 0; i < n; i++) s += va_arg(ap, long); va_end(ap); return s; }
long vcall(void) { return vsum(3, 1L, 2L, 3L); }
