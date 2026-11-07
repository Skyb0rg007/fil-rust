//! The heap: non-moving, segregated by size, with its metadata out of band.
//!
//! Address space is reserved up front and split into fixed-size chunks. Each
//! chunk holds blocks of one size. Which chunk an address is in, and so which
//! block, needs no header: that is how aux arrays and atomic boxes, which have
//! none, can be marked. Large allocations take a run of whole chunks.
//!
//! Blocks are only ever reused after a collection proves them unreachable.
//! Reuse any sooner would be unsound, because compiled code keeps checks across
//! `free` and accepts a recycled or zeroed header. So `free` doesn't touch this
//! module at all.
//!
//! The allocator never starts a collection. Compiled code calls
//! `verse_local_allocator_allocate` where it isn't at a safepoint, so all it can
//! do is ask for one (see `gc`).

use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::abi::{AllocatorSlot, MIN_ALIGN, Thread};

pub const CHUNK_SIZE: usize = 1 << 20;
const CHUNK_SHIFT: u32 = CHUNK_SIZE.trailing_zeros();
/// Blocks at least this big get whole chunks.
const LARGE_THRESHOLD: usize = CHUNK_SIZE / 4;
/// The largest alignment `allocate_with_alignment` accepts.
pub const MAX_ALIGNMENT: usize = CHUNK_SIZE / 2;

/// Address space to reserve. This costs nothing until it's touched.
const RESERVE_SIZE: usize = 1 << 40;

/// Small classes are multiples of 16 and match the compiler's allocator slots.
const SMALL_CLASSES: usize = crate::abi::NUM_ALLOCATOR_SLOTS;
const MAX_SMALL_SIZE: usize = (SMALL_CLASSES - 1) * MIN_ALIGN;

/// Sizes of the medium classes, from `MAX_SMALL_SIZE` up to `LARGE_THRESHOLD`,
/// four to a power of two.
const MEDIUM_SIZES: [usize; 37] = {
    let mut sizes = [0; 37];
    let mut power = 512;
    let mut index = 0;
    while index < sizes.len() {
        let mut quarter = 0;
        while quarter < 4 && index < sizes.len() {
            let size = power + quarter * (power / 4);
            sizes[index] = if size > LARGE_THRESHOLD { LARGE_THRESHOLD } else { size };
            index += 1;
            quarter += 1;
        }
        power *= 2;
    }
    sizes
};
const NUM_CLASSES: usize = SMALL_CLASSES + MEDIUM_SIZES.len();

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ChunkKind {
    Free = 0,
    Blocks,
    LargeHead,
    LargeTail,
}

/// Everything known about a chunk. The table is reserved like the heap, so it
/// costs a page only for the parts of the heap that are in use.
#[derive(Clone, Copy)]
#[repr(C)]
struct Chunk {
    kind: ChunkKind,
    class: u8,
    /// For `Blocks`: the block size.
    block_size: u32,
    /// For `Blocks`: where blocks that have ever been handed out end. Memory
    /// beyond it has never been touched.
    carved: u32,
    /// For `LargeHead`: the size of the run in chunks. For `LargeTail`: how
    /// far back the head is, in chunks.
    run: u32,
    /// For `LargeHead`: the size of the block in bytes.
    large_size: usize,
    /// For `Blocks`: the blocks a sweep found to be free.
    free: *mut u8,
}

/// Where a class gets memory from, once thread slots are out of the picture.
struct ClassState {
    /// Chunks with blocks on their free lists, as chunk indices.
    partial: Vec<u32>,
    /// A chunk being carved by the central allocator (medium classes).
    bump: *mut u8,
    end: *mut u8,
    bump_chunk: usize,
    free: *mut u8,
}

struct Central {
    classes: [ClassState; NUM_CLASSES],
    /// Runs of free chunks, as `(first chunk, length)`, sorted by first chunk.
    free_runs: Vec<(u32, u32)>,
    /// One past the last chunk ever used.
    high_water: usize,
}

unsafe impl Send for Central {}

/// What the heap knows about itself. See [`crate::HeapStats`] for the public
/// view.
#[derive(Default)]
pub struct Counters {
    /// Bytes handed to allocators since the last collection.
    pub allocated_since_collection: AtomicUsize,
    /// Bytes in chunks that are in use.
    pub committed: AtomicUsize,
}

pub struct Heap {
    base: usize,
    chunks: *mut Chunk,
    marks: *mut u64,
    central: Mutex<Central>,
    pub counters: Counters,
}

unsafe impl Send for Heap {}
unsafe impl Sync for Heap {}

static HEAP: OnceLock<Heap> = OnceLock::new();

/// Reserves `size` bytes of zero pages that are committed when they're touched.
fn reserve(size: usize, protection: i32) -> *mut u8 {
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    let result = unsafe { libc::mmap(null_mut(), size, protection, flags, -1, 0) };
    if result == libc::MAP_FAILED {
        crate::failure::internal_panic(format_args!("cannot reserve {size} bytes of address space"));
    }
    result.cast()
}

pub fn heap() -> &'static Heap {
    HEAP.get_or_init(Heap::new)
}

/// The block size of `total`, or `None` if it takes whole chunks.
fn class_for(total: usize) -> Option<(usize, usize)> {
    if total <= MAX_SMALL_SIZE {
        let class = total.div_ceil(MIN_ALIGN).max(1);
        return Some((class, class * MIN_ALIGN));
    }
    if total >= LARGE_THRESHOLD {
        return None;
    }
    let index = MEDIUM_SIZES.partition_point(|&size| size < total);
    Some((SMALL_CLASSES + index, MEDIUM_SIZES[index]))
}

impl Heap {
    fn new() -> Heap {
        let reservation = reserve(RESERVE_SIZE + CHUNK_SIZE, libc::PROT_NONE) as usize;
        let base = (reservation + CHUNK_SIZE - 1) & !(CHUNK_SIZE - 1);
        let num_chunks = RESERVE_SIZE / CHUNK_SIZE;
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        Heap {
            base,
            chunks: reserve(num_chunks * size_of::<Chunk>(), rw).cast(),
            marks: reserve(RESERVE_SIZE / MIN_ALIGN / 8, rw).cast(),
            central: Mutex::new(Central {
                classes: std::array::from_fn(|_| ClassState {
                    partial: Vec::new(),
                    bump: null_mut(),
                    end: null_mut(),
                    bump_chunk: 0,
                    free: null_mut(),
                }),
                free_runs: Vec::new(),
                high_water: 0,
            }),
            counters: Counters::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Central> {
        self.central.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn contains(&self, address: usize) -> bool {
        address.wrapping_sub(self.base) < RESERVE_SIZE
    }

    fn chunk(&self, index: usize) -> *mut Chunk {
        self.chunks.wrapping_add(index)
    }

    fn chunk_address(&self, index: usize) -> *mut u8 {
        (self.base + (index << CHUNK_SHIFT)) as *mut u8
    }

    fn chunk_index(&self, address: usize) -> usize {
        (address - self.base) >> CHUNK_SHIFT
    }

    /// Makes `count` contiguous chunks usable.
    fn take_chunks(&self, central: &mut Central, count: usize) -> usize {
        let run = central.free_runs.iter().position(|&(_, length)| length as usize >= count);
        let first = if let Some(position) = run {
            let (first, length) = central.free_runs[position];
            if length as usize == count {
                central.free_runs.remove(position);
            } else {
                central.free_runs[position] = (first + count as u32, length - count as u32);
            }
            first as usize
        } else {
            let first = central.high_water;
            if (first + count) << CHUNK_SHIFT > RESERVE_SIZE {
                crate::failure::internal_panic(format_args!("the Fil-C heap is out of address space"));
            }
            let protection = libc::PROT_READ | libc::PROT_WRITE;
            let start = self.chunk_address(first);
            if unsafe { libc::mprotect(start.cast(), count << CHUNK_SHIFT, protection) } != 0 {
                crate::failure::internal_panic(format_args!("the Fil-C heap is out of memory"));
            }
            central.high_water = first + count;
            first
        };
        self.counters.committed.fetch_add(count << CHUNK_SHIFT, Ordering::Relaxed);
        first
    }

    fn give_back_chunks(&self, central: &mut Central, first: usize, count: usize) {
        let start = self.chunk_address(first);
        // The next user expects zero pages. Failure would only waste memory
        // if it weren't for that, so don't pretend it can be ignored.
        if unsafe { libc::madvise(start.cast(), count << CHUNK_SHIFT, libc::MADV_DONTNEED) } != 0 {
            crate::failure::internal_panic(format_args!("cannot release memory of the Fil-C heap"));
        }
        for index in first..first + count {
            unsafe { (*self.chunk(index)).kind = ChunkKind::Free };
        }
        self.counters.committed.fetch_sub(count << CHUNK_SHIFT, Ordering::Relaxed);
        let position = central.free_runs.partition_point(|&(start, _)| (start as usize) < first);
        central.free_runs.insert(position, (first as u32, count as u32));
        // Join with the neighbors, so large allocations can find room.
        if position + 1 < central.free_runs.len() {
            let (start, length) = central.free_runs[position];
            let (next, next_length) = central.free_runs[position + 1];
            if start + length == next {
                central.free_runs[position].1 += next_length;
                central.free_runs.remove(position + 1);
            }
        }
        if position > 0 {
            let (previous, previous_length) = central.free_runs[position - 1];
            let (start, length) = central.free_runs[position];
            if previous + previous_length == start {
                central.free_runs[position - 1].1 += length;
                central.free_runs.remove(position);
            }
        }
    }

    fn new_block_chunk(&self, central: &mut Central, class: usize, block_size: usize) -> usize {
        let index = self.take_chunks(central, 1);
        unsafe {
            *self.chunk(index) = Chunk {
                kind: ChunkKind::Blocks,
                class: class as u8,
                block_size: block_size as u32,
                carved: 0,
                run: 0,
                large_size: 0,
                free: null_mut(),
            };
        }
        index
    }

    /// Gets a slot going again once it has nothing left.
    #[cold]
    fn refill(&self, slot: &mut AllocatorSlot) {
        let class = slot.class;
        let block_size = class * MIN_ALIGN;
        let mut central = self.lock();
        if let Some(index) = central.classes[class].partial.pop() {
            let chunk = self.chunk(index as usize);
            slot.free = unsafe { std::mem::replace(&mut (*chunk).free, null_mut()) };
            return;
        }
        let index = self.new_block_chunk(&mut central, class, block_size);
        let start = self.chunk_address(index);
        let usable = CHUNK_SIZE / block_size * block_size;
        slot.bump = start;
        slot.end = start.wrapping_add(usable);
        slot.chunk = index + 1;
    }

    /// Allocates a block of the slot's class. The block isn't zeroed.
    #[inline]
    pub fn allocate_from_slot(&self, slot: &mut AllocatorSlot) -> *mut u8 {
        let block_size = slot.class * MIN_ALIGN;
        loop {
            if !slot.free.is_null() {
                let block = slot.free;
                slot.free = unsafe { *block.cast::<*mut u8>() };
                self.note_allocation(block_size);
                return block;
            }
            if slot.bump != slot.end {
                let block = slot.bump;
                slot.bump = block.wrapping_add(block_size);
                self.note_allocation(block_size);
                return block;
            }
            self.flush_slot(slot);
            self.refill(slot);
        }
    }

    #[inline]
    fn note_allocation(&self, size: usize) {
        self.counters.allocated_since_collection.fetch_add(size, Ordering::Relaxed);
    }

    /// Records how far a slot got into its chunk and detaches it.
    pub fn flush_slot(&self, slot: &mut AllocatorSlot) {
        if slot.chunk != 0 {
            let index = slot.chunk - 1;
            let used = slot.bump as usize - self.chunk_address(index) as usize;
            unsafe { (*self.chunk(index)).carved = used as u32 };
        }
        slot.free = null_mut();
        slot.bump = null_mut();
        slot.end = null_mut();
        slot.chunk = 0;
    }

    fn allocate_medium(&self, class: usize, block_size: usize) -> *mut u8 {
        let mut central = self.lock();
        loop {
            let state = &mut central.classes[class];
            if !state.free.is_null() {
                let block = state.free;
                state.free = unsafe { *block.cast::<*mut u8>() };
                self.note_allocation(block_size);
                return block;
            }
            if state.bump != state.end {
                let block = state.bump;
                state.bump = block.wrapping_add(block_size);
                let used = state.bump as usize - self.chunk_address(state.bump_chunk) as usize;
                unsafe { (*self.chunk(state.bump_chunk)).carved = used as u32 };
                self.note_allocation(block_size);
                return block;
            }
            if let Some(index) = state.partial.pop() {
                let chunk = self.chunk(index as usize);
                state.free = unsafe { std::mem::replace(&mut (*chunk).free, null_mut()) };
                continue;
            }
            let index = self.new_block_chunk(&mut central, class, block_size);
            let start = self.chunk_address(index);
            let state = &mut central.classes[class];
            state.bump = start;
            state.end = start.wrapping_add(CHUNK_SIZE / block_size * block_size);
            state.bump_chunk = index;
        }
    }

    fn allocate_large(&self, size: usize) -> *mut u8 {
        let count = size.div_ceil(CHUNK_SIZE);
        let mut central = self.lock();
        let first = self.take_chunks(&mut central, count);
        unsafe {
            *self.chunk(first) = Chunk {
                kind: ChunkKind::LargeHead,
                class: 0,
                block_size: 0,
                carved: 0,
                run: count as u32,
                large_size: size,
                free: null_mut(),
            };
            for offset in 1..count {
                *self.chunk(first + offset) = Chunk {
                    kind: ChunkKind::LargeTail,
                    class: 0,
                    block_size: 0,
                    carved: 0,
                    run: offset as u32,
                    large_size: 0,
                    free: null_mut(),
                };
            }
        }
        self.note_allocation(count << CHUNK_SHIFT);
        self.chunk_address(first)
    }

    /// Allocates a zeroed block of at least `size` bytes, aligned to 16. Never
    /// reaches a safepoint and never fails: running out of memory is fatal, as
    /// compiled code has no way to handle it.
    pub unsafe fn allocate(&self, thread: *mut Thread, size: usize) -> *mut u8 {
        let (block, zeroed_size) = match class_for(size) {
            Some((class, block_size)) if class < SMALL_CLASSES => {
                let slot = unsafe { &mut (&mut (*thread).slots)[class] };
                (self.allocate_from_slot(slot), block_size)
            }
            Some((class, block_size)) => (self.allocate_medium(class, block_size), block_size),
            // Chunks always come back as zero pages.
            None => return self.allocate_large(size),
        };
        unsafe { ptr::write_bytes(block, 0, zeroed_size) };
        block
    }

    /// The start of the block holding `address`, which must be in the heap, in
    /// a block that exists. Only used while the world is stopped.
    pub fn block_of(&self, address: usize) -> (usize, usize) {
        let index = self.chunk_index(address);
        let chunk = unsafe { &*self.chunk(index) };
        match chunk.kind {
            ChunkKind::Blocks => {
                let start = self.chunk_address(index) as usize;
                let block_size = chunk.block_size as usize;
                (start + (address - start) / block_size * block_size, block_size)
            }
            ChunkKind::LargeHead => (self.chunk_address(index) as usize, chunk.large_size),
            ChunkKind::LargeTail => {
                let head = index - chunk.run as usize;
                let size = unsafe { (*self.chunk(head)).large_size };
                (self.chunk_address(head) as usize, size)
            }
            ChunkKind::Free => crate::failure::internal_panic(format_args!(
                "address {address:#x} is in a free chunk of the Fil-C heap"
            )),
        }
    }

    /// Sets the mark bit of the block starting at `block`. Tells whether it was
    /// clear.
    pub fn mark(&self, block: usize) -> bool {
        let bit = (block - self.base) / MIN_ALIGN;
        let word = unsafe { &mut *self.marks.wrapping_add(bit / 64) };
        let mask = 1u64 << (bit % 64);
        let was_clear = *word & mask == 0;
        *word |= mask;
        was_clear
    }

    fn take_mark(&self, block: usize) -> bool {
        let bit = (block - self.base) / MIN_ALIGN;
        let word = unsafe { &mut *self.marks.wrapping_add(bit / 64) };
        let mask = 1u64 << (bit % 64);
        let was_set = *word & mask != 0;
        *word &= !mask;
        was_set
    }

    /// Frees everything that isn't marked, and clears the marks. All thread
    /// slots must be flushed and the world stopped. Returns the bytes reclaimed
    /// and the bytes still live.
    pub fn sweep(&self, scribble: bool) -> (usize, usize) {
        let mut central = self.lock();
        for state in &mut central.classes {
            state.partial.clear();
            state.free = null_mut();
            // Blocks past the bump pointer stay in their chunk's `carved`
            // accounting, so the rest of the chunk isn't lost.
            state.bump = null_mut();
            state.end = null_mut();
        }
        let mut reclaimed = 0;
        let mut live = 0;
        let mut index = 0;
        while index < central.high_water {
            let chunk = unsafe { *self.chunk(index) };
            match chunk.kind {
                ChunkKind::Free | ChunkKind::LargeTail => index += 1,
                ChunkKind::LargeHead => {
                    let run = chunk.run as usize;
                    if self.take_mark(self.chunk_address(index) as usize) {
                        live += chunk.large_size;
                    } else {
                        reclaimed += chunk.large_size;
                        self.give_back_chunks(&mut central, index, run);
                    }
                    index += run;
                }
                ChunkKind::Blocks => {
                    let (freed, kept) = self.sweep_chunk(&mut central, index, chunk, scribble);
                    reclaimed += freed;
                    live += kept;
                    index += 1;
                }
            }
        }
        (reclaimed, live)
    }

    fn sweep_chunk(&self, central: &mut Central, index: usize, chunk: Chunk, scribble: bool) -> (usize, usize) {
        let start = self.chunk_address(index) as usize;
        let block_size = chunk.block_size as usize;
        let mut free: *mut u8 = null_mut();
        let mut freed = 0;
        let mut kept = 0;
        let mut offset = 0;
        while offset < chunk.carved as usize {
            let block = start + offset;
            if self.take_mark(block) {
                kept += block_size;
            } else {
                let cell = block as *mut u8;
                if scribble {
                    unsafe { ptr::write_bytes(cell, 0xdb, block_size) };
                }
                unsafe { *cell.cast::<*mut u8>() = free };
                free = cell;
                freed += block_size;
            }
            offset += block_size;
        }
        if kept == 0 {
            self.give_back_chunks(central, index, 1);
            return (freed, 0);
        }
        let usable = CHUNK_SIZE / block_size * block_size;
        // What was never carved can still be used: hand it out as free blocks.
        let mut offset = chunk.carved as usize;
        while offset < usable {
            let cell = (start + offset) as *mut u8;
            unsafe { *cell.cast::<*mut u8>() = free };
            free = cell;
            offset += block_size;
        }
        unsafe {
            (*self.chunk(index)).free = free;
            (*self.chunk(index)).carved = usable as u32;
        }
        if !free.is_null() {
            central.classes[chunk.class as usize].partial.push(index as u32);
        }
        (freed, kept)
    }
}
