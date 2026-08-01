//! Owning generational-index storage — `DESIGN.md` §2.6.
//!
//! [`SlotMap<T>`] holds values and hands out [`Handle<T>`]s to them. It is the
//! container for engine-owned resources with an obvious owner: GPU objects,
//! loaded assets, scene nodes.
//!
//! It is deliberately *not* the ECS entity store. Component data lives in
//! archetype columns (§2.10), so entities need generation bookkeeping without a
//! payload — that is [`HandleAllocator`](crate::HandleAllocator), a separate
//! type sharing the same [`Handle`]. See `PLAN.md` §4.1-C.

use std::num::NonZeroU32;

use crate::Handle;

/// The first generation any slot carries. Zero is reserved as the niche that
/// makes `Option<Handle<T>>` free, and as the "obviously invalid" value that
/// [`Handle::from_raw`] rejects.
const FIRST_GENERATION: NonZeroU32 = NonZeroU32::new(1).unwrap();

/// A slot is either holding a value or sitting in the free list. It cannot be
/// both, and modelling it as an enum rather than an `Option<T>` beside a
/// `next_free` field makes that impossible to get wrong.
enum Slot<T> {
    Occupied(T),
    Vacant { next_free: Option<u32> },
}

/// Generational-index storage that owns its values.
///
/// Removing a value bumps the slot's generation immediately, so every
/// outstanding handle to it stops resolving at the moment of removal rather
/// than when the slot is eventually reused.
pub struct SlotMap<T> {
    slots: Vec<Slot<T>>,
    /// Parallel to `slots`. Kept separate so that iteration over generations
    /// during lookup does not pull `T`-sized data into cache.
    generations: Vec<NonZeroU32>,
    free_head: Option<u32>,
    len: usize,
}

impl<T> SlotMap<T> {
    /// A map with no allocation.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            generations: Vec::new(),
            free_head: None,
            len: 0,
        }
    }

    /// Preallocate room for `capacity` values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            generations: Vec::with_capacity(capacity),
            free_head: None,
            len: 0,
        }
    }

    /// Number of occupied slots. Not the same as the number of slots, which
    /// includes retired and free ones.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether any value is stored.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Store a value and return a handle to it.
    ///
    /// Reuses a free slot when one is available, so handles stay dense and the
    /// backing `Vec` does not grow under insert/remove churn.
    ///
    /// # Panics
    ///
    /// If the map would exceed `u32::MAX` slots. The index field is 32 bits by
    /// design (`PLAN.md` §4.1-C); exhausting it is resource exhaustion, not a
    /// recoverable condition.
    pub fn insert(&mut self, value: T) -> Handle<T> {
        match self.free_head {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                let Slot::Vacant { next_free } = slot else {
                    unreachable!("free list only ever links vacant slots");
                };

                self.free_head = *next_free;
                *slot = Slot::Occupied(value);
                self.len += 1;

                // The generation was already bumped when this slot was freed,
                // so the handle issued here cannot collide with a stale one.
                Handle::new(index, self.generations[index as usize])
            }
            None => {
                let index = u32::try_from(self.slots.len())
                    .expect("SlotMap exceeded u32::MAX slots; index field is 32 bits by design");

                self.slots.push(Slot::Occupied(value));
                self.generations.push(FIRST_GENERATION);
                self.len += 1;

                Handle::new(index, FIRST_GENERATION)
            }
        }
    }

    /// Remove a value, returning it if the handle is still live.
    ///
    /// Returns `None` for a stale handle rather than panicking — releasing
    /// something another subsystem still references is normal during hot reload
    /// and in the editor (`CONVENTIONS.md` §6).
    pub fn remove(&mut self, handle: Handle<T>) -> Option<T> {
        if !self.contains(handle) {
            return None;
        }

        let index = handle.index();
        let slot = std::mem::replace(
            &mut self.slots[index as usize],
            Slot::Vacant {
                next_free: self.free_head,
            },
        );
        self.len -= 1;

        // Bump on free, not on allocate: every handle to this slot must stop
        // resolving now, even if the slot is never handed out again.
        //
        // On overflow the slot is retired instead of wrapping. Wrapping would
        // eventually make an ancient handle compare equal to a live one, which
        // is silent aliasing; leaking one slot after four billion reuses is the
        // better failure.
        match self.generations[index as usize].checked_add(1) {
            Some(next) => {
                self.generations[index as usize] = next;
                self.free_head = Some(index);
            }
            None => {
                // Retired: deliberately not linked into the free list. Restore
                // the link we displaced so the list stays intact.
                self.slots[index as usize] = Slot::Vacant { next_free: None };
            }
        }

        match slot {
            Slot::Occupied(value) => Some(value),
            Slot::Vacant { .. } => unreachable!("contains() confirmed the slot was occupied"),
        }
    }

    /// Whether the handle still refers to a live value.
    pub fn contains(&self, handle: Handle<T>) -> bool {
        self.slot_of(handle).is_some()
    }

    /// Borrow the value, if the handle is still live.
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        match self.slot_of(handle)? {
            Slot::Occupied(value) => Some(value),
            Slot::Vacant { .. } => None,
        }
    }

    /// Mutably borrow the value, if the handle is still live.
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        if !self.contains(handle) {
            return None;
        }

        match &mut self.slots[handle.index() as usize] {
            Slot::Occupied(value) => Some(value),
            Slot::Vacant { .. } => None,
        }
    }

    /// Remove every value, invalidating all outstanding handles.
    pub fn clear(&mut self) {
        for index in 0..self.slots.len() {
            if matches!(self.slots[index], Slot::Occupied(_)) {
                // Route through `remove` so retirement and free-list handling
                // stay in exactly one place.
                let handle = Handle::new(index as u32, self.generations[index]);
                self.remove(handle);
            }
        }
    }

    /// Iterate live values with their handles, in slot order.
    ///
    /// Slot order is stable for a given sequence of operations, which is what
    /// the deterministic headless mode in `DESIGN.md` §5 requires.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot {
                Slot::Occupied(value) => {
                    Some((Handle::new(index as u32, self.generations[index]), value))
                }
                Slot::Vacant { .. } => None,
            })
    }

    /// Iterate live values.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.iter().map(|(_, value)| value)
    }

    /// Iterate handles to live values.
    pub fn keys(&self) -> impl Iterator<Item = Handle<T>> + '_ {
        self.iter().map(|(handle, _)| handle)
    }

    /// Mutably iterate live values with their handles, in slot order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Handle<T>, &mut T)> {
        let generations = &self.generations;
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(move |(index, slot)| match slot {
                Slot::Occupied(value) => {
                    Some((Handle::new(index as u32, generations[index]), value))
                }
                Slot::Vacant { .. } => None,
            })
    }

    /// Mutably iterate live values.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.iter_mut().map(|(_, value)| value)
    }

    /// The generation check, in one place. `None` means stale, out of range, or
    /// pointing at a vacant slot.
    fn slot_of(&self, handle: Handle<T>) -> Option<&Slot<T>> {
        let index = handle.index() as usize;

        if self.generations.get(index)? != &handle.generation() {
            return None;
        }

        match self.slots.get(index)? {
            slot @ Slot::Occupied(_) => Some(slot),
            Slot::Vacant { .. } => None,
        }
    }
}

impl<T> Default for SlotMap<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SlotMap<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_value_resolves_through_its_handle() {
        let mut map = SlotMap::new();

        let handle = map.insert("mesh");

        assert_eq!(map.get(handle), Some(&"mesh"));
        assert!(map.contains(handle));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn stale_handle_does_not_resolve_after_slot_reuse() {
        let mut map = SlotMap::new();
        let old = map.insert("first");
        map.remove(old);

        let new = map.insert("second");

        assert_eq!(old.index(), new.index(), "the slot should have been reused");
        assert_ne!(old, new, "but the handle must not be equal");
        assert_eq!(map.get(old), None, "stale handle must not resolve");
        assert_eq!(map.get(new), Some(&"second"));
    }

    #[test]
    fn handle_stops_resolving_the_moment_it_is_freed() {
        // Bump-on-free, not bump-on-allocate: staleness must not wait for the
        // slot to be handed out again.
        let mut map = SlotMap::new();
        let handle = map.insert("gone");

        map.remove(handle);

        assert_eq!(map.get(handle), None);
        assert!(!map.contains(handle));
    }

    #[test]
    fn remove_returns_the_value_once() {
        let mut map = SlotMap::new();
        let handle = map.insert(String::from("owned"));

        assert_eq!(map.remove(handle), Some(String::from("owned")));
        assert_eq!(map.remove(handle), None);
        assert!(map.is_empty());
    }

    #[test]
    fn get_mut_writes_through_to_storage() {
        let mut map = SlotMap::new();
        let handle = map.insert(1_u32);

        *map.get_mut(handle).expect("handle is live") = 42;

        assert_eq!(map.get(handle), Some(&42));
    }

    #[test]
    fn free_slots_are_reused_before_growing() {
        let mut map = SlotMap::new();
        let a = map.insert('a');
        let b = map.insert('b');
        map.remove(a);
        map.remove(b);

        let c = map.insert('c');
        let d = map.insert('d');

        assert_eq!(map.len(), 2);
        assert!(c.index() < 2 && d.index() < 2, "should not have grown");
        assert_ne!(c.index(), d.index());
    }

    #[test]
    fn iteration_skips_freed_slots_and_stays_in_slot_order() {
        let mut map = SlotMap::new();
        let a = map.insert(10);
        let _b = map.insert(20);
        let c = map.insert(30);
        map.remove(a);
        map.remove(c);

        let seen: Vec<i32> = map.values().copied().collect();

        assert_eq!(seen, vec![20]);
    }

    #[test]
    fn iteration_yields_handles_that_still_resolve() {
        let mut map = SlotMap::new();
        map.insert(1);
        map.insert(2);

        let handles: Vec<_> = map.keys().collect();

        assert_eq!(handles.len(), 2);
        for handle in handles {
            assert!(map.contains(handle));
        }
    }

    #[test]
    fn iter_mut_writes_through_to_storage() {
        let mut map = SlotMap::new();
        let handle = map.insert(1);

        for value in map.values_mut() {
            *value += 1;
        }

        assert_eq!(map.get(handle), Some(&2));
    }

    #[test]
    fn clear_invalidates_every_outstanding_handle() {
        let mut map = SlotMap::new();
        let a = map.insert('a');
        let b = map.insert('b');

        map.clear();

        assert!(map.is_empty());
        assert_eq!(map.get(a), None);
        assert_eq!(map.get(b), None);
    }

    #[test]
    fn handles_from_one_map_do_not_resolve_against_a_shorter_one() {
        let mut long = SlotMap::new();
        for n in 0..4 {
            long.insert(n);
        }
        let far = long.keys().last().expect("four inserts");

        let mut short = SlotMap::new();
        short.insert(0);

        // Out-of-range index must be rejected rather than panicking.
        assert_eq!(short.get(far), None);
    }

    #[test]
    fn slot_is_retired_rather_than_wrapping_at_max_generation() {
        let mut map = SlotMap::new();
        let handle = map.insert('x');
        let index = handle.index();

        // Drive the slot to the last usable generation directly; reaching it
        // honestly would take four billion insert/remove cycles.
        map.generations[index as usize] = NonZeroU32::new(u32::MAX).expect("nonzero");
        let doomed = Handle::new(index, NonZeroU32::new(u32::MAX).expect("nonzero"));
        map.remove(doomed);

        let next = map.insert('y');

        assert_ne!(
            next.index(),
            index,
            "a slot at max generation must not be reused, or an ancient handle \
             would eventually compare equal to a live one"
        );
        assert_eq!(map.get(doomed), None);
    }
}
