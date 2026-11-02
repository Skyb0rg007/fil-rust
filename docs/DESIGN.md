# fil-rust design

Status: implemented (see [Status](#status)). Written against Fil-C 0.687: the
pizfix (musl) distribution on x86-64 and aarch64, and the optfil (glibc)
distribution on x86-64.

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
   [Isolation](#optional-out-of-process-isolation)).

`unsafe` is allowed inside the crate, and in opt-in escape hatches for callers
who vouch for a particular library's behavior.

## How Fil-C works at the boundary

These facts come from the 0.687 runtime source (`libpas/src/libpas/filc_runtime.{h,c}`,
`generate_pizlonated_forwarders.rb`, `filc_start_program.c`) and from inspecting
binaries built with the pinned compiler.

### Process structure: the "sandwich"

A Fil-C process has three layers:

| layer | file | compiled with |
|---|---|---|
| user libc (musl, modified) | `pizfix/lib/libc.so` | Fil-C |
| Fil-C runtime | `pizfix/lib/libpizlo.so` | ordinary C ("yolo") |
| yolo libc (plain musl) | `pizfix/lib/libyoloc.so` = `ld-fil1-x86_64.so` | ordinary C |

A Fil-C executable is really a **native yolo-musl program**. `filc_crt.o`
defines the native `main`, which calls
`filc_start_program(argc, argv, pizlonated___libc_start_main, pizlonated_main)`.
That boots the runtime, allocates GC copies of argv/environ, and calls the
user-libc `__libc_start_main`, which in turn calls the program's Fil-C `main`.

The loader itself has nothing Fil-C-specific in it. `ld-fil1-x86_64.so` is
byte-for-byte the same file as `libyoloc.so` (stock musl's combined libc and
`ld.so`). Fil-C shared objects are ordinary ELF files with ordinary
relocations. The load-time work that *is* Fil-C-specific is done by libpizlo,
called from the libraries' own code: constant relocations, global
initialization, and deferred constructors.

### Fil-C on the host glibc (verified)

What ties the runtime to musl is libpizlo's imports, not the loader. libpizlo
imports about 280 symbols from the yolo libc. All but a handful are standard
POSIX/Linux functions that glibc also exports. The exceptions are the yolo-musl
extras (`libc_internal_signals`, `num_libc_internal_signals`, `sigdelsetyolo`,
`yolo_futex_*`) and `getdents`. Each is a thin wrapper around a raw syscall.
The Fil-C user libc exports only `pizlonated_*` symbols (plus `_init` and
`_fini`), so it never clashes with glibc's names.

A ~70-line **compat shim** (`spikes/glibc-host/yoloc_compat.c`) provides those
extras on top of glibc and has the soname `libyoloc.so`. A glibc program loads
the shim with `RTLD_GLOBAL` *before* any Fil-C object, so their `NEEDED
libyoloc.so` binds to the shim by soname. After that, **glibc's own `ld.so`
loads libpizlo, the Fil-C libc and Fil-C libraries.** No custom loader is
needed. `spikes/glibc-host/run.sh` checks this on glibc 2.44:

- the runtime boots, the user libc initializes, and Fil-C `main` runs
- cross-library calls and `printf`/`strdup` work
- 4 threads with heavy allocation under `FUGC_MIN_THRESHOLD=0` (GC churn) work
- futex-based `pthread_cond_timedwait` works
- `stat` and `readdir` return correct struct contents
- signal delivery works
- C++ exceptions work
- `dlopen`/`dlsym` called from Fil-C code (served by glibc) works
- an out-of-bounds write still triggers a Fil-C safety panic

**Consequence:** the Rust program is an ordinary glibc binary. Fil-C code runs
inside it as a set of dlopen'd libraries, and Rust calls them as a peer of
libpizlo. The caveat is that this is not an upstream-supported configuration.
libpizlo was compiled against musl headers, so every struct it passes to libc
must have the same layout in glibc (see [Risks](#open-questions-and-risks)).

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

### GC and threads

- FUGC is a **concurrent, non-moving** GC. A thread is either *entered* (it
  promises to `pollcheck` regularly; its signals are deferred) or *exited* (the
  GC doesn't wait on it). Native code must `filc_exit` before blocking, and
  must be entered whenever it touches GC objects or calls Fil-C code.
- Each thread has a `filc_thread`, found through a pthread key
  (`filc_get_my_thread`). It is NULL on threads the runtime didn't create.
- Roots that native code can use:
  - `filc_thread_track_object` (scoped to the current native frame)
  - exact pointer tables (`filc_exact_ptr_table_*`), which keep objects alive
    until they are freed
  - globals
- Fil-C `free()` turns an object's capability into a freed one immediately.
  The GC can then reclaim the payload even while other pointers to it still
  exist. **Rust can never assume a Fil-C object stays valid across a pollcheck
  or a call into Fil-C.**
- A pointer stored in memory keeps its intval in the payload and its
  capability in an invisible aux slot. Storing a pointer needs a store barrier
  and an aux update (`filc_store_ptr`). Plain byte writes can't do it.

## Approaches considered

**A. In-process native host on glibc (chosen).** The Rust binary is a normal
glibc program. It loads the compat shim and then the Fil-C runtime, boots the
runtime, and calls Fil-C functions through the generic entrypoint, just as
libpizlo does. (An earlier version of this plan made the Rust binary itself a
yolo-musl program. That still works, but it isn't needed now.) Calls cost about as much as a C
call plus argument marshalling, and data can be shared without copying
everything. The downside is that we depend on runtime internals (the
`filc_thread` layout, cc buffers, aux encoding). Those are fixed for a given
compiler version but not across versions. We handle that by pinning the
version and checking it at startup.

**B. Out-of-process host.** A small Fil-C host program loads the library, and
Rust talks to it over IPC or shared memory. Fil-C panics stay isolated and we
don't depend on runtime internals, but every call pays IPC cost and needs full
serialization. We keep this as an optional later mode on top of the same
marshalling types (see below). It doesn't replace A.

**C. Compile Rust itself to Fil-C** (emit LLVM IR from rustc, then pizlonate
it). Safety would come for free, but rustc's LLVM is newer than Fil-C's clang
20, so the IR is incompatible. The core and std libraries also contain inline
asm, and all Rust code would pay Fil-C overhead. Rejected; this is a different
project.

**D. A Fil-C loader written in Rust.** Rejected, because it solves the wrong
problem: the loader isn't Fil-C-specific, and glibc's `ld.so` already loads
everything once the shim is in place. A custom loader would only matter if we
kept the real musl `libyoloc.so` inside a glibc process. That means two libcs
in one address space:
- each wants its own thread control block behind `%fs` (musl's
  `__pthread_self()->errno_val` would read glibc's TCB)
- both grow the heap with `brk`
- both expect to own the process-wide signal handlers and internal signals
  (musl reserves 32–34, glibc 32–33)
- musl's `dlopen`, `dl_iterate_phdr` and TLS need musl's own loader state

A Rust loader would end up reimplementing musl's `ldso` and still be fragile.

## Architecture

```
crates/fil-rust-sys       the C shims (runtime shim, yolo-libc stand-ins), built by build.rs and embedded in the crate
crates/fil-rust           safe API: runtime, executors, pointers, memory, calls, libraries, callbacks, handles
  examples/               runnable programs, each with its C/C++ source in examples/c
  tests/                  integration tests over C/C++ fixtures, run against either flavor
crates/fil-rust-macros    #[derive(FilType)]
crates/fil-rust-bindgen   generates FilType structs and foreign! interfaces from C headers (library + CLI)
crates/bindgen-demo       uses fil-rust-bindgen from a build script
nix/                      the pinned package set, the Fil-C distributions (pizfix for musl, optfil for glibc),
                          the runtime source headers, the build environment and the workspace package
default.nix, shell.nix    the Nix build and dev shell; both take `crossSystem`
```

Nothing Fil-C is linked into the Rust binary. Every native object fil-rust
needs (the runtime shim and the stand-ins) is compiled by `build.rs`, embedded
with `include_bytes!`, and loaded at run time from a `memfd`. The Fil-C
distribution itself is found at run time from [`Config`].

### Flavors

Fil-C has two user-libc flavors, selected per process with `Config::musl` /
`Config::glibc` (or `FILC_FLAVOR`), and compiled in with the `musl` (default)
and `glibc` cargo features. Both features can be on at once.

| | musl ("pizfix") | glibc ("optfil", `/opt/fil`) |
|---|---|---|
| user libc | `pizfix/lib/libc.so` | `lib/libc.so.6666`, `libm.so.6666`, `libdl.so.6662`, `libpthread.so.6660` |
| yolo libc the runtime imports | `libyoloc.so` (musl) | `libyolocimpl.so`, `libyolomimpl.so`, `ld-fil1-x86_64.so` (glibc 2.40-ish) |
| stand-ins fil-rust provides | `libyoloc.so` | those three, plus `libpizlo.so` (see below) |
| shim built against | musl headers | host glibc headers |

The glibc distribution's objects use **symbol versions**. glibc's `ld.so`
matches a versioned reference by version *name* anywhere in the global scope,
so a stand-in only needs the right soname, the right `VERDEF` nodes (from a
linker version script), and definitions for the symbols the host glibc
doesn't already export: the yolo futex wrappers, the internal-signal table,
and a few `__ieee754_*` aliases in libm. Everything else binds to the host
glibc.

The glibc distribution's `libpizlo.so` has no `DT_SONAME` and its user libs
have no RUNPATH, so their `DT_NEEDED libpizlo.so` can't match the runtime
loaded by path. An empty stand-in with soname `libpizlo.so` satisfies the
dependency by name; the runtime's symbols come from the real libpizlo, which
is loaded `RTLD_GLOBAL`.

The nix shell provides both distributions. optfil's compiler expects to live
at `/opt/fil`, so `nix/optfil.nix` wraps `filcc`/`fil++` in `bwrap` with the
distribution bind-mounted there.

### The runtime shim

Most of the runtime's native API is `static inline` code in
`filc_runtime.h`. Rather than copy layouts into Rust, `fil-rust-sys` compiles
`csrc/shim.c` against the runtime headers of the pinned source tag (fetched
by `nix/filc-runtime-src.nix`) and exposes a table of `fr_*` functions,
resolved with `dlsym` and checked by an ABI version number:

- threads: `fr_enter`, `fr_exit`, `fr_is_entered`, `fr_pollcheck`,
  `fr_with_native_frame`, `fr_check_thread_layout`
- memory: `fr_allocate`, `fr_check`, `fr_ptr_info_get`, `fr_copy_in/out`,
  `fr_memmove`, `fr_strlen`, `fr_load_ptr`, `fr_store_ptr` (with the store
  barrier and aux update), `fr_track`
- roots: `fr_root_set/get` over an exact pointer table
- calls: `fr_call_getter`, `fr_check_function`, `fr_cc_begin`,
  `fr_cc_set/get_word`, `fr_cc_set/get_ptr`, `fr_call`
- callbacks and executors: `fr_closure_new`, `fr_main_getter`,
  `fr_worker_function`, `fr_panic`

Every memory operation runs entered and checks the capability first, so the
garbage collector can't reclaim an object in the middle of an access.

### Startup

`Runtime::start(config)`:

1. **Loads in order:** the flavor's stand-ins (from memfds, `RTLD_GLOBAL`),
   then the distribution's `libpizlo.so` and user libc by path. The order is a
   safety invariant: a Fil-C object loaded first would pull in the real yolo
   libc through its RUNPATH. After loading, `dl_iterate_phdr` confirms that no
   real yolo libc is mapped; the same check runs after every `Runtime::load`.
2. **Loads the shim** and checks its ABI version and flavor.
3. **Boots.** `filc_start_program(argc, argv, __libc_start_main, main_getter)`
   never returns, so it runs on a dedicated std thread. The user libc
   initializes itself (stdio, environ, constructors) through its own
   `__libc_start_main` and then calls the "main" that `fr_main_getter`
   provides: a shim function that calls back into Rust. The flavors differ
   here: musl's runtime parks the boot thread in `pause()` and runs `main`
   on a new thread, while glibc's runs `main` on the boot thread itself.
   fil-rust works either way, and `tests/boot.rs` checks the resulting
   thread layout. Once inside, a check that the shim's view of
   `filc_thread` matches the runtime's (same thread, entered, correct tid,
   stack limit set) guards against a mismatched shim.
4. **Becomes the spawner.** The Fil-C main thread starts the initial executor
   threads and then serves requests to start more (below). It never returns,
   since returning from Fil-C `main` would call `exit()`.

The Rust `main` stays in charge. An `atexit` hook in the Rust process flushes
Fil-C stdio when the process exits normally.

### Executors

Rust threads are never adopted into the runtime. Instead Fil-C work runs on
**executor threads**, which are created by the Fil-C libc's own
`pthread_create` (so musl's per-thread state, like `errno`, exists) with the
shim's `fr_worker_function` as the start routine.

- `Runtime::run(f)` queues `f`, blocks the calling Rust thread until an
  executor has run it, and returns its result. Because it blocks, `f` may
  borrow from the caller. Called from a thread that is already a Fil-C thread
  (inside a job or a callback) it runs `f` inline in a nested scope.
- `Runtime::spawn(f)` queues a `'static` job and returns a `JoinHandle`.
- The pool is **elastic**: when queued jobs outnumber idle and starting
  executors, the spawner starts another, up to `Config::max_workers` (256 by
  default). Jobs that wait on each other therefore don't starve.
- Panics in jobs are caught on the executor and resumed in the joiner.

### Never block while entered

FUGC's soft handshakes wait for every entered thread to reach a pollcheck.
A thread that blocks on a Rust primitive while entered stalls every
collection; and since creating a Fil-C thread takes the handshake lock, it
can deadlock the pool too. (This was a real deadlock: a job blocked on a
`OnceLock` whose initializer was waiting for a new executor.) So:

- `Fil::exited(f)` leaves the runtime around blocking Rust code. Its closure
  must be `Send`, so it can't capture a `Fil` or a `FilPtr`.
- Internally, every lock that can be taken while entered is taken by
  `try_lock` first, falling back to exit → lock → enter. `JoinHandle` waits
  leave the runtime when called from a Fil-C thread.
- `Fil::pollcheck()` is for long Rust loops that don't call Fil-C.

### The context token: `Fil<'f>`

`Fil<'f>` proves the current thread is a Fil-C thread inside the runtime. It
is `!Send` and `!Sync`, and it only exists inside a `run`/`spawn` closure or
a callback. `'f` is invariant and higher-ranked (`for<'f> FnOnce(&Fil<'f>)`),
so nothing tied to it can escape the closure.

Every Rust → Fil-C transition pushes a native frame. Pointers that Rust
obtains are tracked in the innermost frame (`fr_track`) and stay alive until
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
  threads through an exact pointer table. It is `Send + Sync`; `get(fil)`
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

**Exceptions.** `fr_call` pushes a Fil-C frame whose origin has a
personality that catches every exception. If a C++ exception escapes the
callee, the unwinder runs the callee's cleanups (destructors) on its way to
that frame; the shim then disposes of the exception and returns a status that
becomes `Error::Exception`. Rust panics never cross the boundary.

### Libraries

`Runtime::load(path)` preloads the library's `DT_NEEDED` dependencies from the
configured search path and the distribution's lib directory, then `dlopen`s it
with the host loader, `RTLD_LOCAL`. Symbols are found through the
`pizlonated_<name>` getters, which return a properly bounded pointer to the
function object or global (globals are initialized lazily by their getter).
`Library::function` checks that the symbol is a function before returning a
`Function`; `Library::global::<T>` returns a typed pointer.

### Callbacks

`Callback<Sig>::new(fil, closure)` registers a `Send + Sync` closure in a Rust
table and creates a Fil-C closure function object (`fr_closure_new`) carrying
its id. When Fil-C calls it, a shim thunk enters Rust, which decodes the
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

## Safety argument (summary)

| hazard | prevented by |
|---|---|
| Rust forging a capability | `FilPtr` is opaque; aux values come only from the runtime |
| Rust accessing out of bounds, freed, read-only or special memory | every access is a copy checked against the live capability, entered |
| GC reclaiming an object Rust holds | pointers are tracked in the current native frame; `FilHandle` roots across frames |
| a `FilPtr` outliving its frame or thread | the invariant, higher-ranked `'f`; `Fil` and `FilPtr` are `!Send` |
| data races being UB in Rust | Rust never holds references into shared Fil-C memory; `FilBuf` is exclusive until shared |
| wrong Rust-side signature | the callee checks argument slots; a mismatched pointer has a null capability |
| Fil-C getting a pointer into Rust memory | impossible: data is copied into GC objects |
| unwinding across the boundary | exceptions → `Error::Exception`; Rust panics in callbacks → Fil-C panic |
| the GC stalled by blocked Rust code | `exited`; internal locks never block while entered |
| runtime ABI drift | shim ABI version and flavor checks at startup; pinned distribution versions |
| the real yolo libc entering the process | fil-rust owns all loading; `dl_iterate_phdr` check after each load |
| musl/glibc struct and constant differences | audited import list and layout probe; `sendmsg` padding fixed by redirect |
| signals landing on threads the runtime doesn't know | `sigaction` redirect forwards them to a Fil-C thread |

## Status

Implemented: everything above, for both flavors on x86-64 and for the musl
flavor on aarch64 (the glibc distribution is x86-64 only). The integration
tests (`tests/`) run against either flavor (`FILC_FLAVOR=glibc cargo test
--features glibc`) and cover scalar, string, struct, sret, varargs and
function-pointer calls; checked memory access and its failure modes; handles
and owned buffers under GC stress; callbacks from many Fil-C threads; C++
containers, threads and exceptions; process-ending cases (Fil-C safety
panics, signature mismatches, panicking callbacks, `exit`) in child processes;
the boot layout of each flavor; the musl/glibc boundary audit; signal
handling next to Rust's; and bindings generated from headers.

aarch64 is cross-built on an x86-64 host and tested under qemu-user
(`nix-shell --argstr crossSystem aarch64-unknown-linux-gnu --run 'cargo test'`,
which needs `binfmt_misc` for aarch64). The whole suite, the examples and
`bindgen-demo` pass there, with and without `FUGC_MIN_THRESHOLD=0`.

## Open questions and risks

- **Runtime internals aren't a stable API.** Mitigated by pinning versions,
  keeping the shim small, and checking its ABI version. Bumping Fil-C means
  rebuilding the shim and rerunning the tests on both flavors.
- **Running libpizlo next to the host libc isn't supported upstream.**
  This is audited by `tests/abi_audit.rs`:
  - Every libc symbol libpizlo imports is on a reviewed list
    (`tests/abi/<flavor>_<arch>_imports.txt`), so a Fil-C upgrade that adds an
    import fails until it is reviewed.
  - A probe (`tests/abi/layout_probe.c`) prints the size, alignment and field
    layout of every libc struct libpizlo passes (about 60 structs) and the
    value of about 400 constants it forwards unchanged. Built against musl's
    and against glibc's headers, the two outputs may differ only in a
    reviewed list. On x86-64 they differ in nine places: `rusage`, `sysinfo`
    and `sched_param` are larger in musl (glibc writes less than the buffer);
    `msghdr`'s length fields are `int` + padding in musl (libpizlo zeroes the
    struct); `semid_ds.sem_nsems` is narrower (the low bytes hold the value);
    musl's `O_LARGEFILE` is the real kernel bit (a no-op on 64-bit); two
    compile-time stack minimums are unused.
  - On aarch64 they also differ in four pthread types, and that was another
    real bug: glibc's `pthread_attr_t` and `pthread_mutex_t` are 8 bytes
    larger than musl's. glibc's state fits in musl's size, but
    `pthread_attr_init` (which `pthread_getattr_np` uses) and
    `pthread_mutex_init` clear the whole glibc size, so the first Fil-C
    thread overwrote its caller's saved frame pointer and crashed. The
    stand-in wraps the `pthread_attr_*` functions, `pthread_getattr_np`,
    `pthread_create` and `pthread_mutex_init` to work on a glibc-sized copy
    (aborting if glibc ever uses the extra bytes), and libpizlo's imports of
    them are redirected there. libpizlo passes no `pthread_mutexattr_t` or
    `pthread_condattr_t`, the other two.
  - One difference was a real bug: `cmsghdr.cmsg_len` is `int` + padding in
    musl, and libpizlo passes the program's control buffer through as is.
    musl's `sendmsg` zeroes that padding; glibc's has none to zero, so file
    descriptor passing with a dirty buffer failed with `EINVAL`. fil-rust
    now redirects libpizlo's `sendmsg`/`sendmmsg` imports (by rewriting its
    GOT entries after loading it; boot fails if an expected import is
    missing) to stand-in wrappers that do what musl does, and the audit tests
    the fix end to end.
  - The glibc flavor's libpizlo was built against the same libc family, and
    every versioned import is resolved at load time (`RTLD_NOW`).
- **Signals.** Checked by `tests/signals.rs`:
  - Rust's stack-overflow handler (SIGSEGV/SIGBUS on guard pages) keeps
    working on Rust threads once the runtime is up, since Fil-C never
    installs handlers for synchronous fault signals. A Rust overflow on an
    executor thread (whose guard Rust doesn't know) still ends the process
    with a fatal signal. A Fil-C stack overflow is a Fil-C panic.
  - Fil-C signal handlers run (`raise`, interval timers).
  - The runtime's native handler assumes it runs on a Fil-C thread, which
    holds in a pure Fil-C process but not next to Rust threads: a signal
    landing on one crashed the process. fil-rust redirects libpizlo's
    `sigaction` import so the runtime's handler sits behind a trampoline
    that forwards signals arriving on non-Fil-C threads to the Fil-C main
    thread (with their siginfo where the kernel allows it).
  - The stand-ins report glibc's internal signals (32 and 33) so the
    runtime leaves them alone.
- **Panics are process-fatal.** This is inherent to Fil-C. An out-of-process
  host (a Fil-C executable serving calls over shared memory) would let Rust
  survive them, at the cost of copying.
- **aarch64 is tested under emulation only.** qemu-user runs the suite, but
  its memory model is stronger than real hardware's, so weak-ordering bugs in
  the executor or callback paths could hide there. Other targets are
  rejected at build time.
