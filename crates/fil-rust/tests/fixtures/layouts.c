/* Fixture for tests/layout.rs: struct shapes whose layout and calling
 * convention Rust's #[derive(FilType)] must reproduce exactly. For each
 * struct S:
 *   S_size, S_align   sizeof and alignof
 *   S_fill(p)         writes the reference value
 *   S_check(p)        1 if *p is the reference value
 *   S_bump(v)         v with every field changed, by value both ways
 *   S_via(f, v)       f(v), with f a function pointer (a Rust callback)
 */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

int anchor = 99;
int inc(int x) { return x + 1; }

#define SHAPE(S, FILL, CHECK, BUMP)                                         \
    const size_t S##_size = sizeof(struct S);                               \
    const size_t S##_align = _Alignof(struct S);                            \
    void S##_fill(struct S *p) { FILL }                                     \
    int S##_check(const struct S *p) { return CHECK; }                      \
    struct S S##_bump(struct S v) { struct S *p = &v; BUMP return v; }      \
    struct S S##_via(struct S (*f)(struct S), struct S v) { return f(v); }

struct padded { char a; double b; char c; };
SHAPE(padded,
      p->a = 'a'; p->b = 2.5; p->c = 'c';,
      p->a == 'a' && p->b == 2.5 && p->c == 'c',
      p->a++; p->b *= 2; p->c++;)

struct shorts { char a[3]; short b; };
SHAPE(shorts,
      p->a[0] = 1; p->a[1] = 2; p->a[2] = 3; p->b = -300;,
      p->a[0] == 1 && p->a[1] == 2 && p->a[2] == 3 && p->b == -300,
      p->a[0]++; p->a[1]++; p->a[2]++; p->b--;)

struct floats2 { float a; float b; };
SHAPE(floats2,
      p->a = 1.5f; p->b = -0.25f;,
      p->a == 1.5f && p->b == -0.25f,
      p->a += 1; p->b += 1;)

struct doubles5 { double a, b, c, d, e; };
SHAPE(doubles5,
      p->a = 1; p->b = 2; p->c = 3; p->d = 4; p->e = 5;,
      p->a == 1 && p->b == 2 && p->c == 3 && p->d == 4 && p->e == 5,
      p->a += 10; p->b += 10; p->c += 10; p->d += 10; p->e += 10;)

struct ptr_mid { int8_t a; int *p; int8_t b; };
SHAPE(ptr_mid,
      p->a = -1; p->p = &anchor; p->b = 1;,
      p->a == -1 && p->p == &anchor && *p->p == 99 && p->b == 1,
      p->a--; p->b++; *p->p += 0;)

struct nested { struct shorts in; uint64_t x; bool flag; };
SHAPE(nested,
      shorts_fill(&p->in); p->x = 0x1122334455667788ull; p->flag = true;,
      shorts_check(&p->in) && p->x == 0x1122334455667788ull && p->flag,
      p->in = shorts_bump(p->in); p->x++; p->flag = !p->flag;)

struct u16x5 { uint16_t a[5]; };
SHAPE(u16x5,
      for (int i = 0; i < 5; i++) p->a[i] = 1000 * (i + 1);,
      p->a[0] == 1000 && p->a[1] == 2000 && p->a[2] == 3000 && p->a[3] == 4000 && p->a[4] == 5000,
      for (int i = 0; i < 5; i++) p->a[i] += 1;)

struct one { char c; };
SHAPE(one,
      p->c = 'z';,
      p->c == 'z',
      p->c--;)

struct with_fn { int (*fn)(int); int x; };
SHAPE(with_fn,
      p->fn = inc; p->x = 41;,
      p->fn != 0 && p->fn(p->x) == 42,
      p->x = p->fn(p->x);)

struct points { struct floats2 pts[3]; };
SHAPE(points,
      for (int i = 0; i < 3; i++) { p->pts[i].a = i; p->pts[i].b = -i; },
      p->pts[0].a == 0 && p->pts[1].a == 1 && p->pts[2].a == 2 && p->pts[2].b == -2,
      for (int i = 0; i < 3; i++) p->pts[i] = floats2_bump(p->pts[i]);)

struct mixed_ints { uint8_t a; uint32_t b; uint16_t c; uint64_t d; int8_t e; };
SHAPE(mixed_ints,
      p->a = 200; p->b = 4000000000u; p->c = 60000; p->d = UINT64_MAX; p->e = -100;,
      p->a == 200 && p->b == 4000000000u && p->c == 60000 && p->d == UINT64_MAX && p->e == -100,
      p->a++; p->b++; p->c++; p->d--; p->e--;)

struct int_float { int32_t i; float f; };
SHAPE(int_float,
      p->i = -7; p->f = 3.5f;,
      p->i == -7 && p->f == 3.5f,
      p->i *= 2; p->f *= 2;)

struct three_ptrs { const char *a; const char *b; const char *c; };
SHAPE(three_ptrs,
      p->a = "a"; p->b = "bb"; p->c = "ccc";,
      p->a[0] == 'a' && p->b[1] == 'b' && p->c[2] == 'c',
      { const char *t = p->a; p->a = p->c; p->c = t; })
