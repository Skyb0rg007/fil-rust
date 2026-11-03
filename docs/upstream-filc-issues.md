# Fil-C issues found while building fil-rust

Problems in Fil-C itself (not in fil-rust) found along the way, written up
for reporting upstream. Line numbers are for the
[v0.687](https://github.com/pizlonator/fil-c/tree/v0.687) sources; "musl" is
the pizfix binary distribution, "glibc" the optfil one, both 0.687, on
x86-64 Linux.

## 1. `filc_setproctitle_initialize` publishes the new `environ` before filling it

`libpas/src/libpas/filc_setproctitle.c`, called from `really_start_program`
(`filc_runtime.c:6854`):

```c
if ((environ = calloc(i + 1, sizeof(*environ))) == NULL) {   /* line 76 */
...
for (i = 0; envp != NULL && envp[i] != NULL; i++)
    environ[i] = strdup(envp[i]);                             /* line 105 */
```

`environ` points at the zeroed array before any entry is copied, so for the
duration of the copy loop the process environment looks empty or truncated
to any other thread. In a normal Fil-C program nothing else runs yet, but
when `filc_start_program` is called by a host that already has threads (an
embedder; here, a Rust program calling into Fil-C), a concurrent `getenv`
returns NULL for variables that are set.

Observed: a thread looping on `getenv("FILC_ROOT")` while another thread
calls `filc_start_program` misses 50-170 times per start (out of ~200k
reads); the environment afterwards is identical, only the array moved from
the stack to the heap.

Suggested fix: fill the new array first, then assign `environ` once (with a
release store), e.g.

```c
char **newenv = calloc(i + 1, sizeof(*newenv));
if (!newenv) return;
for (i = 0; envp[i]; i++) newenv[i] = strdup(envp[i]);
newenv[i] = NULL;
__atomic_store_n(&environ, newenv, __ATOMIC_RELEASE);
```

Related, minor: the same function sets `argv[1] = NULL` on the caller's
`argv`, which an embedder passing its own array may not expect.

## 2. A use racing with `free` can be reported as out-of-bounds instead of use-after-free

`free_impl` (`filc_runtime.c:3169`) shrinks the object first and marks it
freed afterwards:

```c
object->upper = filc_object_lower_not_null(object);   /* line 3180 */
for (;;) {
    ... pas_compare_and_swap_uintptr_weak(..., flags | FILC_OBJECT_FLAG_FREE, ...)
}
```

An access on another thread that lands between the two sees
`ptr >= upper` without the FREE flag and fails with a bounds error rather
than a use-after-free error. Safety isn't affected (the access is refused
either way), only the diagnostic. Seen with an 8 MiB copy into an object
while another thread frees it: some runs report out-of-bounds. Setting the
flag before shrinking (or having the bounds-failure path recheck the flag,
which is what fil-rust's shim does) would report it as freed.

## 3. Reading one variadic argument too many isn't caught after an even number of 8-byte arguments

Pure C, no fil-rust involved; same result with the musl and glibc
distributions, at `-O0`:

```c
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
static long sum(int want, ...) {
    va_list ap; va_start(ap, want);
    long s = 0;
    for (int i = 0; i < want; i++) s += va_arg(ap, long);
    va_end(ap);
    return s;
}
int main(int argc, char **argv) {
    long r;
    switch (atoi(argv[1])) {
    case 1: r = sum(2, 1L); break;             /* passes 1, reads 2 */
    case 2: r = sum(3, 1L, 2L); break;         /* passes 2, reads 3 */
    case 3: r = sum(4, 1L, 2L, 3L); break;     /* passes 3, reads 4 */
    case 4: r = sum(5, 1L, 2L, 3L, 4L); break; /* passes 4, reads 5 */
    default: return 2;
    }
    printf("no trap, sum %ld\n", r);
}
```

| arguments passed | read | result |
|---|---|---|
| 1 | 2 | `filc safety error: cannot read pointer with ptr >= upper` |
| 2 | 3 | no trap, the extra argument reads as 0 (`sum 3`) |
| 3 | 4 | `filc safety error` |
| 4 | 5 | no trap, reads 0 (`sum 10`) |

So the variadic argument area appears to be sized with padding that
`va_arg` is allowed to read: with an even count of 8-byte arguments, one
read past the last argument returns zero instead of trapping. It isn't a
memory-safety hole (the padding is part of the allocation and zeroed),
but it's an argument-count mismatch that Fil-C otherwise diagnoses.
