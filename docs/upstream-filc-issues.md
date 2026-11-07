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

A fix is on the `publish-environ-after-copy` branch of the fork
(`~/Projects/Forks/fil-c`, commit `18e1e2475c99`, based on `upstream/deluge`
at `fa2e66b3d25d`). Compiled on its own into a harness that reads the
environment from a second thread during the call, the original code misses
on most reads that land during the copy and the fix on none.

Related, minor: the same function sets `argv[1] = NULL` on the caller's
`argv`, which an embedder passing its own array may not expect.

## 2. `sigaction` refuses every fault signal, even to set `SIG_DFL`

`filc_native_zsys_sigaction`, `filc_runtime.c:8866`:

```c
filc_ptr user_handler = filc_load_ptr_at(my_thread, act_ptr, &user_act->sa_handler);
if (is_unsafe_signal_for_handlers(signum) && filc_ptr_ptr(act_ptr) != SIG_DFL) {
    filc_set_errno(ENOSYS);
    return -1;
}
```

This compares `act_ptr`, the pointer to the `struct sigaction`, which can't be
null here, instead of `user_handler`. So asking for the default action of
`SIGSEGV`, `SIGBUS`, `SIGFPE`, `SIGILL` or `SIGTRAP`, which is already what's in
effect and is plainly allowed, fails:

```c
signal(SIGSEGV, SIG_DFL);  /* returns SIG_ERR, errno = ENOSYS */
```

Programs that reset dispositions at startup, as sandboxes and crash reporters
do, see an error. The fix is to compare `filc_ptr_ptr(user_handler)`. On the
next branch, `is_special_signal_handler` then already takes the right path.
`faults_can_be_left_at_default` in `crates/fil-rust/tests/fixtures/signals.c`
reproduces it, returning -1 on libpizlo.

## 3. Changing a disposition with a signal queued fails an assertion

`lookup_and_call_signal_handler_with_mask`, `filc_runtime.c:1180`, asserts that
the signal's table entry is set:

```c
filc_signal_handler* handler = filc_signal_table[info->si_signo];
PAS_ASSERT(handler);
```

`filc_native_zsys_sigaction` sets that entry to `NULL` for `SIG_DFL` and
`SIG_IGN`, and doesn't deal with signals already queued. It even makes that
more likely: `filc_increase_special_signal_deferral_depth` moves a pending
`FILC_THREAD_STATE_DEFERRED_SIGNAL` into `have_deferred_signal_special`, so the
`filc_exit` inside `sigaction` doesn't run the old handler, and the bit comes
back once the table has changed. The next pollcheck then panics:

```
filc panic: src/libpas/filc_runtime.c:1180: void lookup_and_call_signal_handler_with_mask(filc_thread *, siginfo_t *): assertion handler failed
```

A signal only has to land on an entered thread just before it calls `signal()`,
so this is a narrow race in an ordinary program. To reproduce it reliably,
`disposition_changes_under_a_queued_signal` in the same fixture holds the signal
back with `zincrement_signal_deferral_depth` across the `signal()` call. Running
the handlers before raising the depth, as `zsys_sigaction` in
`crates/fil-rust-rt/src/signal.rs` does, covers what's already queued. For what arrives during the call, a null
entry needs to mean the signal is dropped rather than a failed assertion.
