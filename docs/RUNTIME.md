# A Rust runtime for Fil-C libraries

Status: in progress on `library-runtime`. This is the plan and its running findings.

## Context

fil-rust currently hosts the whole Fil-C system: stock `libpizlo.so` (libpas, FUGC's
concurrent collector, handshakes), the user libc, a boot thread and executor threads.
Upstream is taking the runtime toward being always in control, with no escape and a
required concurrent GC. That conflicts with what fil-rust wants to be.

Goal: fil-rust becomes a **library**. A Rust program loads existing Fil-C-compiled `.so`
files, unmodified, with no libpizlo, no libpas and no FUGC. The collector is written in Rust
and runs only when Rust says so, so Fil-C can be added to a Rust system a piece at a time.

Honest scope of "no extra threads": a library that imports nothing from libc runs on the
caller's own thread with no boot thread or executors (**bare mode**). Most real libraries
call `strlen` or `printf`, so they use **libc mode**: unmodified `libc.so`, one boot thread,
and executors, but still no libpizlo, no collector thread, and no GC unless Rust asks.

Decisions already made:

- Replace libpizlo with a **Rust runtime**. The Fil-C compiler stays.
- **Tracing GC in Rust**, stop-the-world and non-moving. A true no-GC mode (zig#36237 style
  P^I reuse) is **future work, after compiler changes**. Not built here.
- **Fil-C user libc is optional**, loaded on demand and unmodified.
- libpizlo stays only as a **differential oracle** during the branch and is **removed at
  parity**.

## What the research established

Verified from `nm`/`readelf`, source reading and an independent review. Nothing was run yet;
marked items need a spike (M0).

- **ABI surface.** The pass declares 64 helpers; libpizlo exports 63 (`filc_pollcheck` is
  rewritten away). Musl objects import 42 functions plus one data symbol,
  `filc_current_marking_state`; glibc 55 plus that one. Natives number 519 (musl) and 531
  (glibc). No `pizlonatedFI*/ET*` are imported, and natives work with `signature = 0`.
- **Stub feasibility.** No custom calling conventions, IFUNC exports, imported TLS or copy
  relocations. `jmp *table` preserves argument registers. Soname matching works as the
  compat stand-ins do today.
- **ABI is becoming stable.** Upstream names it `fil1` and plans to freeze x86-64
  ([issue](https://github.com/pizlonator/fil-c/issues/279)), not enforced yet. We pin 0.687.
- **`my_thread` is argument 0**, never TLS. Nine helpers take no thread and must find it;
  `filc_defer_or_run_global_ctor` can run before any thread exists.
- **Offsets are fixed:** `stack_limit` 0, `state` 8, `tid` 12, `top_frame` 16,
  `unwind_registers` 32, cookie 64, `cc_inline_buffer` 128, aux 384, outline 640/648/656,
  allocators 3072+, stride 208 (x86-64) or 568 (aarch64). A real special-object header sits
  at `thread - 16`.
- **Allocator slots are opaque.** `verse_local_allocator_allocate(slot)` must never return
  null and never safepoint.
- **Barrier and pollcheck go dead** with `filc_current_marking_state == 0` and
  `state & 0xE == 0`. Stop-the-world needs only the Stop bit.
- **Roots are precise; no conservative fallback.** CC buffers, return registers and
  `unwind_registers` are invisible, so collection may run only at safepoints.
- **Dead objects keep their header** (`upper == lower`, FREE set) until a trace proves them
  unreachable. A zeroed or recycled header passes compiled checks.
- **Allocas are precisely tracked.** Compiled code never frees them; the collector does.
  Escaping allocas are ordinary heap objects on malloc's path. Stack-kept locals publish their
  lowers through frame slots and `filc_stack_aux`. Native code tracks what it allocates through
  `filc_thread_track_object`, native frames and `allocation_roots`. Aux arrays and boxes are
  headerless, so size and marks come from chunks.
- **`malloc`/`free` are compiler-lowered** and work bare. `calloc`, `realloc`, `strlen`,
  `printf` come from libc.
- **Every fixture has `DT_NEEDED libc.so`**, even those importing nothing from it. Mode must
  be decided from imported symbols, not `DT_NEEDED`.
- **libpizlo is partly Fil-C-compiled.** `filc/src/*.c` defines 85 `pizlonated_` symbols,
  including `zversion`, `zgc_request_and_wait`, `__atomic_*`, `zerrorf`, the fd wrappers, and
  `_Unwind_*`/EH accessors. Its undefined symbols are only helpers, natives and `memset`, so
  it loads bare. `libpizlo.a` has these objects on musl; glibc ships no `.a`.
- **Libc constraints.** `__libc_start_main` never returns, and musl finds its thread through
  the cookie, so a thread with no `struct pthread` can't call libc. `struct pthread` only
  comes from libc's `pthread_create`, so libc mode keeps executors.

## Architecture

```
Rust program (host glibc)
├── fil-rust          safe API over `Shim`, plus a backend split for boot and libc-less mode
├── fil-rust-rt  NEW  object model, heap, GC, helpers, natives (extern "C")
├── memfd stubs       libyoloc stand-in (exists), generated libpizlo.so stub (new),
│                     name-only libc.so / libc.so.6666 stand-ins for bare mode (new)
├── memfd support     libfilrust_support.so: the filc/src objects, loaded in both modes
└── dlopen            unmodified foo.so, and unmodified libc.so in libc mode
```

**`libpizlo.so` stub.** Generated by `fil-rust-sys/build.rs` from symbol lists, one per flavor
and arch, derived from the pass's helper declarations plus that `libpizlo.so`'s dynsym, and
checked in. Every function is `jmp *table[i]`; Rust fills the table before any Fil-C object
loads; `filc_current_marking_state` lives in the stub. Slots for support-library symbols are
filled from `dlsym(support)`, so the stub forwards rather than shadows. Unimplemented helpers
report their name and abort. Unimplemented native getters return a function object that does
the same when called. Data getters (`__cpu_model`, `__cpu_features2`) get real objects. With
the Rust backend the stub replaces `compat_glibc_pizlo`, since they share a soname.

**Backend seam.** `Shim` (`crates/fil-rust-sys/src/lib.rs`) is a table of 37 `extern "C"`
functions; `fil-rust-rt` provides the same table plus `fr_collect` and `fr_version`.
`Config::runtime` / `FIL_RUST_RUNTIME=rust` selects the backend. Not everything is below the
seam: `Runtime::libc`, `runtime_library`, `Fil::libc`, `flush`, `version`, `collect_garbage`
and `spawn` change, as do `check_library`, the `lib_dir` libpizlo check,
`check_no_real_yolo_libc` (must also reject the real libpizlo), signal setup (`dlsym` of
`filc_get_my_thread`, `redirect_imports`) and `fr_root_*` (a Rust table the GC scans).

**Process mode is chosen before any `Fil` exists.**

```rust
pub enum Libc { Auto, Always, Never }   // Config::libc, default Auto
```

- `Runtime::load` scans the object's imports first, outside `run`. In `Auto`, an object
  needing libc boots libc mode, draining bare `run` calls first. `Fil::load` on a bare thread
  returns an error if the object needs libc, since it can't upgrade under a live `Fil`.
- Constructors go through a three-state gate: run immediately (bare), defer until
  `__libc_start_main` (libc mode), or already run. Running twice is fatal.
- `CURRENT` splits into "attached bare thread" and "libc executor".

**Bare threads.** A thread gets a `filc_thread` lazily, with a base frame, and is exited
outside `run`. An enter/exit guard covers panics. Handles flush before a collection, and
argc/argv/environ are synthesized and rooted for constructors. Teardown runs from a TLS
destructor that never takes a collection path. Multi-mutator from the start: `Runtime` is
`Copy` and `run` works from any Rust thread.

**Collector control.**

```rust
pub enum Collection {
    Never,                                      // no trace, so nothing is reclaimed
    Manual,                                     // only Runtime::collect / Fil::collect_garbage
    Threshold { min_bytes: usize, growth_percent: u32 },  // default
}
Config::collection(...); Runtime::set_collection(...);
Runtime::collect(); Runtime::heap_stats() -> HeapStats;
```

No collector thread. A collection runs on the mutator that asked, inside one function
reachable only from `filc_pollcheck_slow`, explicit collect, and exit-allowed `filc_allocate`
(M0 confirms). `Threshold` sets the Check bit by CAS. `fr_exit` never starts a collection, but
an exited thread can be collected around, so Rust-side thunks track CC pointer slots at entry
(callbacks today don't), and no lock over roots is held across a safepoint. `zgc_wait` returns
under `Never` and `Manual`. Marking is iterative and runs on a dedicated stack, since stack
slack is only 32 KiB. Rust-only allocation loops never poll; this is documented.

**Allocation tracking.** Every allocation is GC memory, known to the Rust heap from the moment
it exists, and every one is reclaimed the same way: a Rust-driven trace finds it unreachable.
There is no leak category and no separate lifetime scheme for allocas. `free` only tombstones;
the memory comes back in the next trace, like everything else.

| Memory | Tracked through | Reclaimed by |
|---|---|---|
| Stack locals with naked lowers | frame `lowers[]` slots, while live | return (they are on the stack); what they point at, by trace |
| Stack locals with explicit aux | `filc_stack_aux` in a frame slot | same |
| Escaping allocas, including lazy clones | heap block; frame lowers while in scope | trace, after return drops the last reference |
| Byval promotions, varargs snapshots | heap block (`READONLY` for args); frame lowers | trace |
| `malloc`, `zgc_alloc`, realloc | heap block | trace; `free` tombstones first, and heap refs redirect to the singleton |
| Aux arrays, atomic boxes | chunk metadata, owned by their object | trace, with their object |
| jmp_bufs, thread-locals, unwind contexts, siginfo, closures | special objects; setjmp slots, `thread_locals`, thread fields | trace, with destructors where needed |
| Allocated by Rust natives | `track` into the native frame before the next safepoint | trace, after the frame pops |

Compiled code can't tell the runtime which blocks came from an alloca, and tracing doesn't need
it to. `HeapStats` reports live, tombstoned and reclaimed bytes, and the collection that freed
them, so Rust can see this memory and decide when to reclaim it. The `zgc_*` primitives that
Fil-C code and libc use (`zgc_alloc`, `zgc_free`, `zgc_realloc`, `zgc_aligned_alloc`,
`zgc_request_and_wait`) sit directly on this heap and collector.

**Heap.** Reserved VA in fixed chunks, one size and alignment class per chunk, out-of-band
mark bits and metadata. Aux arrays and boxes are ordinary blocks found by chunk. Large objects
use their own runs and fresh zero pages. Objects sit above any `upper - size` wrap.

**Failures.** Safety failures report directly and abort. A Rust `panic!` would run a hook that
can exit into a collection point. `filc_stack_overflow_failure` stays an assembly trampoline
onto a failure stack large enough for Rust formatting.

## Milestones

Each ends with passing tests and a commit on `library-runtime` (one-line message, no trailers,
per the commit rule). No pushing without being asked.

**M0. Spikes** (`spikes/rust-rt/`): done, go.
- `filcc -nodefaultlibs` works. `spikes/rust-rt/bare.c` gets `NEEDED libpizlo.so, libyoloc.so`
  and no libc, so it passes `check_library`. `-nostdlib` drops `libpizlo.so` too, so it doesn't.
  That object imports 20 symbols, 16 of them runtime helpers.
- `filc_ptr` is flattened by LLVM into two integer arguments (`ptr`, then `lower`), confirmed
  in `filc_store_ptr_atomic_outline`. **Rust helpers take two word parameters**, never the
  struct by value, which matches compiled callers on both architectures, including the case
  where a pointer straddles the last argument register. Returns are a two-word `repr(C)` pair.
- Thread offsets confirmed in x86-64 binaries: CC buffers 0x80 and 0x180, unwind registers
  0x20 and 0x30, cookie 0x40, state 8, allocator slots at 3072 + 208n. aarch64 is unverified,
  because its distribution isn't in the store; check it at M6.
- Exit-allowed `filc_allocate` is a real safepoint: libpizlo already exits there when zeroing
  large allocations, with no distinction by size in the pass. Rust natives must use a
  non-collecting path, as must the inline allocator.
- The trapping stub moved to M1, since it is the same artifact as the real stub.

**M1. Object model and bare calls** (`crates/fil-rust-rt`, stub generator): done on musl x86-64.
- `#[repr(C)]` thread, object, frame, origin with compile-time asserts and a C `offsetof`
  cross-check against the pinned headers.
- Failure paths, access checks, `filc_memmove`/`memset` family, ptr atomics and boxes, aux
  ensure, promote/demote, va_arg, globals, constant relocations, deferred ctors,
  thread-locals, native-thunk macro, support library.
- Allocator with `Collection::Never`.
- Bare fixtures built with `-nodefaultlibs`, plus existing fixtures with no libc imports.
  Safety failures must match: OOB, UAF, double free, wild pointer, wrong signature.

Results: compiled libraries run with no libpizlo, libpas or libc loaded, and the support
library's `zasprintf` runs on Rust natives. `tests/layout.rs` has a C compiler assert every
layout against the pinned headers. The aarch64 and glibc builds of this code are not run yet;
the 16-byte atomics for aarch64 are written but untested. Helpers for unwinding and `setjmp`
still report their name, and most natives are still missing, as planned for M5. Existing
fixtures with no libc imports need the `fil-rust` API, so they run from M2.

**M2. Backend seam**: done on musl x86-64.
- `fr_*` in Rust, `Config::runtime`, `Libc`, name-only stand-ins, bare threads, callbacks with
  CC tracking, `zclosure_new`, handles. A set `has_exception` is fatal until M5.
- `tests/rt_bare.rs`, including `run` from several Rust threads.

`Backend::Rust` runs `tests/rt_bare.rs` and `tests/layout.rs` with no thread started and no file
of the distribution mapped; `rt_bare.rs` also passes on libpizlo, apart from what's specific to
bare threads. A library that imports from libc fails to load, naming the symbol, until M4 adds the
import scan. Callback entry tracks every aux word of the argument buffer, since it can't tell which
are pointers. That's exact: compiled callers zero their aux words for the whole call.

**M3. Tracing GC, multi-mutator**: done on musl x86-64, except as noted below.
- Stop-the-world across bare threads.
- All roots from `filc_mark_global_roots` and the thread root walk: frames with stack auxes
  and setjmp slots, native frames, globals, ifunc `root_ptrs`, deferred ctors, errno and
  dlerror handlers, user argv/environ/auxv, signal table, `thread_locals`, thread fields.
- Aux and box tracing, freed-object singleton redirect, sweep, and FUGC's census, revive and
  destruct phases, so thread, mmap and table destructors have a home.
- `Collection`, `HeapStats`, heap verifier, stress mode (`FIL_RUST_GC_STRESS`: collect at every
  safepoint, scribble swept memory).
- Alloca tests, one fixture per row of the tracking table that exists at this milestone:
  a local whose only reference is a frame slot or stack aux survives a collection triggered
  from a callback and a pollcheck, and its contents stay intact. After return, a Rust-driven
  collection reclaims it. A loop of escaping allocas, varargs and byval calls with periodic
  `Runtime::collect` stays flat in `HeapStats`, and grows under `Never`.

`crates/fil-rust-rt/src/gc.rs` stops the world, marks precisely, and sweeps. `tests/rt_gc.rs` has a
case per row of the tracking table that exists so far. Each root and edge kind the marker follows
was checked by disabling it and watching a test fail, except deferred constructors, which are
compiled global function objects: marking them can't change what a test sees. Some findings:

- Compiled callers keep every pointer's lower in a frame slot, even ones just loaded, so tracking
  callback arguments at entry only matters for callers in Rust. `tests/collector.rs` covers that.
- A `volatile` pointer field is atomic, so it has a box. A plain field has a lower in its aux entry.
- A local array zeroed in a loop got a stack aux; the same array with `= { NULL }` didn't.
- `free` doesn't drop what a tombstone's aux refers to until the object is marked, and a `Ptr` in
  Rust's hands keeps a tombstone in place. Heap references are redirected.

Not done here: census, revive and destruct. Threads turned out not to need them, since a thread
record is an ordinary block with no destructor (see M4), so they came with their first users in M5:
mappings, and the tables and weak references in `tables.rs`.

**M4. Libc mode on musl x86-64, with threads**: done.
- ELF import scan, `Auto` upgrade, ctor gate, boot, errno and dlerror handlers.
- `zthread_*`, futex and multi-threaded libc, which boot itself needs (`spawn_workers`).
- `dlopen`/`dlsym`, syscall groups A and B from tables, `writev` and iovec natives.
- Port suites: `smoke`, `calls`, `memory`, `layout`, `threads`, `lifecycle`, `stress`,
  `startup`. Mark what stays out until M5, such as the Fil-C `fork` case in `edges.rs`.

`crates/fil-rust-rt/src/start.rs` boots the libc, `zthread.rs` has threads and `sys.rs` the system
calls. `tests/rt_libc.rs` covers each way of getting the libc, and refusing it. `tests/sys.rs` runs
a C program through the system calls on either backend and has to get the same results. Beyond the
planned suites, `callbacks`, `embedded`, `process`, `properties`, `misuse`, `bindgen` and the `ui`
tests pass too. Under `FIL_RUST_GC_STRESS`, the libc suites stay green. Some findings:

- A thread record is a heap block, allocated while holding the thread list so a collection can't
  sweep it before it's listed. `scan` keeps the cookie and result flights of one that finished, for
  whoever joins it.
- A new thread must not register a TLS destructor before it's running: glibc takes the loader lock
  to do that, and a constructor can be holding it while waiting for that thread.
- `zthread_exit` can't use the host's `pthread_exit`, whose forced unwinding would walk compiled
  frames. It escapes to the thread's entry through `fr_call_escapable`. That assembly exists for
  aarch64 too, but hasn't run there.
- Bound as `RTLD_GLOBAL`, the real `libc.so` coexists with the name-only stand-in: libraries that
  need it find its symbols in the global scope. `Library::getter` falls back to the handle kept by
  the runtime, since `dlsym` on a handle only looks at that library and what it depends on.

`tests/boot.rs` checks each backend for what it should map. `abi_audit` passes on both: its
`cmsghdr` padding case is why sends copy control data and clean it the way musl's `sendmsg` does. Sockets, `epoll`, the
`mmap` family, `fork`, `execve`, `wait*`, timers, SysV IPC, `mount` and `prctl` landed in M5. A few
calls fail by name on both backends, since upstream's musl flavor panics on them too: `fanotify_*`,
`getcpu`, `pkey_*`, `process_madvise`, `process_mrelease`, `epoll_pwait2` and `dlvsym`. They come
with M6.

A signal that lands on a thread that isn't Fil-C's is written down there, and the libc main thread
is woken with signal 34 to run the handler at its next safepoint. Sending the signal itself on, as
before, left it waiting whenever the main thread had it blocked for a moment (in a handler for it,
or during a collection), and a program that reset it to `SIG_DFL` in that time was killed by it.
Under `FIL_RUST_GC_STRESS=8` that killed the `signals` suite in 5 runs of 20; now it's 0 of 20.
Signal 34 is musl's `SIGSYNCCALL`, which Fil-C programs already can't touch, but it's the host
glibc's `SIGRTMIN`: Rust code in a process using the Rust backend mustn't use it. A signal the
program blocks on the main thread is still sent on as it is, for `sigwait`. Not covered by tests,
since jobs don't run on the main thread: that path, delivery when the program unblocks, and
`sigsuspend` and `pause` going back to waiting when woken for a signal they block.

The roots for the errno and dlerror handlers and for argv, environ and auxv are what upstream has,
but disabling them changes nothing a test can see: the handlers are global function objects, and
the vectors stay in `__libc_start_main`'s frame for as long as the process runs. Tracking the
function and argument in `zthread_create` is redundant the same way, with the compiled caller's frame.

**M5. The rest of musl**
- `setjmp`/`zlongjmp`, `_Unwind_RaiseException`, landing pads, deferred signals with an
  async-signal-safe thread lookup, `mmap` family, `fork`, `execve`, sockets, epoll ptrtable,
  `zweak_*`, `zweak_map_*`, `zgc_finq_*`, `zptrtable_*`, `zfiber_*`, `zmath_*`, `zcpuid*`.
- Whole musl workspace suite green on the Rust backend, plus a `gc-stress` equivalent.
  Backend-specific tests (`boot.rs`, `abi_audit`) are marked, not silently skipped.
- Assess the two suspected upstream bugs, `filc_runtime.c:1811` (thread dereferenced before
  its null check) and `:8866` (compares `act_ptr`, not the handler). Don't copy them, and
  record real ones in `docs/upstream-filc-issues.md`.

Done so far: `jump.rs` has `setjmp`, `eh.rs` exceptions and `signal.rs` signals, and `tests/jumps.rs`,
`tests/cxx.rs` and `tests/signals.rs` pass on both backends. Notes from those:

- Every lock an exited thread can hold has signals blocked around it, as a handler could want it
  too. That includes a whole collection, which runs exited: a handler landing on one would wait
  for the collection under it to end. Joining a thread uses a futex, not a lock, so a signal can
  run a handler during a join.
- Restoring a frame's lowers in `zlongjmp` is ported but untested: disabling it changes nothing a
  test can see, as no fixture gets a compiler to change a frame slot between `setjmp` and `longjmp`.
  The same goes for rooting the exception object while unwinding, which compiled frames also hold,
  and a handler's function, which is global.
- `:8866` and a third bug, a signal queued across `sigaction`, are real, and both are in
  `docs/upstream-filc-issues.md`. `:1811` is only reachable from a thread that isn't Fil-C's, which
  a pure Fil-C process doesn't have. Here it's why such signals are forwarded, so it isn't reported.
- `tables.rs` has pointer tables, exact pointer tables, weak references, weak maps and finalizer
  queues, tested by `tests/tables.rs`. Weak maps are ephemerons: marking repeats until no live key
  adds a value. After the census, finalizable objects nothing reached are marked and queued, and
  what that marks gets a second census. FUGC skips that, so a weak map or weak reference only a
  finalizable object reaches hands back what was collected: issue 6 in
  `docs/upstream-filc-issues.md`, and the one table test skipped on libpizlo.
- `epoll` needs only the four `_impl` natives. The support library's Fil-C wrappers keep a weak
  exact pointer table per epoll descriptor and pass pointers in event data through it, so the
  natives move integers between Fil-C's 16-byte event and the kernel's packed 12-byte one.
- `queries.rs` has what `stdfil.h` asks of the runtime about itself, tested by `tests/stdfil.rs`.
  There's no scavenger and no runtime threads, so their natives do nothing; collections stop the
  world, so `zgc_is_stw` is always true. `zgc_try_request` arms the caller's Check bit, so the
  collection happens at its next safepoint and the call doesn't wait. `zsetproctitle` only checks
  its string, since the process title is the host's. `zdump_heap` and `zdump_pas_status` print
  `HeapStats`, not every object; `zdump_stacks` stops the world to read every thread's frames.
  Pointer descriptions (`zptr_to_new_string`) match libpizlo's byte for byte.

**M6. glibc flavor and aarch64**
- The larger `zsys_*` set, per-flavor struct layouts for Rust natives (musl layouts on a glibc
  host), support library compiled from `filc/src` with the glibc compiler (widen
  `nix/filc-runtime-src.nix`).
- `tests.glibc` and the aarch64 cross suite.

**M7. Remove libpizlo**
- Flip the default; delete `csrc/shim.c` and the oracle backend; replace `abi_audit` with an
  audit of the stub and natives; rewrite README and DESIGN; drop handshake-era rules.
- The pinned Fil-C source stays, for `filc/src` and layout cross-checks.

Not in this plan: true no-GC mode, a Rust mini libc to widen bare mode, adopting Rust threads
in libc mode, concurrent or parallel marking, the LLVM JIT, the sampling profiler.

## Critical files

- New: `crates/fil-rust-rt/`, `crates/fil-rust/tests/rt_*.rs`, `spikes/rust-rt/`,
  `docs/RUNTIME.md`
- Change: `crates/fil-rust-sys/build.rs` (stub, stand-ins, support library),
  `crates/fil-rust-sys/src/lib.rs` (`Shim`, `Flavor`), `crates/fil-rust/src/runtime.rs` (boot
  split, modes, `Collection`), `crates/fil-rust/src/library.rs` (import scan, preload),
  `crates/fil-rust/src/callback.rs` (CC tracking), `nix/tests.nix`, `nix/filc-runtime-src.nix`
- Reuse: `memfd_path`, `dlopen_bytes`, `loaded_objects`, `compat_musl.c`, `compat_glibc_*`,
  `compat_glibc_pizlo.c` as the stand-in pattern, test fixtures and `tests/common`
- Port from, with attribution (libpas license): `filc_runtime.c`, `filc_runtime_inlines.h`,
  `filc_native.h`, `fugc.c`, `filc_stack_overflow_failure.S`

## Verification

- **Differential:** every backend-neutral test runs on both backends with the same results and
  panics. Backend-specific tests are listed.
- **Bare proof:** `dl_iterate_phdr` shows no libpizlo, no libpas and no real libc, allowing the
  name-only stand-ins and `libyoloc` stand-in. Thread count stays at 1 under `Manual`.
- **Collector:** stress mode, heap verifier, `tests/properties.rs` with `PROPTEST_CASES=1000`,
  and the alloca tests from M3, kept running through later milestones (jmp_buf and thread-local
  rows join at M5).
- **Safety:** ported panic tests fail exactly where libpizlo fails. An audit test asserts the
  stub lists every helper the pass can emit and every native in that flavor's libpizlo.
- **Nix:** new `passthru.tests` suites (`rust-rt`, `rust-rt-gc-stress`, later `-glibc`),
  `--cores 4`. `tests.clippy` stays clean. Existing suites stay green until M7.

## Risks

1. **Safepoint discipline.** No conservative fallback; a stray GC is silent use-after-free.
   One collection entry point, debug assertions, stress mode, and the CC-tracking rule.
2. **Size.** Estimated 6K lines through M3, 13K through M4, 30K at parity. M1-M3 are useful
   alone, but only for libraries with no libc imports.
3. **Unsafe Rust is now the safety boundary.** A missed check is a hole for all user code.
   Differential panic tests are the main defense.
4. **ABI drift before the freeze is real.** Pinned to 0.687; the audit fails on drift.
5. **Pause times.** Stop-the-world waits for pollchecks, and recursion without back edges never
   polls. Acceptable for an explicit, Rust-driven collector, and documented.
