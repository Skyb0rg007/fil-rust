# Fil-C issues found while building fil-rust

Problems in Fil-C itself (not in fil-rust) found along the way, written up
for reporting upstream. Only behavior that's wrong for a correct program
belongs here; Fil-C trapping undefined behavior differently than expected
(or not trapping UB that stays memory-safe) doesn't. Line numbers are for the
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

Upstream may consider this outside what `filc_start_program` supports (it
expects to run at program start, before there are other threads), but the
fix is cheap and makes embedding safe. Suggested fix: fill the new array first, then assign `environ` once (with a
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
