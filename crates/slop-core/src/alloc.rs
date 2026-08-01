//! Generation bookkeeping without storage — `docs/DESIGN.md` §2.6.
//!
//! [`HandleAllocator<T>`] issues and validates [`Handle<T>`]s for things whose
//! data lives somewhere else. The motivating case is ECS entities: an entity is
//! an ID, and its components live in archetype columns (§2.10), so there is no
//! single array for a [`SlotMap`](crate::SlotMap) to own.
//!
//! Staleness behaves exactly as it does in [`SlotMap`](crate::SlotMap) —
//! generations bump on free, and a slot at the last generation is retired rather
//! than wrapped. The two types are deliberately separate rather than one type
//! with an optional payload, because the ECS would pay for a payload it never
//! uses on every entity. See `docs/PLAN.md` §4.1-C.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use crate::Handle;

/// See [`slotmap`](crate::SlotMap): zero is reserved so `Option<Handle<T>>` is
/// free and so obviously-invalid handles are rejectable.
const FIRST_GENERATION: NonZeroU32 = NonZeroU32::new(1).unwrap();

/// Hands out generational handles for externally stored data.
pub struct HandleAllocator<T> {
    generations: Vec<NonZeroU32>,
    live: Vec<bool>,
    /// A stack rather than an intrusive linked list — with no payload slot to
    /// borrow a `next` pointer from, a stack is both simpler and more
    /// cache-friendly to pop from.
    free: Vec<u32>,
    len: usize,
    _tag: PhantomData<fn() -> T>,
}

impl<T> HandleAllocator<T> {
    /// An allocator with no allocation.
    pub fn new() -> Self {
        Self {
            generations: Vec::new(),
            live: Vec::new(),
            free: Vec::new(),
            len: 0,
            _tag: PhantomData,
        }
    }

    /// Preallocate room for `capacity` handles.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            generations: Vec::with_capacity(capacity),
            live: Vec::with_capacity(capacity),
            free: Vec::new(),
            len: 0,
            _tag: PhantomData,
        }
    }

    /// Number of live handles.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether any handle is live.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total slots ever created, live or not. Callers storing data in a parallel
    /// array size it by this, since handle indices range over it.
    pub fn slot_count(&self) -> usize {
        self.generations.len()
    }

    /// Issue a new handle, reusing a freed slot when one is available.
    ///
    /// # Panics
    ///
    /// If the allocator would exceed `u32::MAX` slots — see
    /// [`SlotMap::insert`](crate::SlotMap::insert).
    pub fn allocate(&mut self) -> Handle<T> {
        match self.free.pop() {
            Some(index) => {
                self.live[index as usize] = true;
                self.len += 1;

                // Already bumped at free time, so this cannot collide with an
                // outstanding stale handle.
                Handle::new(index, self.generations[index as usize])
            }
            None => {
                let index = u32::try_from(self.generations.len()).expect(
                    "HandleAllocator exceeded u32::MAX slots; index field is 32 bits by design",
                );

                self.generations.push(FIRST_GENERATION);
                self.live.push(true);
                self.len += 1;

                Handle::new(index, FIRST_GENERATION)
            }
        }
    }

    /// Release a handle. Returns whether it was live — a stale or foreign handle
    /// is a no-op rather than a panic, matching
    /// [`SlotMap::remove`](crate::SlotMap::remove).
    pub fn free(&mut self, handle: Handle<T>) -> bool {
        if !self.is_live(handle) {
            return false;
        }

        let index = handle.index() as usize;
        self.live[index] = false;
        self.len -= 1;

        // Bump on free so the handle dies now, not at reuse. On overflow the
        // slot is retired — simply never pushed back onto the free stack —
        // because wrapping would eventually let an ancient handle compare equal
        // to a live one.
        if let Some(next) = self.generations[index].checked_add(1) {
            self.generations[index] = next;
            self.free.push(handle.index());
        }

        true
    }

    /// Whether the handle is still live.
    pub fn is_live(&self, handle: Handle<T>) -> bool {
        let index = handle.index() as usize;

        self.generations.get(index) == Some(&handle.generation())
            && self.live.get(index).copied().unwrap_or(false)
    }

    /// Release every handle.
    pub fn clear(&mut self) {
        for index in 0..self.generations.len() {
            if self.live[index] {
                let handle = Handle::new(index as u32, self.generations[index]);
                self.free(handle);
            }
        }
    }

    /// Iterate live handles in slot order, which is stable for a given sequence
    /// of operations (`docs/DESIGN.md` §5).
    pub fn iter(&self) -> impl Iterator<Item = Handle<T>> + '_ {
        self.live
            .iter()
            .enumerate()
            .filter(|(_, live)| **live)
            .map(|(index, _)| Handle::new(index as u32, self.generations[index]))
    }
}

impl<T> Default for HandleAllocator<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Debug for HandleAllocator<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandleAllocator")
            .field("live", &self.len)
            .field("slots", &self.generations.len())
            .field("free", &self.free.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Entity;

    #[test]
    fn allocated_handle_is_live() {
        let mut alloc = HandleAllocator::<Entity>::new();

        let handle = alloc.allocate();

        assert!(alloc.is_live(handle));
        assert_eq!(alloc.len(), 1);
    }

    #[test]
    fn stale_handle_does_not_resolve_after_slot_reuse() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let old = alloc.allocate();
        alloc.free(old);

        let new = alloc.allocate();

        assert_eq!(old.index(), new.index(), "the slot should have been reused");
        assert_ne!(old, new);
        assert!(!alloc.is_live(old));
        assert!(alloc.is_live(new));
    }

    #[test]
    fn handle_stops_resolving_the_moment_it_is_freed() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let handle = alloc.allocate();

        assert!(alloc.free(handle));

        assert!(!alloc.is_live(handle));
    }

    #[test]
    fn freeing_twice_reports_the_second_as_not_live() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let handle = alloc.allocate();

        assert!(alloc.free(handle));
        assert!(!alloc.free(handle));
        assert!(alloc.is_empty());
    }

    #[test]
    fn out_of_range_handle_is_rejected_rather_than_panicking() {
        let mut small = HandleAllocator::<Entity>::new();
        small.allocate();

        let mut large = HandleAllocator::<Entity>::new();
        for _ in 0..8 {
            large.allocate();
        }
        let far = large.iter().last().expect("eight allocations");

        assert!(!small.is_live(far));
        assert!(!small.free(far));
    }

    #[test]
    fn free_slots_are_reused_before_growing() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let a = alloc.allocate();
        let b = alloc.allocate();
        alloc.free(a);
        alloc.free(b);

        alloc.allocate();
        alloc.allocate();

        assert_eq!(alloc.slot_count(), 2, "should not have grown");
        assert_eq!(alloc.len(), 2);
    }

    #[test]
    fn iteration_yields_only_live_handles_in_slot_order() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let a = alloc.allocate();
        let b = alloc.allocate();
        let c = alloc.allocate();
        alloc.free(b);

        let live: Vec<_> = alloc.iter().collect();

        assert_eq!(live, vec![a, c]);
    }

    #[test]
    fn clear_invalidates_every_outstanding_handle() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let a = alloc.allocate();
        let b = alloc.allocate();

        alloc.clear();

        assert!(alloc.is_empty());
        assert!(!alloc.is_live(a));
        assert!(!alloc.is_live(b));
    }

    #[test]
    fn slot_is_retired_rather_than_wrapping_at_max_generation() {
        let mut alloc = HandleAllocator::<Entity>::new();
        let handle = alloc.allocate();
        let index = handle.index();

        alloc.generations[index as usize] = NonZeroU32::new(u32::MAX).expect("nonzero");
        let doomed = Handle::new(index, NonZeroU32::new(u32::MAX).expect("nonzero"));
        alloc.free(doomed);

        let next = alloc.allocate();

        assert_ne!(
            next.index(),
            index,
            "a slot at max generation must not be reused"
        );
        assert!(!alloc.is_live(doomed));
    }

    #[test]
    fn staleness_matches_slotmap_for_the_same_operation_sequence() {
        // The two containers must agree, or code that swaps one for the other
        // silently changes when handles die.
        let mut map = crate::SlotMap::<Entity>::new();
        let mut alloc = HandleAllocator::<Entity>::new();

        let map_old = map.insert(Entity);
        let alloc_old = alloc.allocate();
        map.remove(map_old);
        alloc.free(alloc_old);
        let map_new = map.insert(Entity);
        let alloc_new = alloc.allocate();

        assert_eq!(map_old.index(), alloc_old.index());
        assert_eq!(map_old.generation(), alloc_old.generation());
        assert_eq!(map_new.index(), alloc_new.index());
        assert_eq!(map_new.generation(), alloc_new.generation());
    }
}
