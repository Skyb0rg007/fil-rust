# A Rust runtime for Fil-C libraries

Status: in progress on `library-runtime`. This is the plan and its running findings. Since
2026-11-08 the plan is the switch to a Rust libc (below), which replaces libc mode and M6-M8.

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

M5 is done, but for `stress` under the stress modes (below). `jump.rs` has `setjmp`, `eh.rs` exceptions and `signal.rs` signals, and `tests/jumps.rs`,
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
- `zstack_scan`, in `eh.rs`, starts at the caller's frame, as natives push none, and describes an
  inlined call as a frame of its own. Tested from C in `tests/stdfil.rs` and, for a frame that
  catches, from C++ in `tests/cxx.rs`.
- `cpu.rs` has `cpuid`, `xgetbv`, `__cpu_model` and `__cpu_features2`. The last two come from
  compiler-rt's own detection at `llvmorg-20.1.8` (Fil-C's clang), vendored in
  `fil-rust-sys/csrc/compiler-rt` and built into a static library under renamed symbols, as it's
  the code libpizlo links and the tables are too easy to get wrong by hand. They're filled in on
  first use, so code needn't call `__builtin_cpu_init`, and `__cpu_features2` has all three
  words. libpizlo gets both wrong: issue 7, and why two `tests/cpu.rs` tests skip it.

That leaves no musl native missing but those that fail by name on purpose (above), fibers and
`zmath_*`. Fibers exist only where upstream builds them, the glibc flavor. `zmath_*` are there for
glibc's libm, which can't do its x87 work in Fil-C code; the musl libc doesn't import them, and they
take `long double`, which needs its own handling here. Both come with M6.

The gc-stress equivalent is `rust-rt-gc-stress`. Besides collecting at every nth safepoint
(`FIL_RUST_GC_STRESS=n`), it collects at the first safepoint after any allocation
(`FIL_RUST_GC_STRESS_ALLOCATIONS=1`), as libpizlo's `FUGC_MIN_THRESHOLD=0` does, for the suites
gc-stress runs: loops through big buffers make the first mode far too slow for `cxx` and `edges`.
`stress` stays out of both. Its threads all allocate, and each allocation stops every one of them,
where FUGC's mutators never wait: one of its tests takes about seven minutes alone. The suite
also hadn't been setting `FIL_RUST_RUNTIME=rust`, so its backend-neutral tests ran on libpizlo. The
runtime crate builds with `opt-level = 2` in the dev profile, which halves the stress runs.

## The switch to a Rust libc (2026-11-08)

M6-M8 as first planned (glibc flavor and aarch64 on Fil-C's libc, then removing libpizlo, then a
native libc beside Fil-C's) are replaced by one switch. **Trust the host libc, do all checks in Rust,
avoid libpizlo**:

- libc imports are Rust natives over the host libc (the `libc` crate) and Rust std, so Rust and
  Fil-C share one environment, one set of standard streams, one time zone. Every library runs on
  the calling thread.
- Libc mode is deleted: no Fil-C libc, boot thread, executors, job queue, constructor deferral, or
  errno and dlerror handlers registered by a libc.
- The libpizlo backend is deleted now, without waiting for parity.
- `libfilrust_support.so` stays. It's Fil-C code with no libc or libpizlo dependency.
- Not every libc function has to exist. A missing function fails by name when it's called, not when
  its library loads.
- The first cut is the glibc flavor (optfil, x86-64), C and C++. The musl flavor keeps working for
  libraries that import nothing from libc; musl layouts for the natives may come later. The aarch64
  suite shrinks to those libraries.
- No Fil-C sources are pinned through Nix: the store is often strained. The small parts needed are
  vendored.

### Design

**Delivery.** `abi/update-symbols.sh` records every `pizlonated_*` export of the glibc
distribution's `libc.so.6666`, `libm.so.6666`, `libpthread.so.6660` and `libdl.so.6662`, with
versions and kinds. `build.rs` makes a stand-in per soname in the libpizlo stub's pattern: every
export a `jmp *table[i]`, `.symver` aliases for each version, and a version script with every node.
`fill_stub` binds them all by name. A missing function's getter returns a function object that
fails by name when called; a missing datum fails when its getter runs. The `compat_glibc_*`
stand-ins keep their sonames and version nodes and lose what only libpizlo used.

**Per-thread libc state** (errno, dlerror, the `strtok` save pointer, the locale, pthread key
values) lives in `ThreadExtra`, allocated at attach and marked with the thread. Never in a
`thread_local!` with a destructor: registering one takes the loader lock, which a constructor may
hold while it waits for a collection.

**Race-tolerant access.** Rust reads Fil-C memory through a view whose scalar reads are single
volatile loads, with bulk methods bounded by their arguments (the rule in `spikes/rseq-pin`). Host
functions get Fil-C pointers only when their accesses are bounded by explicit lengths. Short inputs
scanned for a terminator (paths, modes, names, formats) are copied, bounded by the object's end.
Long scans (`strlen`, `strchr`, `strto*`) are Rust over the view. Pointers the host returns into its
own memory (`getenv`, `strerror`) are copied into GC objects or interned.

**State owners.**

| State | Owner |
|---|---|
| Environment | `std::env`. `getenv` returns an interned GC copy; the `environ` getter rebuilds its snapshot when the host's array changes |
| stdin, stdout, stderr | Rust `std::io`, so Fil-C output stays in order with `println!`. Rust's stdout is line-buffered, so `setvbuf` can only add flushes |
| Other streams | host `FILE *` |
| Time zone | the host's `localtime_r` |
| Locale | C only |
| Kernel state | already shared, since system calls are native |

**FILE.** glibc's headers inline `getc_unlocked`, `putc_unlocked`, `feof_unlocked` and
`ferror_unlocked` into reads of `FILE`'s fields, so a `FILE *` is a real 216-byte GC object with
glibc's layout. Its buffer pointers stay NULL, so the inline paths always call `__uflow` or
`__overflow`, and the Rust side writes the EOF and error bits into `_flags`. A stream is found by
the object's identity in a locked weak table, never by its fields. `fclose` frees the object, so a
later use traps as a use-after-free. An unreachable stream is dropped without flushing: its
destructor runs with the world stopped and does no I/O. Natives format while entered, then exit,
then take the stream's lock and write; taking Rust's stdout lock while entered could deadlock a
collection. `gc::fork` holds every stream lock.

**printf.** Rust parses the format. Arguments come from the call buffer or a `va_list`. Host
`snprintf` formats one conversion at a time. `%s` needs a NUL within the object, and its precision
is capped at what's left of the object. `%n` writes through a checked pointer.

**Threads and callbacks.** `pthread_create`, `join`, `detach`, `self` and `exit` go over
`zthread.rs`. Mutexes, condition variables, `once` and rwlocks are Rust futex code on glibc's
layouts. Key destructors run on threads Fil-C created; on attached Rust threads they leak.
`pthread_exit` on an attached thread fails by name. Natives that call Fil-C (`qsort`, `bsearch`,
`pthread_once`, exit handlers) pass exceptions on and survive `longjmp`.

**Constructors and exit.** ELF constructors get argv from `Config::args` (the host's by default) and
a fresh environ, rooted. `__cxa_atexit` (which `atexit` and C++ static destructors use) records
handlers; Fil-C `exit()` runs them, flushes Fil-C's streams and Rust's stdout, and calls the host's
`exit`. A host `atexit` hook, registered with the first handler, runs whatever is left.

**Signals.** One that lands on a thread that isn't Fil-C's goes to the thread that installed the
handler, if it's alive and doesn't block it; otherwise to any attached thread; otherwise it waits
for the next entry. `__libc_current_sigrtmin` returns 35, keeping Fil-C code off signal 34.

**dlopen from Fil-C** runs `check_library` on the target; `dlsym` calls the `pizlonated_` getter;
`dlclose` never unmaps, since collected objects may still point into the mapping.

**API.** `Backend`, `Config::runtime`, `Libc`, `Config::libc`, `workers`, `max_workers`,
`FIL_RUST_RUNTIME` and `FIL_RUST_WORKERS` go. `Runtime::libc` returns the libc stand-in. `flush`
flushes Fil-C's streams. `spawn` is `std::thread` plus `run`. `Config::args` stays. The `Shim` table
goes, and `fil-rust` calls `fil-rust-rt` directly.

### L0. Spikes: done, go

- **Versioned stand-ins work.** A generated `libc.so.6666` exports all 3,024 `pizlonated_*` entries
  of the distribution's (2,788 names, 93 of them data), and `libm.so.6666` all 1,183. glibc fixtures
  and the distribution's `libc++.so.1` and `libc++abi.so.1` bind against them with `RTLD_NOW`. Each
  export needs a global alias per version, `.symver alias, name@@V` or `name@V`, with default names
  also listed under their node in the version script. A plain `local: *` hides the non-default
  versions, so the script makes only the aliases local.
- **Kinds.** The distribution's getters tell functions (11 bytes, or a 5-byte jump to another getter)
  from data (53 or 57 bytes) and thread-local data (34) by size.
- **The support library builds for glibc** from `filc/src` at v0.687 with the glibc `filcc -O3`,
  against the headers optfil ships. Its imports are helpers, `z*` natives and `memset`, as on musl.
  `filc/src` is unchanged from v0.687 to the current upstream.
- **Call buffer layout.** Arguments, varargs included, sit at their natural alignment: a `double`
  takes 8 bytes, a `long double` 16 bytes at a 16-byte boundary (the gap is zeroed), and each
  pointer's lower sits in the aux word at the same offset. A `va_list` is a pointer to a snapshot
  with this layout. The natives' argument cursor must align, not just count words.
- **Static destructors.** `atexit` is compiled into each library (from `libc_nonshared`) and calls
  `pizlonated___cxa_atexit(fn, arg, __dso_handle)`, as C++ static destructors do. The host
  `__cxa_finalize` that crt's `.fini_array` calls never sees them, so the runtime runs them.
- **No goldens needed.** The suites that compared backends (`sys.rs` and others) assert in C and
  report the failing line, so they don't need libpizlo's outputs.

### Milestones

Each ends green in Nix and is committed.

- **L1. One backend.** Delete `csrc/shim.c`, `compat_signals.inc`, `redirect_imports`, libpizlo's
  boot, `Backend` and the `Shim` table; `fil-rust` calls `fil-rust-rt` directly. Delete
  `nix/filc-runtime-src.nix`: the layout cross-check against `filc_runtime.h` moves into
  `abi/update-symbols.sh`, run by hand against a local checkout when the pinned version changes.
  musl libc mode stays until L6, as the only coverage for the system-call suites. Nix: `musl` takes
  over `rust-rt` and `rust-rt-gc-stress`; `glibc` is build and clippy only; aarch64 runs the
  libc-free musl tests.
- **L2. glibc on the Rust runtime, bare: done.** Generated stand-ins, missing natives failing when
  called, runtime-owned errno and dlerror (the registered handlers win until L6), the glibc support
  library from vendored `filc/src`, flavor-aware loading. Tests on glibc: `rt_bare`, `rt_gc`,
  `layout`, `cpu`, `libc_stand_ins` (errno per thread and set by the runtime; missing functions and
  data failing by name); `rt_bare` proves nothing of the distribution is mapped. `jumps`, `tables`
  and `stdfil` need string functions, `sigemptyset` and `longjmp`, so they move to L3, and
  `stdfil`'s `snprintf` to L4.
- **L3. libc core.** Done so far: the race-tolerant view (`view.rs`: one bounds check, relaxed byte
  loads, scans that stop at the object's end), `double` arguments, strings, memory, ctype tables,
  `strto*` and `__isoc23_*` over the host's parsers, allocation, `strerror` and its kind (interned,
  read-only), the environment over `std::env`, time over the host's (a `struct tm` crosses as a
  copy, its zone name interned), and the system calls: the `zsys_*` natives under glibc's names
  where they match, and glibc's own wrappers (`stat`, `getcwd(NULL, 0)`, `sleep`, `send`, the `*64`
  twins). Tests: `tables` on glibc, and `libc_core`, whose fixture must write the same transcript
  built for Fil-C and for the host, with misuse failing, other threads moving terminators, and the
  environment seen both ways. A parse that runs into the end of an unterminated buffer fails; one
  that stops before it doesn't. Compiled code asks for `environ` once and keeps its storage, so its
  snapshot is made again when Fil-C code changes the environment; Rust's changes reach it then, and
  `getenv` at once. To do: `signal`, `sigaction` and routing, the `sig*set` functions; `setjmp` and
  `longjmp`; timers, whose `timer_t` is a pointer here; exit handlers; `qsort`, `bsearch`; `dl*`;
  constructor arguments. Tests still to come: `jumps` on glibc; signals with no libc main thread.
- **L4. stdio.** The `va_list` decoder, FILE objects, `__uflow` and `__overflow`, the `fopen`
  family, printf, `getline`, `puts`, `DIR *`. Tests: Fil-C `printf` and Rust `println!` in order;
  inline `getc_unlocked` and `feof_unlocked`; corrupted FILE fields stay safe; use after `fclose`
  traps; capped `%s`; `%n`; fork with buffered output; `smoke`, `calls`, `misuse`, `bindgen`,
  `stdfil` on glibc.
- **L5. pthreads and C++.** pthreads; the C locale's `newlocale`, `uselocale` and `*_l`; wide
  characters; `__isoc23_sscanf`; what else libc++ calls on tested paths. Tests on glibc: `threads`,
  `callbacks`, `stress`, `edges`, `lifecycle`, `cxx`, `std::thread`, key destructors.
- **L6. Switch over.** Delete libc mode and the `zregister_*` natives; musl becomes libc-free only;
  port `maps`, `signals`, `sockets`, `system`, `sys`, `process`, `startup`, `ui` and the examples to
  glibc; delete `rt_libc.rs`; `boot.rs` proves nothing but the stand-ins is mapped; `abi_audit`
  checks the stand-ins against the distribution and the natives' layouts against the headers. Nix:
  `glibc`, `glibc-gc-stress`, `musl`, `examples`, `clippy`. Rewrite README and DESIGN.

Not in this plan: true no-GC mode, concurrent or parallel marking, the LLVM JIT, the sampling
profiler.

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
