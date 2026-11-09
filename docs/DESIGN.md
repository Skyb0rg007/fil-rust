# fil-rust design

Status: implemented (see [Status](#status)). Written against Fil-C 0.687: the
optfil (glibc) distribution on x86-64, and the pizfix (musl) distribution on
x86-64 and aarch64. How the runtime was built, milestone by milestone, is in
[RUNTIME.md](RUNTIME.md).

## Goal

A Rust program must be able to call any function in a Fil-C shared library
**safely**, meaning:

1. Safe Rust code using this crate cannot cause undefined behavior on the Rust
   side, whatever the Fil-C code does.
2. Safe Rust code cannot break any Fil-C invariant. Rust can never give Fil-C
   code a capability it couldn't have made itself, and Rust never writes past
   the bounds of a Fil-C object. To Fil-C, Rust looks like just another
   (memory-safe) Fil-C caller.
3. If the Fil-C side breaks a rule, the result is a Fil-C safety panic, never
   memory corruption. A Fil-C panic aborts the whole process. That is memory
   safe, but it can't be recovered from in-process (see
   [Open questions and risks](#open-questions-and-risks)).

`unsafe` is allowed inside the crate, and in opt-in escape hatches for callers
who vouch for a particular library's behavior.

## How Fil-C works at the boundary

These facts come from the 0.687 runtime source (`libpas/src/libpas/filc_runtime.{h,c}`,
`fugc.c`, `filc_start_program.c`) and from inspecting binaries built with the
pinned compilers.

### What fil-rust replaces

A stock Fil-C process has three layers:

| layer | musl ("pizfix") | glibc ("optfil") | compiled with |
|---|---|---|---|
| user libc | `libc.so` | `libc.so.6666`, `libm.so.6666`, `libpthread.so.6660`, `libdl.so.6662` | Fil-C |
| runtime | `libpizlo.so` | `libpizlo.so` | ordinary C ("yolo") |
| yolo libc | `libyoloc.so` | `libyolocimpl.so`, `libyolomimpl.so`, `ld-fil1-x86_64.so` | ordinary C |

A Fil-C executable is a native program whose `main` boots libpizlo, which
starts FUGC (a concurrent collector with its own threads, built on libpas)
and calls the user libc's `__libc_start_main`. Fil-C shared objects are
ordinary ELF files with ordinary relocations; the load-time work that *is*
Fil-C-specific (constant relocations, global initialization, constructors)
is done by the runtime, called from the libraries' own code.

That runtime assumes it owns the process, and upstream is taking it further
that way: a required concurrent collector, no escape. fil-rust keeps none of
the three layers. The runtime is a Rust library, `fil-rust-rt`, a port of
libpizlo's object model, helpers and natives with its own collector; the
glibc flavor's user libc is Rust too, over the host's glibc. The loader is the
host's: glibc's `ld.so` loads Fil-C objects once stand-ins with the right
sonames and symbol versions are in the global scope.

### Symbols are getters, not functions

For each exported C symbol `foo`, a Fil-C `.so` exports:

- `pizlonated_foo`: a *getter* with the signature
  `filc_ptr (*)(filc_thread*, const filc_origin*)`. It returns a flight pointer
  (`{ptr, lower}`, returned in rax:rdx) to the function object or global
  variable. Globals may be lazily initialized by the getter.
- `pizlonatedFI<sig>_foo` / `pizlonatedFIP<sig>_foo`: typed fast entrypoints
  that the compiler uses for direct calls. The signature encoding is compiler
  internal, so we ignore these.

### Calling convention (generic entrypoint)

A function pointer points at a `filc_function`:

```c
struct filc_function {
    void* fast_entrypoint;
    pizlonated_function generic_entrypoint;
    filc_signature signature;
};
typedef pizlonated_return_value (*pizlonated_function)(
    filc_thread* my_thread, void* callee_lower, size_t argument_size);
struct pizlonated_return_value { bool has_exception; size_t return_size; };
```

The arguments are laid out like the fields of a struct (each slot at least 8
bytes and naturally aligned). They go in the calling thread's
`cc_inline_buffer` (256 bytes, spilling into `cc_outline_buffer`). Each pointer
argument's `lower` (its capability) goes in the parallel *aux* buffer. Return
values come back in the same buffers, with `return_size` bytes in use.
`filc_call_user_*` in libpizlo is the reference implementation of a native →
Fil-C call: check the function pointer, size the arguments, zero the buffers,
write the arguments, track pointer arguments, lock the native frame, call
`generic_entrypoint`, then read the returns.

This is what makes the design safe. **The callee checks every argument as it
reads it.** If an argument slot is missing, the callee raises a safety panic
(`cc cursor ... doesn't have room`). An integer passed where a pointer is
expected reads a zeroed aux slot, so it becomes a pointer with a null
capability that can't be dereferenced. So a wrong signature on the Rust side
leads at worst to a Fil-C panic, never to UB. That holds as long as Rust only
ever writes genuine `lower` values into the aux buffer.

Struct-by-value arguments and returns follow clang's LLVM-IR lowering for the
target (x86-64 SysV or AAPCS64: coercion to integer pairs or float arrays,
`sret` pointers). They don't follow the C source signature.


### Threads and the collector

- Compiled code receives its thread, a `filc_thread`, as argument 0 of every
  helper and function; it's never found through TLS. Its layout (stack limit,
  state word, top frame, call buffers, allocator slots) is fixed per compiler
  version and checked at compile time in `fil-rust-rt/src/abi.rs`.
- A thread is either *entered* (it runs compiled code or touches GC objects,
  and promises to reach a `pollcheck` regularly) or *exited* (blocked or in
  native code, and the collector doesn't wait for it).
- Roots are precise, with no conservative fallback: frame slots holding each
  live pointer's capability, native frames, globals, and runtime tables. Call
  buffers and registers aren't visible, so a collection may happen only at a
  safepoint.
- Fil-C `free()` revokes an object's capability immediately; the memory comes
  back once a trace finds the object unreachable. **Rust can never assume a
  Fil-C object stays valid across a pollcheck or a call into Fil-C.**
- A pointer stored in memory keeps its address in the payload and its
  capability in an invisible aux slot. Storing one needs an aux update, which
  plain byte writes can't do.

## Approaches considered

**A. An in-process runtime in Rust (chosen).** The Rust binary is a normal
glibc program. It loads stand-ins for the Fil-C runtime and libc, implemented
in Rust, and calls Fil-C functions through the generic entrypoint as libpizlo
does. Calls cost about as much as a C call plus argument marshalling, and
data can be shared without copying everything. The cost is depending on
runtime internals (the `filc_thread` layout, call buffers, aux encoding,
helper signatures), which are fixed for a compiler version but not across
versions. We pin the version; upstream plans to freeze the ABI (`fil1`).

**A′. Hosting the stock runtime in-process.** fil-rust's first design loaded
the distribution's `libpizlo.so` and Fil-C libc next to the host glibc, with
small stand-ins for the yolo libc, booted the libc on a dedicated thread and
ran Rust's calls on a pool of Fil-C executor threads. It worked, but every
call was a cross-thread hop, FUGC ran its own threads, Fil-C and Rust had
separate environments and stdio buffers, and each upstream step towards a
runtime that owns the process made it harder. It was replaced by A.

**B. Out-of-process host.** A small Fil-C host program loads the library, and
Rust talks to it over IPC or shared memory. Fil-C panics stay isolated and we
don't depend on runtime internals, but every call pays IPC cost and needs full
serialization. This could be added later on top of the same marshalling
types; it doesn't replace A.

**C. Compile Rust itself to Fil-C** (emit LLVM IR from rustc, then pizlonate
it). Safety would come for free, but all Rust code would pay Fil-C's
overhead, and the core and std libraries contain inline asm. A different
project.

**D. A Fil-C loader written in Rust.** Rejected, because the loader isn't
Fil-C-specific: glibc's `ld.so` loads everything once the stand-ins are in
place.

## Architecture

```
crates/fil-rust-rt        the Fil-C runtime and the glibc flavor's libc, in Rust
crates/fil-rust-sys       the stubs and stand-ins (generated), the support library (Fil-C), built by build.rs
  abi/                    the symbol lists they're generated from, and the script that regenerates them
  csrc/filc/              Fil-C's filc/src at v0.687, vendored: the support library's source
crates/fil-rust           safe API: runtime, pointers, memory, calls, libraries, callbacks, handles
  examples/               runnable programs, each with its C/C++ source in examples/c
  tests/                  integration tests over C/C++ fixtures
crates/fil-rust-macros    #[derive(FilType)]
crates/fil-rust-bindgen   generates FilType structs and foreign! interfaces from C headers (library + CLI)
crates/fil-rust-build     compiles C/C++ with Fil-C in build scripts, for include_library!
crates/bindgen-demo       builds and embeds a C library and its bindings from a build script
nix/                      the pinned package set, the Fil-C distributions, the build environment, the test suites
default.nix, shell.nix    the Nix build and dev shell; both take `crossSystem`
```

Nothing from the Fil-C distribution is linked into the Rust binary.
`fil-rust-rt` is ordinary Rust. The native objects Fil-C libraries bind to by
name (the runtime stub, the libc stand-ins, the support library) are built by
`fil-rust-sys/build.rs`, embedded with `include_bytes!`, and loaded at run
time from memfds. The distribution is found at run time from [`Config`], only
for what a library depends on beyond those, such as `libc++.so.1`.

### Flavors

A process holds one runtime, of one flavor, chosen with `Config::glibc` /
`Config::musl` (or `FILC_FLAVOR`), and compiled in with the `glibc` and
`musl` (default) cargo features. Both features can be on at once.

| | glibc ("optfil", `/opt/fil`) | musl ("pizfix") |
|---|---|---|
| architectures | x86-64 | x86-64, aarch64 |
| libc | the runtime's, in Rust | none |
| libc stand-ins | `libc.so.6666`, `libm.so.6666`, `libpthread.so.6660`, `libdl.so.6662`: every export and version of the distribution's, each a slot | `libc.so`, a soname only |
| yolo libc stand-ins | `libyolocimpl.so`, `libyolomimpl.so`, `ld-fil1-x86_64.so`: sonames and version nodes | `libyoloc.so` |
| libraries that import from libc | served | refused, `Error::Unsupported` |

Every Fil-C library is linked against its flavor's libc whether it uses it or
not, so the musl check reads the imports: a library is refused for the first
function it imports that only the libc could give it. Musl layouts for the
libc natives could come later.

The glibc distribution's objects use **symbol versions**, and glibc's `ld.so`
refuses a library whose version nodes are missing. `abi/update-symbols.sh`
records every `pizlonated_*` export of the distribution's libraries, with its
versions and kind (function, data or thread-local data, told apart by the
getter's size), in `abi/glibc_*.txt`. `build.rs` makes a stand-in per soname:
every export a `jmp *table[i]`, a `.symver` alias per version, and a version
script with every node. At startup `fil-rust-rt` fills the slots by name from
its native tables. A function with nothing behind it gets a getter that
returns a function object failing by name when called, so a library that
merely takes its address still loads; a variable with nothing behind it fails
by name when its getter runs.

The nix shell provides both distributions. optfil's compiler expects to live
at `/opt/fil`, so `nix/optfil.nix` wraps `filcc`/`fil++` in `bwrap` with the
distribution bind-mounted there.

### The runtime stub

Compiled libraries link against `libpizlo.so` by name. The stub with that
soname is generated in the same way from `abi/runtime_symbols_<flavor>.txt`:
the 65 helpers the compiler pass can emit (taken from the pass's source, since
one missing symbol fails a whole `dlopen`), `filc_current_marking_state`
(which stays 0, so the store barrier is dead), and the 519 native getters of
that flavor's libpizlo. The two flavors' lists are identical.

The slots are filled from `fil-rust-rt`'s tables. A few natives are Fil-C
code in the stock runtime too (`filc/src`: `zasprintf`, the descriptor
wrappers that keep epoll's pointer tables, `_Unwind_*`): those come from
`libfilrust_support.so`, built from the vendored source for each flavor, and
their slots point into it. An unimplemented helper reports its name and
aborts.

### The runtime: `fil-rust-rt`

A port of libpizlo, so failures, layouts and pointer descriptions match:

- **Object model and checks.** Objects with a header at `lower - 16`, aux
  arrays for stored capabilities, special objects (functions, threads,
  jmp_bufs, closures), and the access checks compiled code relies on.
- **Heap.** Reserved address space in chunks, one size class per chunk, with
  mark bits and metadata out of band.
- **Collector.** Stop-the-world, non-moving, precise. There's no collector
  thread: a collection runs on the thread that asked for it, reachable only
  from a safepoint, an explicit `Runtime::collect`, or an allocation that may
  collect. It stops every entered thread at its next pollcheck, marks from
  every thread's frames and the global roots, follows aux arrays, weak maps
  (as ephemerons) and finalizer queues, sweeps, and runs destructors for
  special objects. `Collection::{Never, Manual, Threshold}` chooses when, and
  `HeapStats` reports what's live, freed and reclaimed. `FIL_RUST_GC_STRESS=n`
  collects at every nth safepoint and scribbles over swept memory.
- **Helpers and natives.** Allocation, `memmove` and friends, atomics,
  varargs, globals and constructors, `setjmp`/`longjmp`, exceptions and
  unwinding, threads (`zthread_*`), signals, system calls, `mmap`, sockets,
  pointer tables, weak references, `stdfil.h` queries, and `cpuid`.

Failures report and abort directly rather than through `panic!`, whose hook
could run into a collection point.

### Startup

`Runtime::start(config)`:

1. Checks that the configured root is a distribution of the flavor.
2. Records the arguments constructors will see (`Config::args`, or the
   host's).
3. Loads, from memfds and `RTLD_GLOBAL`, the yolo libc stand-ins, the libc
   stand-ins and the runtime stub, then the support library, and fills every
   slot.
4. Checks with `dl_iterate_phdr` that nothing it stands in for is mapped (the
   real yolo libc, a real libpizlo, the distribution's libc). The same check
   runs after every `load`.

It starts no threads and runs no Fil-C code.

### Threads

Any Rust thread can run Fil-C code. The first `Runtime::run` on a thread
attaches it: it gets a `filc_thread`, with its libc state (errno, dlerror,
pthread key values, the locale) beside it, and keeps it until the OS thread
exits. Each `run` enters the runtime, pushes a native frame for the pointers
Rust obtains, runs the closure with a `Fil`, and exits again. A `run` inside a
callback or another `run` reuses the thread's entry. `Runtime::spawn` is
`std::thread` plus `run`.

Threads that Fil-C code creates (`pthread_create`, `std::thread`) are
`zthread`s: OS threads running compiled code from the start, which may call
Rust callbacks like any other.

A child made by `fork()` from Rust has only the forking thread, but the
runtime still lists the others, and locks they held stay held, so a
`pthread_atfork` handler marks the child: `run`, `spawn` and `join` panic
there, and values from the runtime leak when dropped instead of taking those
locks. Fil-C code's own `fork` is fine: the runtime stops the world and
holds its locks across it, and both processes carry on. `posix_spawn` and
`exec` are unaffected.

### Never block while entered

A collection waits for every entered thread to reach a pollcheck. A thread
that blocks on a Rust primitive while entered stalls every collection, and
whoever waits on that collection, which may be the thread it's waiting for.
So:

- `Fil::exited(f)` leaves the runtime around blocking Rust code. Its closure
  must be `Send`, so it can't capture a `Fil` or a `FilPtr`.
- Internally, every lock that can be taken while entered is taken by
  `try_lock` first, falling back to exit → lock → enter. `JoinHandle` waits
  leave the runtime when called from a Fil-C thread. Every libc native that
  can block (I/O, mutexes, sleeps) runs exited.
- `Fil::pollcheck()` is for long Rust loops that don't call Fil-C.
- glibc registers a `thread_local!`'s destructor under the loader lock, which
  `dlopen` holds while constructors run, so the runtime keeps per-thread
  state in the thread's own record, never in such a thread-local.

### The libc (glibc flavor)

The libc that glibc-flavor libraries import is Rust natives over the host's
glibc (the `libc` crate) and Rust's std, so Fil-C and Rust share one libc's
state:

| state | owner |
|---|---|
| environment | `std::env`; `getenv` returns an interned GC copy, and the `environ` snapshot is rebuilt when the host's changes |
| stdin, stdout, stderr | Rust's `std::io`, so `printf` and `println!` stay in order |
| other streams | host `FILE *`s |
| time zone | the host's `localtime_r` |
| locale | the C locale only |
| `libm` | the host's, through `dlsym` (`long double` goes through `double`) |
| kernel state | shared, since system calls are native |

**Every argument is checked in Rust before the host sees it.** Rust reads
Fil-C memory through a view whose loads are bounded by the object (and stop
at its end) and tolerate other threads writing concurrently. Host functions
get Fil-C memory only when their accesses are bounded by an explicit length;
short terminator-scanned inputs (paths, modes, formats) are copied; long
scans (`strlen`, `strto*`) are Rust over the view. Memory the host returns
(`getenv`, `strerror`) is copied into GC objects.

- **FILE.** glibc's headers inline `getc_unlocked`, `putc_unlocked`,
  `feof_unlocked` and `ferror_unlocked` into reads of `FILE`'s fields, so a
  `FILE *` is a real 216-byte GC object in glibc's layout. Its buffer pointers
  stay null, so the inline paths always call `__uflow` or `__overflow`, and
  the EOF and error bits are written into `_flags`. A stream is found by the
  object's identity, never by its fields, so a scribbled-over or forged
  `FILE` can't reach another stream. `fclose` frees it, so a later use traps.
- **printf and scanf.** Rust parses the format; arguments come from the call
  buffer or a `va_list`; the host's `snprintf` formats one conversion at a
  time on copied values. `%s` needs a terminator within its object unless a
  precision bounds it, `%n` stores through a checked pointer, and too few
  arguments fail. `sscanf` parses a copy and stores through checked pointers.
- **pthreads.** `pthread_t` is the thread's object, and threads are
  `zthread`s. Mutexes, condition variables, rwlocks, `once`, barriers, spin
  locks and semaphores are futex code over glibc's object sizes, where a
  zeroed object is a ready lock. Key destructors run on threads Fil-C
  created; on attached Rust threads the values are dropped.
- **Errors.** `errno` lives in the thread's libc state, and an `errno` the
  host sets comes back to Fil-C.
- **Constructors and exit.** Constructors get `Config::args` and the
  environment. `atexit` and `__cxa_atexit` (C++ static destructors) record
  handlers; Fil-C `exit()` runs them, flushes Fil-C's streams and Rust's
  stdout, and calls the host's `exit`. A host `atexit` hook runs what's left
  when the process exits from Rust.
- **`dlopen` from Fil-C** runs the same library checks as `Runtime::load`;
  `dlsym` calls the `pizlonated_` getter; `dlclose` never unmaps, since
  collected objects may still point into the mapping.
- **What's missing** fails by name when called. The library still loads.

### Signals

A Fil-C handler never runs in the middle of compiled code: the runtime's
native handler records the signal, and the thread runs the handler at its
next safepoint, or as it comes back from a blocking call. A signal that lands
on a thread that isn't Fil-C's (any Rust thread outside `run`) goes to the
thread that installed the handler, if it's alive and doesn't block it;
otherwise to another attached thread; otherwise it waits for the next to
attach. The runtime wakes threads with signal 34, so `__libc_current_sigrtmin`
returns 35 to keep Fil-C code off it; Rust code in the process shouldn't use
it either. Fil-C never handles synchronous fault signals, so Rust's
stack-overflow detection keeps working; a Fil-C stack overflow is a Fil-C
panic.

### The context token: `Fil<'f>`

`Fil<'f>` proves the current thread is a Fil-C thread inside the runtime. It
is `!Send` and `!Sync`, and it only exists inside a `run`/`spawn` closure or
a callback. `'f` is invariant and higher-ranked (`for<'f> FnOnce(&Fil<'f>)`),
so nothing tied to it can escape the closure.

Every Rust → Fil-C transition pushes a native frame. Pointers that Rust
obtains are tracked in the innermost frame and stay alive until
the frame pops. `fil.scope(|fil| ..)` opens a nested frame to bound memory in
long loops.

### Pointers: `FilPtr<'f, T>`

`FilPtr` is an opaque `{ptr, lower}` plus a C-type marker `T`. Safe code
can't forge one: it comes from the runtime (an allocation, a call's return, a
loaded pointer field, a global) or from `FilPtr::null()` /
`FilPtr::from_addr()`, which carry a null capability. Arithmetic only moves
the address; bounds are checked on access.

All accesses are **copies** checked against the live capability:
bounds, liveness (Fil-C `free` revokes a capability immediately), write
permission, alignment for pointer slots, and "special" objects (functions,
thread objects), which aren't data. Failures are `Error`s. Rust never holds a
reference into memory that Fil-C might also be using, so a racing Fil-C
thread can at worst produce torn values, as in Fil-C itself.

`Ptr<T>` is `FilPtr<'static, T>` for use in signatures and struct field
types; values are always `FilPtr<'f, T>`.

### Rooted handles and owned buffers

- `FilHandle<T>` (`fil.root(ptr)`) keeps an object alive across scopes and
  threads through a root table that the collector scans. It is `Send + Sync`; `get(fil)`
  yields a `FilPtr` in the current scope. Dropping it releases the slot
  (deferred until a Fil-C thread can do it, so `Drop` never blocks while
  entered).
- `FilBuf` is a Fil-C allocation that Rust owns exclusively, so it derefs to
  a plain `[u8]` with no per-access checks. `share(self, fil)` consumes it and
  returns a `FilPtr`, after which Fil-C may retain or use it concurrently.

### Types: `FilType` and `#[derive(FilType)]`

`FilType` describes a C type: its size and alignment, its *leaves*
(eightbytes holding integers, floats or pointers, which drive the calling
convention), and how to read and write its Rust value (`Value<'f>`) at an
offset. It is implemented for the integer and float types, `bool`, `()`,
arrays, `FilPtr`, `Opaque`, and `fn(..) -> R` signatures (whose value is a
typed `FilFn`).

`#[derive(FilType)]` computes a C layout for a struct of `FilType` fields
(`#[fil(size = N, align = M)]` checks it at compile time), and supports one
lifetime parameter for structs holding pointers (`Node<'f>`).

`FilType` can be implemented by hand in safe code, so nothing it touches can
forge a pointer. `read` and `write` get a `Place<'f>`: an opaque location
inside an object that has already been bounds-checked for the whole value,
which can only be offset (`byte_add`) and handed to field types. Values that
travel as one calling-convention word (`to_word`, `from_word`) are `Word<'f>`,
which safe code can build only from integer bits; a word holding a pointer
comes only from a `FilPtr`. Both carry the scope lifetime, so neither can
outlive it. `FilArg::push` (for `DynCall` varargs) appends to an opaque
`DynArgs<'f>` the same way.

### Calls

A `Function<Sig>` is a checked function pointer with signature
`Sig = fn(A, B, ..) -> R`. Calls encode arguments into the generic calling
convention's buffers, following clang's lowering as Fil-C sees it:

- each argument starts on an 8-byte slot; integers narrower than 64 bits are
  extended; pointers write their capability into the aux buffer
- larger returns go through an `sret` pointer passed as a leading argument
- varargs are simply extra slots after the fixed arguments
- on x86-64, structs of up to 16 bytes are flattened per eightbyte; larger
  (or misaligned) structs are copied **inline** into the argument buffer at
  their alignment, one slot per eightbyte
- on aarch64, a homogeneous floating-point aggregate (up to four `float`s or
  `double`s, so up to 32 bytes) is passed inline as its floats; any other
  struct of up to 16 bytes takes exactly two words, even a 4-byte one; larger
  structs are copied to a temporary and passed by pointer

(The aarch64 rules were read off Fil-C's aarch64 compiler with `-mllvm
-print-before-all`. `long double`, a 128-bit float there, is unsupported.)

Return values are read back as words and pointers through checked accessors.
`DynCall` builds a call argument by argument when the signature is only known
at run time. `foreign!` declares a struct with one typed method per C
function and per global variable (`static name: T;` yields a
`FilPtr<'f, T>`), all looked up when the struct is created.

**Exceptions.** A call pushes a Fil-C frame whose origin has a
personality that catches every exception. If a C++ exception escapes the
callee, the unwinder runs the callee's cleanups (destructors) on its way to
that frame; the runtime then disposes of the exception and returns a status that
becomes `Error::Exception`. Rust panics never cross the boundary.

### Libraries

`Runtime::load(path)` first checks the file without running any of it: it
must be an ELF shared library linked against `libpizlo.so` (so a native
library, whose constructors would run unchecked, is refused) and not against
the other flavor's libc. On the musl flavor, which has no libc, a library
that imports anything from libc is refused with `Error::Unsupported`, naming
the first such function. It then preloads the library's `DT_NEEDED`
dependencies from the configured search path and the distribution's lib
directory (a preloaded dependency satisfies the entry through its soname, so
it needs one), then `dlopen`s it with the host loader, `RTLD_LOCAL`. Symbols are found through the
`pizlonated_<name>` getters, which return a properly bounded pointer to the
function object or global (globals are initialized lazily by their getter).
`Library::function` checks that the symbol is a function before returning a
`Function`; `Library::global::<T>` returns a typed pointer.

### Callbacks

`Callback<Sig>::new(fil, closure)` registers a `Send + Sync` closure in a Rust
table and creates a Fil-C closure function object (`fr_closure_new`) carrying
its id. When Fil-C calls it, the runtime's thunk enters Rust, which decodes the
arguments through the same checked accessors (too few arguments is a Fil-C
panic), runs the closure with a fresh `Fil` for the callback's scope, and
encodes the return. A Rust panic in the closure, or a call after the
`Callback` was dropped, becomes a Fil-C panic. Callbacks may be called from
any Fil-C thread, including threads Fil-C code creates itself.

### Bindings from headers: `fil-rust-bindgen`

bindgen parses the header (allowlisted to the header's own declarations and
what they use) and the generator rewrites its output with `syn`:

- a struct becomes `#[derive(FilType)]`, with `#[fil(size, align)]` taken
  from bindgen's layout (so the derive's layout is checked against clang's at
  compile time); it gets a `'f` lifetime if it holds pointers, directly or
  through other structs or typedefs (a fixed point)
- `T*` becomes `Ptr<T>` in signatures and `FilPtr<'f, T>` in fields; `void*`
  becomes untyped `Ptr`; a pointee struct with a lifetime is spelled
  `S<'static>`
- a function-pointer typedef becomes a plain `fn(..) -> R` type, so it works
  with `Callback`; in fields, function pointers are `FilFn<'f, ..>`
- opaque structs become uninhabited marker enums
- functions and `extern` globals go into one `foreign!` struct

What can't be represented is left out with a reason, which propagates:
unions, bitfields and flexible arrays make a struct opaque (usable only
behind a pointer); a typedef or function that uses such a type by value, a
variadic function, or a `long double` is dropped. The output is formatted
with prettyplease. `tests/generated/` holds bindings for two fixture headers;
fil-rust's tests compile and run them, and fil-rust-bindgen's tests check
that they are up to date.

### Embedded libraries: `fil-rust-build`

`fil-rust-build` is a build-script helper in the style of the `cc` crate.
`Build::compile(name)` compiles each source to an object with the Fil-C
compiler (`-fPIC`, Cargo's optimization level and debug info, `-MMD` for
header dependencies, which become `rerun-if-changed`), then links
`lib<name>.so` with soname `lib<name>.so`. A Fil-C library only loads into
a runtime of its flavor, so it builds once per flavor whose compiler it
finds (`FILC_CC`/`FILC_ROOT` for musl, `FILC_GLIBC_CC` or `/opt/fil` for
glibc on x86-64), or the flavors requested with `Build::flavor`, which must
then exist. For each flavor it writes `$OUT_DIR/fil-rust-build/<name>.<flavor>.rs`,
an `Option<&[u8]>` expression (an `include_bytes!` of the library, or
`None`), and `include_library!("name")` builds an `EmbeddedLibrary` from the
two in a const context, so it can be a `static`.

`Fil::load_embedded` writes the runtime flavor's build to a memfd named
after the soname and then takes the ordinary `load` path (`check_library`,
dependency preloading, `dlopen` by `/proc/self/fd` path), so an embedded
library gets exactly the checks a file does. glibc matches a `DT_NEEDED`
name against the sonames of loaded objects, so embedded libraries linked
against each other (`Build::link_with`) resolve once the dependency is
loaded. It doesn't match an opened *path* against a soname, though, so
loading by path never deduplicates: fil-rust remembers each embedded soname
with its bytes and reuses the memfd for the same library, refuses a
different library under a loaded soname, and refuses a soname already
loaded from a file. Either would otherwise load a second copy, with its own
globals.

## Safety argument (summary)

| hazard | prevented by |
|---|---|
| Rust forging a capability | `FilPtr` is opaque; aux values come only from the runtime |
| Rust accessing out of bounds, freed, read-only or special memory | every access is a copy checked against the live capability, entered |
| the collector reclaiming an object Rust holds | pointers are tracked in the current native frame; `FilHandle` roots across frames |
| a `FilPtr` outliving its frame or thread | the invariant, higher-ranked `'f`; `Fil` and `FilPtr` are `!Send` |
| data races being UB in Rust | Rust never holds references into shared Fil-C memory; `FilBuf` is exclusive until shared; the libc reads through a race-tolerant view |
| wrong Rust-side signature | the callee checks argument slots; a mismatched pointer has a null capability |
| Fil-C getting a pointer into Rust memory | impossible: data is copied into GC objects |
| the host libc trusting Fil-C's arguments | every native checks pointers against their capabilities first; scans stop at the object's end; host-returned memory is copied |
| a stream, lock or directory forged from Fil-C memory | the libc finds them by object identity, never by contents |
| unwinding across the boundary | exceptions → `Error::Exception`; Rust panics in callbacks → Fil-C panic |
| the collector stalled by blocked Rust code | `exited`; internal locks and blocking natives never block while entered |
| a collection at a point roots can't see | one collection entry, reachable only from safepoints; stress mode collects at every one |
| runtime ABI drift | pinned distribution versions; stubs generated from checked-in lists; layouts asserted at compile time and cross-checked by `update-symbols.sh` |
| a second runtime or libc entering the process | fil-rust owns all loading; `dl_iterate_phdr` check after start and each load |
| Fil-C's glibc layouts differing from the host's | `abi_audit` compares every struct and constant the natives pass; `epoll_event`, the one difference, is converted |
| signals landing on threads the runtime doesn't know | routed to a Fil-C thread, run at its next safepoint |

## Status

Implemented: everything above. On the glibc flavor (x86-64), the integration
tests (`FILC_FLAVOR=glibc cargo test --features glibc`) cover scalar,
string, struct, sret, varargs and function-pointer calls; checked memory
access and its failure modes; handles and owned buffers under collector
stress; callbacks from many Fil-C threads; C++ containers, threads and
exceptions; process-ending cases (Fil-C safety panics, signature mismatches,
panicking callbacks, `exit`) in child processes; signal handling next to
Rust's; system calls, mappings, sockets and processes; bindings generated
from headers; and libraries built with `fil-rust-build` and loaded from
memory. The libc has its own suites (`libc_core`, `libc_stdio`,
`libc_threads`, `libc_signals`, `libc_stand_ins`): each fixture is built for
Fil-C and for the host and must write the same transcript, and misuse (forged
streams, unterminated strings, too-wide `%n`, freed mutexes, use after
`fclose`) must trap. `boot.rs` proves a library using the libc maps nothing
from the distribution and starts no thread; `abi_audit.rs` checks the
stand-ins export every symbol and version the distribution does, and the
layouts above.

Beyond those: code that must not compile (`tests/ui`: pointers escaping their
scope or thread, forged `Place`s and `Word`s, wrong signatures); misuse that
must fail cleanly (`misuse.rs`); property tests comparing Rust's view of
memory, values and arithmetic with C's (`properties.rs`); struct layouts
checked field by field against C (`layout.rs`); startup in fresh processes
(`startup.rs`); concurrency stress while the collector runs (`stress.rs`);
edge cases (`edges.rs`); and the process exiting, forking or crashing while
the runtime is busy (`lifecycle.rs`). The ELF reader behind the load check is
fuzzed with mutated real files. Nix runs each configuration as a
`passthru.tests` derivation (`nix/tests.nix`): `glibc`, `glibc-gc-stress`,
`musl`, `examples`, `clippy`.

On the musl flavor, the runtime's own tests and the libc-free tests (`rt_bare`,
`rt_gc`, `layout`, `boot`, `cpu`) run on x86-64 and, cross-built and under
qemu-user, on aarch64.

## Open questions and risks

- **Unsafe Rust over the host libc is the safety boundary.** A check missed
  in a native is a hole for all Fil-C code. The race-tolerant view, the
  differential libc tests and the misuse tests are the defense.
- **Runtime internals aren't a stable API.** Mitigated by pinning versions and
  regenerating the symbol lists and layout checks on every bump. Upstream's
  planned `fil1` freeze would make this a one-time cost.
- **Pause times.** A collection stops the world and waits for pollchecks, and
  every allocation-heavy thread waits on every collection. Fine for a
  collector Rust drives; not a concurrent collector.
- **libc breadth.** Unimplemented functions fail when called, so libraries
  load, but a path through libc++ or a C library may reach one. Memory
  streams and stream `scanf` aren't there yet, locales are C only, and
  `long double` math has `double` precision.
- **The musl flavor has no libc.** Its libraries must use none, for now.
- **Panics are process-fatal.** This is inherent to Fil-C. An out-of-process
  host would let Rust survive them, at the cost of copying.
- **aarch64 is tested under emulation only**, and only the musl flavor:
  qemu-user's memory model is stronger than real hardware's, so weak-ordering
  bugs could hide there. Other targets are rejected at build time.
