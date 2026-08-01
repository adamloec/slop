//! Frame-scoped bump allocation — `docs/CONVENTIONS.md` §8.
//!
//! [`FrameArena`] is a fixed block of memory with a bump pointer. Allocation is
//! an add and a bounds check; there is no per-value free. The whole block is
//! reclaimed at once by [`reset`](FrameArena::reset), once per frame.
//!
//! This exists so that per-frame scratch — visible-object lists, draw call
//! batches, job payloads — costs nothing. The alternative, a `Vec` per frame,
//! puts the global allocator in the frame loop and makes frame time depend on
//! heap state.
//!
//! # Why the capacity is fixed
//!
//! The arena does not grow. Exceeding it panics rather than falling back to the
//! heap, because an arena that silently grows hides exactly the per-frame
//! allocation it was introduced to eliminate — the frame would still hitch, and
//! nothing would say so. A hard limit turns that into a loud, reproducible
//! failure with a number attached, which is what `docs/DESIGN.md` §5's budget
//! discipline needs.
//!
//! # Why values are not dropped
//!
//! Reset moves a pointer; it does not walk allocations. Types needing `Drop`
//! are therefore rejected at compile time rather than leaked at runtime.

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::fmt;
use std::mem::{align_of, size_of};
use std::ptr::NonNull;

/// Alignment of the backing block. A cache line, which covers every scalar and
/// SIMD type the engine uses, so per-allocation alignment never has to reach
/// past the block's own guarantee.
const BLOCK_ALIGN: usize = 64;

/// A fixed-capacity bump allocator, reset once per frame.
///
/// Allocation takes `&self` rather than `&mut self` — the exception to
/// `docs/CONVENTIONS.md` §5, and the reason is that an allocator handing out one
/// borrow at a time would be useless. Each call returns a disjoint region, so
/// there is no aliasing to hide. [`reset`](FrameArena::reset) takes `&mut self`,
/// which is what makes this sound: the borrow checker will not let a frame end
/// while any allocation from it is still held.
pub struct FrameArena {
    block: NonNull<u8>,
    capacity: usize,
    offset: Cell<usize>,
    /// Peak usage since construction. Survives [`reset`] so a frame budget can
    /// be sized from a real run rather than guessed.
    high_water: Cell<usize>,
}

impl FrameArena {
    /// Reserve `capacity` bytes up front.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero, or the allocation fails.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "FrameArena capacity must be nonzero");

        let layout = Layout::from_size_align(capacity, BLOCK_ALIGN)
            .expect("capacity overflows when aligned to a cache line");

        // SAFETY: `layout` has a nonzero size, which is `alloc`'s requirement.
        let ptr = unsafe { alloc::alloc(layout) };
        let block = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));

        Self {
            block,
            capacity,
            offset: Cell::new(0),
            high_water: Cell::new(0),
        }
    }

    /// Bytes handed out since the last reset.
    pub fn used(&self) -> usize {
        self.offset.get()
    }

    /// Total bytes available between resets.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes still available this frame.
    pub fn remaining(&self) -> usize {
        self.capacity - self.offset.get()
    }

    /// Peak usage across every frame so far. This is the number to size a
    /// capacity from.
    pub fn high_water(&self) -> usize {
        self.high_water.get()
    }

    /// Reclaim everything at once.
    ///
    /// Takes `&mut self`, so this cannot compile while any allocation from this
    /// arena is still borrowed. That is the entire safety argument for the
    /// `&self` allocation methods.
    pub fn reset(&mut self) {
        self.offset.set(0);
    }

    /// Move a value into the arena.
    ///
    /// # Panics
    ///
    /// If the remaining capacity cannot fit `T`. The message carries the
    /// requested size and what was left, because a budget overrun is only
    /// actionable with numbers.
    // Returning `&mut` from `&self` is the defining shape of a bump allocator,
    // and it is sound here: every call bumps the offset before returning, so no
    // two calls can hand out overlapping regions, and `reset` takes `&mut self`
    // so nothing can be rewound while a borrow is outstanding. Taking `&mut
    // self` instead would permit exactly one live allocation at a time, which
    // defeats the purpose.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        const {
            assert!(
                !std::mem::needs_drop::<T>(),
                "FrameArena never runs destructors; reset only moves a pointer. \
                 Store a type without Drop, or own the value elsewhere."
            );
        }

        let ptr = self.alloc_raw(size_of::<T>(), align_of::<T>()).cast::<T>();

        // SAFETY: `alloc_raw` returned a region of at least `size_of::<T>()`
        // bytes aligned for `T`, uninitialized and not aliased by any other
        // allocation from this arena. Writing initializes it.
        unsafe {
            ptr.write(value);
            &mut *ptr
        }
    }

    /// Copy a slice into the arena.
    ///
    /// # Panics
    ///
    /// If the remaining capacity cannot fit `src`.
    // See `alloc` for why `&mut` from `&self` is correct here.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> &mut [T] {
        const {
            assert!(
                !std::mem::needs_drop::<T>(),
                "FrameArena never runs destructors; reset only moves a pointer."
            );
        }

        if src.is_empty() {
            return &mut [];
        }

        let bytes = size_of::<T>()
            .checked_mul(src.len())
            .expect("slice size overflows usize");
        let ptr = self.alloc_raw(bytes, align_of::<T>()).cast::<T>();

        // SAFETY: `alloc_raw` returned `bytes` of uninitialized, correctly
        // aligned memory not aliased by any other allocation from this arena,
        // and `src` is a valid readable slice of exactly that length. The two
        // cannot overlap, since `src` is borrowed and the arena region is fresh.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), ptr, src.len());
            std::slice::from_raw_parts_mut(ptr, src.len())
        }
    }

    /// Bump the pointer, returning uninitialized memory.
    ///
    /// Zero-sized requests get a dangling-but-aligned pointer and consume
    /// nothing, matching what the standard library does for ZSTs.
    fn alloc_raw(&self, size: usize, align: usize) -> *mut u8 {
        assert!(
            align <= BLOCK_ALIGN,
            "FrameArena blocks are {BLOCK_ALIGN}-byte aligned; type requires {align}"
        );

        if size == 0 {
            return align as *mut u8;
        }

        let current = self.offset.get();
        // The block base is BLOCK_ALIGN-aligned and `align` divides it, so
        // aligning the offset is enough to align the address.
        let start = current.next_multiple_of(align);
        let end = start
            .checked_add(size)
            .expect("arena offset overflows usize");

        assert!(
            end <= self.capacity,
            "FrameArena exhausted: needed {size} bytes (aligned to {align}), \
             {remaining} of {capacity} remaining. Raise the capacity or allocate less.",
            remaining = self.capacity - current,
            capacity = self.capacity,
        );

        self.offset.set(end);
        if end > self.high_water.get() {
            self.high_water.set(end);
        }

        // SAFETY: `end <= capacity`, so `start` is within the block.
        unsafe { self.block.as_ptr().add(start) }
    }
}

impl Drop for FrameArena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, BLOCK_ALIGN)
            .expect("capacity was validated in with_capacity");

        // SAFETY: `block` came from `alloc::alloc` with this exact layout in
        // `with_capacity`, and has not been freed — this is the only `dealloc`.
        // Allocated values need no destructors, which `alloc` enforces at
        // compile time.
        unsafe { alloc::dealloc(self.block.as_ptr(), layout) }
    }
}

// SAFETY: the arena owns its block exclusively and shares nothing. Sending it to
// another thread moves the block with it. It is deliberately NOT `Sync`: `Cell`
// makes concurrent `alloc` calls a data race, so each thread gets its own arena
// rather than sharing one.
unsafe impl Send for FrameArena {}

impl fmt::Debug for FrameArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameArena")
            .field("used", &self.used())
            .field("capacity", &self.capacity)
            .field("high_water", &self.high_water())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_value_reads_back() {
        let arena = FrameArena::with_capacity(1024);

        let value = arena.alloc(42_u32);

        assert_eq!(*value, 42);
    }

    #[test]
    fn allocations_do_not_overlap() {
        let arena = FrameArena::with_capacity(1024);

        let a = arena.alloc(1_u64);
        let b = arena.alloc(2_u64);
        *a = 10;

        assert_eq!(*a, 10);
        assert_eq!(
            *b, 2,
            "writing through one allocation must not touch another"
        );
    }

    #[test]
    fn slice_is_copied_not_aliased() {
        let arena = FrameArena::with_capacity(1024);
        let source = [1_u32, 2, 3, 4];

        let copied = arena.alloc_slice_copy(&source);
        copied[0] = 99;

        assert_eq!(copied, &[99, 2, 3, 4]);
        assert_eq!(source, [1, 2, 3, 4], "the source must be untouched");
    }

    #[test]
    fn empty_slice_consumes_nothing() {
        let arena = FrameArena::with_capacity(1024);

        let empty = arena.alloc_slice_copy::<u32>(&[]);

        assert!(empty.is_empty());
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn allocations_are_correctly_aligned() {
        let arena = FrameArena::with_capacity(1024);

        // A one-byte allocation first, so the next offset is deliberately odd.
        arena.alloc(1_u8);
        let wide = arena.alloc(1_u64);

        let address = std::ptr::from_mut(wide) as usize;
        assert_eq!(address % align_of::<u64>(), 0);
    }

    #[test]
    fn reset_reuses_the_same_memory() {
        let mut arena = FrameArena::with_capacity(1024);

        let first = std::ptr::from_mut(arena.alloc(1_u32)) as usize;
        arena.reset();
        let second = std::ptr::from_mut(arena.alloc(2_u32)) as usize;

        assert_eq!(first, second, "reset must rewind the bump pointer");
        assert_eq!(arena.used(), size_of::<u32>());
    }

    #[test]
    fn high_water_survives_reset() {
        // This is the number a frame budget gets sized from, so it must not be
        // cleared by the reset that happens every frame.
        let mut arena = FrameArena::with_capacity(1024);
        arena.alloc_slice_copy(&[0_u8; 100]);

        arena.reset();
        arena.alloc(1_u8);

        assert_eq!(arena.used(), 1);
        assert_eq!(arena.high_water(), 100);
    }

    #[test]
    fn remaining_tracks_what_was_taken() {
        let arena = FrameArena::with_capacity(256);

        arena.alloc_slice_copy(&[0_u8; 64]);

        assert_eq!(arena.used(), 64);
        assert_eq!(arena.remaining(), 192);
    }

    #[test]
    #[should_panic(expected = "FrameArena exhausted")]
    fn exhaustion_panics_rather_than_growing() {
        // Silently falling back to the heap would hide the frame hitch this
        // type exists to prevent.
        let arena = FrameArena::with_capacity(64);

        arena.alloc_slice_copy(&[0_u8; 128]);
    }

    #[test]
    fn capacity_can_be_filled_exactly() {
        let arena = FrameArena::with_capacity(64);

        arena.alloc_slice_copy(&[0_u8; 64]);

        assert_eq!(arena.remaining(), 0);
    }

    #[test]
    fn zero_sized_allocation_consumes_nothing() {
        let arena = FrameArena::with_capacity(64);

        arena.alloc(());

        assert_eq!(arena.used(), 0);
    }
}
