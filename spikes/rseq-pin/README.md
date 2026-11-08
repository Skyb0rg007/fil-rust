# rseq pins versus race-tolerant native code

A standalone C prototype measuring the two ways native code (a native libc such as glibc-rs)
could safely use memory that Fil-C threads may be writing at the same time.

1. **rseq pins.** Every Fil-C store becomes an rseq critical section that tests an extra
   `PINNED` bit (`0x40`, unused in Fil-C 0.687's flag byte) before storing. Native code pins
   an object by setting the bit, then calls `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ)`.
   Every section running on another CPU restarts and re-tests. After that, no store can land
   until the pin is cleared, so the native function gets real `noalias` access. Costs: every
   Fil-C store, plus a pin and sometimes a `membarrier` per native call.
2. **Race tolerance.** Fil-C is unchanged. Native code reads and writes shared memory only
   with relaxed atomics or opaque inline-asm moves, so it never relies on memory staying the
   same. Costs: lost optimizations in native code.

Dropping `restrict` alone is not one of the options. Without it the compiler still assumes
there are no data races, so it may still reload a value it already checked. The `plain`
columns are there only to separate the cost of `noalias` from the cost of race tolerance.

Run `./run.sh` (gcc), or `CLANG=/path/to/host/clang ./run.sh` to also build the kernels with
clang. Fil-C's own `clang` is the wrong compiler here.

## What's measured

- `filc_ops.h` writes out by hand what Fil-C 0.687 emits for a store:
  - test `READONLY|FREE` in the header's flag byte;
  - check the bounds;
  - store;
  - the loop's pollcheck.

  The rseq variant adds the section entry (`lea` of the descriptor and one store of it into
  the thread's rseq area, which glibc registers) and the `0x46` mask. The store is the
  section's last instruction. Loads are unchanged, because pinning doesn't block readers.
- `pin stores`: ns per store for sequential `u8`, sequential `u32`, random `u32` stores, and
  a `u32` load followed by a store. The variants are:
  - unchecked;
  - Fil-C today;
  - Fil-C with rseq.

  Sizes are 4 KiB, 256 KiB and 16 MiB.
- `pin native` (`kernels.c`): `add`, `sum`, `find` (memchr-like), `copy`, `accumulate`
  (`*out += a[i]*3`) and `histogram` (a shared byte used as a table index). Each comes in
  these variants:
  - `restrict`;
  - `plain`;
  - `atomic`: relaxed atomics on shared memory, what safe Rust over atomic views compiles to;
  - `asmvec`: 16-byte inline-asm loads and stores, race-tolerant and hand-vectorized with
    SSE2.

  They're built for baseline x86-64 (Rust's default target) and x86-64-v3.
- `pin barrier`: the cost of an uncontended pin and unpin, and `membarrier` latency with 0
  to 13 other threads running rseq store loops, or 13 asleep.
- `pin verify MODE [WIDEN]`: four writer threads store continuously into a 4 KiB object. The
  borrower pins it, snapshots it, holds it for 2 µs, and checks that nothing changed, 20,000
  times. The modes:
  - `full` is the protocol.
  - `nobarrier` skips `membarrier`; it's a negative control.
  - `nopin` skips the pin.

  `WIDEN` inserts that many `pause` instructions between the flag test and the store,
  making the gap visible.

## Results

From `results.txt` (2026-11-08): Core Ultra 7 255H, Linux 7.4.0-rc1, `powersave` governor,
gcc 16.2 and clang 21.1. The measuring thread runs on CPU 0, a performance core. Rows for the
4 KiB sequential loops moved by up to 2× between builds, with code alignment the likely
cause, so treat those as noise. The rest agreed across three runs.

### The protocol works, and the barrier matters more than expected

With the full protocol, no store landed during any of 80,000 borrows: either section entry,
with the gap widened or not. With the gap widened, about 80,000 restarts show the barrier
aborting writers mid-section.

Pinning without the barrier failed in essentially every borrow, even with the gap left
unwidened. The gap isn't the few instructions between the flag test and the store. It's
everything already in the writer core's store buffer. With four cores contending for the same
cache lines, stores that passed the test before the pin keep reaching memory for
microseconds. `membarrier` interrupts those cores, which drains their store buffers. That's
why it's required, not only a guard against rare preemption.

A side effect, visible only with the widened gap: a section longer than the gap between pins
never completes, because every pin restarts it. Real sections are a handful of instructions.

### Cost on Fil-C stores

These loops do nothing but store, so they bound the cost from above. Real code does other
work between stores, but whole-program cost wasn't measured.

| | Fil-C today | + rseq, store every time | + rseq, compare first |
|---|---|---|---|
| Sequential stores, L1 | baseline | +1 to +6% | +15 to +16% |
| Sequential stores, L2 and DRAM | baseline | +13 to +24% | +17 to +18% |
| Random stores, L1 | baseline | under +2% | under +2% |
| Random stores, L2 | baseline | +33% | 0% |
| Random stores, DRAM | baseline | +57% | 0% |
| Load then store | baseline | 0% (or slightly faster) | +6 to +11% |

- **Storing the descriptor every time** is nearly free while stores hit L1. Once they miss,
  the slot store waits behind them in the store buffer, which drains in order. That halves
  how many misses can be outstanding, so random stores to DRAM lose the most.
- **Comparing first** removes that, but adds instructions to every iteration, which shows
  in loops limited by instruction throughput.

Neither entry wins everywhere. A compiler could hoist the `lea` of the descriptor out of the
loop (inline asm can't), saving an instruction in both. That's untested.

### Cost per native call

- Pin plus unpin, uncontended: 15 to 18 ns across runs (two locked read-modify-writes).
- `membarrier`, only when an object goes from unpinned to pinned while other threads run:

| Other threads | Median | p99 | Max |
|---|---|---|---|
| none | 0.14 µs | 0.16 µs | 4 µs |
| 1 running | 1.5 µs | 1.6 µs | 5 µs |
| 5 running | 2.4 µs | 3.0 µs | 8 µs |
| 13 running (performance and efficiency cores) | 3.4 µs | 3.9 µs | 13 µs |
| 13 asleep | 0.3 µs | 0.4 µs | 2 µs |

An earlier run had one 449 µs call among 5,000 with 13 threads running. The cost follows the number of threads currently running, not the number that exist, and
never depends on the region's size.

### Cost of race tolerance in native code

As a multiple of the `restrict` version, over the three sizes:

| Kernel | `plain` (not safe) | `atomic` (compiled) | `asmvec` (SSE2 asm) |
|---|---|---|---|
| add, sum, copy | 0.9 to 1.2 | 1.1 to 7 | 0.9 to 5 |
| find (memchr-like) | 1.0 | 1.0 (not vectorized either way); 7 to 17 at x86-64-v3 with gcc | 0.1 to 1.9 |
| accumulate | 1.0 | 1.0 with gcc; 1.5 to 2.7 with clang | - |
| histogram (a shared byte as an index) | 1.0 | 1.0 | - |

- **`noalias` itself bought almost nothing.** `plain` matched `restrict` to within 10%,
  apart from one DRAM-bound case at 1.24×. Both compilers add runtime overlap checks and
  vectorize anyway.
- **What race tolerance costs is vectorization.** Compilers don't vectorize atomic accesses,
  so loops they would vectorize get slower. Scalar loops, including the histogram pattern where
  a reloaded index could go out of bounds, cost nothing.
- **Hand-written vector code recovers much of it.** Inline-asm loads are race-tolerant
  and vectorize. At the same 16-byte width they were within 1.25× of gcc's code. clang's
  unrolled loops and glibc's `memcpy` beat them by up to 2.3× in L1. Against AVX2 builds they
  were up to 5× slower, because this asm is SSE2 only. `find`
  stayed scalar except with gcc at x86-64-v3, so elsewhere the asm version is 10× faster than
  even `restrict`. AVX2 asm wasn't tried.

## Conclusion

The two approaches tax different code:

- **rseq pins** tax every Fil-C store in the program. That's 0 to 57% on store-only loops
  storing every time, or 0 to 18% comparing first, plus 15 to 18 ns per native call and
  sometimes a `membarrier` of 1.5 to 3.4 µs. In exchange, native code runs at full
  optimization with real references. They also need a Fil-C compiler change, and every store
  site must stay inside a section.
- **Race tolerance** taxes only native functions that loop over shared memory and would have
  been vectorized: 1.1 to 7× when the compiler does the work (up to 17× for one gcc AVX2
  case). Hand-written vector code removes much of that, and scalar code pays nothing. Fil-C
  is untouched.

The functions where race tolerance hurts (`memcpy`, `memchr`, `strlen`, `memcmp` and their
relatives) are the ones a libc already hand-writes in vector code. So race tolerance
concentrates its cost where libcs already spend their effort, while rseq spreads its cost
over all Fil-C code. On these numbers, race tolerance, with hand-written vector code for
the hot memory and string functions, is the cheaper default. rseq pins stay an option if
some native code can't be written that way.
