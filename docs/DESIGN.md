# fil-rust design

Status: plan, nothing implemented yet. Written against Fil-C 0.687 (pizfix /
musl distribution, x86_64).

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

**Consequence:** Rust code can live in a Fil-C process as a peer of libpizlo:
native code linked against `libyoloc.so` and loaded by `ld-fil1-x86_64.so`.
Rust cannot live in a glibc process and `dlopen` a Fil-C library. The loader,
libc, and runtime all have to be Fil-C's.

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

Struct-by-value arguments and returns follow clang's LLVM-IR lowering for
x86-64 SysV (coercion to integer pairs, `sret` pointers). They don't follow the
C source signature.

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

**A. In-process native host (chosen).** The Rust binary is a yolo-musl program
that boots the Fil-C runtime itself and calls Fil-C functions through the
generic entrypoint, just as libpizlo does. Calls cost about as much as a C
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

## Architecture

```
fil-rust-sys     raw bindings to libpizlo + a small C shim, pinned to one Fil-C version
fil-rust         safe API: runtime context, pointers, buffers, calls, libraries, callbacks
fil-rust-macros  #[fil_rust::main], foreign-function declarations, #[derive(FilRepr)]
nix/, .cargo/    toolchain: musl target, linker wrapper, runtime headers
```

### fil-rust-sys and the C shim

Most of the runtime's native API is `static inline` code in
`filc_runtime.h` / `filc_runtime_inlines.h`. Examples are cursors,
`filc_ptr_create_with_object`, `filc_store_ptr`, frame push/pop, and pollcheck.
Reimplementing it in Rust would copy layout details by hand. Instead,
`fil-rust-sys` compiles a small **yolo C shim** against the runtime headers from
the matching source tag. Nix fetches those headers alongside the binary
distribution. The shim exports plain `extern "C"` functions:

- `fr_initialize`, `fr_enter`, `fr_exit`, `fr_pollcheck`, `fr_my_thread`
- `fr_push_native_frame` / `fr_pop_native_frame`, `fr_track(ptr)`
- `fr_allocate(size, align) -> filc_ptr`, `fr_ptr_lower/upper/is_readonly/is_free`
- `fr_load_ptr(base, off)`, `fr_store_ptr(base, off, val)` (with the barrier)
- `fr_call_begin(arg_size)`, `fr_arg_int{8,16,32,64}`, `fr_arg_f{32,64}`,
  `fr_arg_ptr`, `fr_call(fn_ptr) -> (has_exception, return_size)`,
  `fr_ret_*` (all built on the runtime's own cursor functions)
- `fr_handle_table_new / encode / decode` (exact pointer table)
- `fr_function_new(thunk, data_ptr) -> filc_ptr` (a closure function object
  for callbacks)
- `fr_version()`, checked against `zversion()` at startup

Later we can move hot paths (`fr_arg_*`) into Rust for speed, once tests pin
down their behavior.

### Bootstrapping: `#[fil_rust::main]`

The Rust binary targets `x86_64-unknown-linux-musl`, linked dynamically against
`libyoloc.so`, with `ld-fil1-x86_64.so` as its interpreter. Its startup mirrors
`filc_crt.o`:

```rust
#[fil_rust::main]
fn main(fil: &mut Fil) { ... }
```

The macro expands to a native `main` that calls
`filc_start_program(argc, argv, pizlonated___libc_start_main, rust_main_getter)`.
`rust_main_getter` returns a function object whose generic entrypoint is a Rust
thunk. So the user libc gets fully initialized (stdio, environ, ctors) by its
own `__libc_start_main`, and then Rust code runs inside it on an already
entered `filc_thread`. Returning from the closure returns from Fil-C `main`, so
`exit()` runs atexit handlers as usual.

Linking caveat: both libcs are called `libc`, and the user libc's file is
literally `libc.so`. Rust's `-lc` must resolve to `libyoloc.so`. A linker
wrapper (`fil-rust-link`, shipped by the Nix shell and set in
`.cargo/config.toml`) rewrites `-lc`, sets `-dynamic-linker`, and adds
`pizfix/lib` to the RUNPATH.

### The context token: `Fil`

```rust
pub struct Fil { thread: NonNull<filc_thread>, _not_send: PhantomData<*mut ()> }
```

- If you have a `&Fil` or `&mut Fil`, the current thread is entered. `Fil` is
  `!Send`/`!Sync`.
- Calling into Fil-C (or anything that can pollcheck or free) takes
  **`&mut Fil`**.
- Views into Fil-C memory borrow **`&Fil`**.

The borrow checker then rules out the main hazard statically: a Rust reference
into a Fil-C object can't outlive the next call or pollcheck. In other words,
no view survives a point where Fil-C code could free the object or the GC could
reclaim it.

- `fil.exited(|| blocking_io())` exits the runtime around blocking Rust code.
  The closure gets no `Fil`, so it can't touch GC memory.
- `fil.pollcheck()` is for long-running Rust loops.
- Every Rust → Fil-C call pushes a native frame. Pointers created during the
  call are tracked in that frame and released when it is popped.

### Pointers: `FilPtr<'f>`

`FilPtr` is an opaque `{ptr, lower}`. Safe code **can't forge one**. You only
get one:

- from the runtime (allocation, a call's return value, a pointer loaded from
  memory, a global, `dlsym`), or
- as `FilPtr::null()` or `FilPtr::from_addr(usize)`, which have a null
  capability. These are like Fil-C's integer-to-pointer casts, so they are
  harmless.

The `'f` lifetime ties a `FilPtr` to the native frame that keeps it alive.
`fil.scope(|fil| ...)` opens a nested frame. To keep a pointer across scopes or
store it in Rust structs, convert it to a `FilHandle`. A handle is a strong
root held in an exact pointer table. It stays valid until Fil-C code frees the
object (decoding then returns a pointer with a freed capability, which still
can't be used to cause harm). `FilHandle` is `Send` and is decoded with a `Fil`
on any thread.

Pointer arithmetic (`ptr.offset(n)`) only changes the intval, as in Fil-C.
Bounds are checked when the pointer is accessed, not when it is computed.

### Memory: tracked buffers

This is the "Rust types for Fil-C tracked memory" idea, split by who might be
accessing the memory at the same time:

| type | what it is | access from Rust |
|---|---|---|
| `FilBuf` | GC allocation made by Rust and **never passed to Fil-C** | `as_slice(&Fil) -> &[u8]`, `as_mut_slice(&mut Fil)` (exclusive, so Rust references are sound) |
| `FilPtr` / `FilHandle` | anything Fil-C might also be accessing | **copy only**: `read(&Fil, off, &mut [u8])`, `write(&Fil, off, &[u8])`, `read_val::<T: Pod>`, `write_val`, `read_cstr` |

- Passing a `FilBuf` to a Fil-C function **consumes** it (`buf.share()` turns
  it into a `FilPtr`). Fil-C may keep the pointer and use it from another
  thread, so Rust can't safely hand out `&[u8]` into it again.
- Copy accessors re-check the capability (lower ≤ p, p + n ≤ upper, not
  readonly for writes, not freed) **while entered** and with no pollcheck in
  between. The GC therefore can't reclaim the object partway through a copy.
- Copies of shared memory use relaxed per-byte or per-word atomic accesses
  (`atomic_memcpy`-style). A Fil-C thread racing with Rust then produces torn
  values, never Rust UB. This mirrors Fil-C's own rule that races can't break
  memory safety.
- Pointer fields are read and written only through `read_ptr` / `write_ptr`,
  which go through the shim's barrier and aux handling. A plain byte write
  over a pointer slot just leaves a pointer whose intval doesn't match its
  capability. Fil-C allows that and it is safe.
- `unsafe fn as_slice_unchecked(&self, &Fil) -> &[u8]` is the escape hatch
  for callers who know the library doesn't retain or free a buffer.

Convenience types built on these: `FilCString` (allocate and copy, plus a NUL
terminator), `FilBox<T: FilRepr>`, and `FilArray<T>`.

### Struct layout: `#[derive(FilRepr)]`

The derive covers `#[repr(C)]` structs mirroring C structs. It produces:

- field offsets
- a pointer-field map, so pointer fields get `FilPtr` accessors that go
  through `read_ptr` / `write_ptr` while every other field is plain `Pod`
- typed `read` / `write` for `FilPtr<T>`

The layout is checked against the C header in tests, using a test-only Fil-C
probe that prints `offsetof` values.

### Calls

The low level is an untyped call builder (`fil.call(f).arg_i32(1).arg_ptr(p).ret_i32()?`)
on top of the shim cursor functions. The high level is typed declarations:

```rust
fil_rust::foreign! {
    lib = "libfoo.so";
    fn add(a: i32, b: i32) -> i32;
    fn dup_upper(s: FilPtr<c_char>) -> FilPtr<c_char>;
    static global_counter: i32;
}
```

These expand to typed wrappers taking `&mut Fil`. Calls return
`Result<R, FilException>`: if `has_exception` is set (a C++ exception
escaped), the exception is reported instead of unwinding into Rust. Returns
are read through checked cursors, so a too-small `return_size` panics the
runtime rather than reading garbage.

Supported argument and return types, in order of milestones:

1. Integers, floats, `FilPtr<T>`, `bool`, unit.
2. Structs by value: a `FilRepr` struct also gets a classification matching
   clang's x86-64 SysV lowering (up to 16 bytes → one or two integer/SSE
   slots, larger → `sret` / byval pointer). Golden tests compare against the
   IR that clang emits for the same prototypes.
3. Varargs: Fil-C passes varargs in the same buffer (see `zcan_va_arg`). This
   needs a spike to confirm the layout.

Later, the declaration bodies can be generated from C headers with bindgen.
Bindgen's output provides the types, and a wrapper pass maps C pointers to
`FilPtr<T>`.

### Libraries and symbols

- **Link time:** list the library as a NEEDED dependency (through the linker
  wrapper). Look symbols up with the yolo `dlsym(RTLD_DEFAULT, "pizlonated_<name>")`
  and call the getter to get a `FilPtr`.
- **Run time:** `Library::open(fil, path)` calls Fil-C's own `dlopen` /
  `dlsym` from the user libc, as ordinary Fil-C calls. This keeps the runtime's
  relocation, deferred-ctor, and global-initialization handling exactly as for
  Fil-C callers, and `dlsym` returns a properly capability-checked `FilPtr`.

`Library::function(name) -> FilFn` checks that the pointer is a function
object (`filc_check_function_call`) before any call.

### Callbacks (Fil-C → Rust)

`fil.callback(|fil, args: FilArgs| -> FilRet { ... }) -> FilFnPtr` creates a
closure function object through the runtime's closure support
(`zclosure_new` / `filc_native_zclosure_new` semantics):

- The generic entrypoint is a Rust `extern "C"` thunk.
- The closure data is a small GC object holding a `FilHandle` index into a
  Rust-side table of boxed closures.
- The thunk rebuilds a `Fil` for the current thread and reads its arguments
  through the checked cursor. So Fil-C passing the wrong arguments panics the
  runtime instead of corrupting Rust.
- Rust panics are caught at the thunk boundary and turned into a Fil-C panic
  (abort). Unwinding never crosses the boundary.

Typed wrappers (`FilFnPtr<fn(i32) -> i32>`) work the same way as in the
calls section above.

### Threads

Threads spawned with `std::thread` have no `filc_thread`, so they can't call
Fil-C. `fil_rust::thread::spawn(fil, |fil| ...)` creates the thread with
Fil-C's own `zthread_create`, called as a normal Fil-C function with a Rust
callback. The thread is then registered, entered, and joinable through
`zthread_join`. Adopting threads that already exist is out of scope until the
runtime has an API for it.

### Optional: out-of-process isolation

A `fil-rust-host` Fil-C executable could load a library and serve calls over a
socket plus shared memory, reusing `FilRepr` for serialization. Rust would then
use the same `foreign!` declarations with `Remote` instead of `Fil`. A Fil-C
panic would kill only the host process, and Rust would get it as an `Err`.
This only gets built if someone needs to survive panics.

## Safety argument (summary)

| hazard | prevented by |
|---|---|
| Rust forging a capability | `FilPtr` is opaque; aux values come only from the runtime |
| Rust writing out of bounds of a Fil-C object | every access is checked against the live capability in the shim/accessor |
| Fil-C freeing memory Rust is reading | views borrow `&Fil`; calls and pollchecks take `&mut Fil` |
| GC reclaiming an object mid-access | accesses happen entered, with no pollcheck in between |
| data races on shared memory being UB in Rust | shared memory is copy-only, using atomic copies |
| wrong Rust-side signature | the callee checks argument slots; a mismatched type becomes a null capability |
| Fil-C getting a pointer into Rust memory | impossible: Rust memory never gets a capability; data must be copied into GC objects |
| unwinding across the boundary | exceptions → `Err`; Rust panics in callbacks → abort |
| runtime ABI drift | crate pinned to one Fil-C version, checked via `zversion()` at startup |

## Milestones

**M0: spikes (go/no-go for approach A).** Throwaway code in `spikes/`:
1. A Rust musl binary dynamically linked against `libyoloc.so` with the
   `ld-fil1` interpreter prints "hello", with `-lc` redirected. Check that the
   Rust std's signal handlers (stack-overflow SIGSEGV) don't clash with the
   runtime's signal handling.
2. Boot the runtime from Rust with a Rust-provided `pizlonated_main` getter.
   Check that the user-libc `printf` works when called from Fil-C.
3. Hand-written call of `add` and `dup_upper` from a test `.so` through the
   generic entrypoint, using a C shim built against the 0.687 headers.
4. Confirm the struct-return and varargs buffer layouts.

**M1: `fil-rust-sys`.** Nix fetches the runtime headers at the pinned tag; the
shim gets built by `build.rs`; linker wrapper and `.cargo/config.toml`; version
check.

**M2: safe core.** `Fil`, `#[fil_rust::main]`, scopes and native frames,
`FilPtr`, `FilHandle`, `FilBuf`, copy accessors, `FilCString`, untyped calls
with scalars and pointers. Tests build Fil-C fixtures with `filcc` in
`build.rs`.

**M3: libraries and typed calls.** Link-time and `dlopen` loading, `foreign!`,
globals, exceptions → `Err`.

**M4: callbacks and threads.**

**M5: structs.** `FilRepr` derive, by-value struct ABI, varargs, bindgen
integration.

**M6 (optional): out-of-process isolation.**

## Open questions and risks

- **Runtime internals aren't a stable API.** Mitigated by pinning the version,
  keeping the shim minimal, and checking `zversion()`. Bumping Fil-C means
  rebuilding the shim and rerunning the ABI golden tests.
- **Rust std on yolo musl.** It should work, since the musl ABI matches Rust's
  musl target. The risks are the dynamic-linking flags and Rust's own `crt`
  objects (M0.1).
- **Signals.** The runtime defers signals for entered threads and installs its
  own handlers. Rust std's SIGSEGV/SIGBUS stack-guard handler may need to be
  disabled or reordered.
- **Panics are process-fatal in-process.** This is inherent to Fil-C. M6 is
  the answer if it matters.
- **aarch64.** The design is architecture-neutral except for the struct-ABI
  classifier. The first target is x86_64 only.
