//! One component type's storage inside one archetype.
//!
//! A contiguous, type-erased array. Every element is the same type, laid out
//! back to back, sized and aligned by a [`TypeInfo`] rather than by a Rust
//! generic parameter.
//!
//! # Why type-erased
//!
//! `docs/DESIGN.md` §2.4: a component type may be declared at runtime by a WASM
//! guest, so there is no Rust type to be generic over. `Column<T>` cannot exist
//! for a `T` the host was never compiled against. What does exist is a
//! `TypeInfo` carrying size, alignment and a destructor, and that is exactly
//! enough to allocate, index, move and free.
//!
//! Typed access is layered on top: a caller who *does* have the Rust type
//! resolves the whole column to a `&[T]` once, then iterates natively. The type
//! erasure costs one check per column per query, not one per element.
//!
//! # Why contiguous
//!
//! §2.10 picked archetype storage so that iteration is a linear scan, and §2.3
//! requires handing a guest module contiguous columns of component data in
//! shared linear memory. Both want exactly this array. A column that is
//! blittable ([`Transfer::Blittable`]) can be handed across the WASM boundary as
//! raw bytes with no gather step; one that is not, cannot, and
//! [`Column::as_bytes`] enforces that rather than trusting the caller.
//!
//! # This module is a sanctioned home for `unsafe`
//!
//! `docs/PLAN.md` §7 confines `unsafe` to `slop-rhi` and the allocator. This is
//! the third: type-erased storage is raw pointer arithmetic by construction, and
//! there is no safe formulation of it. Every block carries a `// SAFETY:`
//! comment, and the invariants the whole file rests on are stated on
//! [`Column`].

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::Cell;
use std::ptr::NonNull;

use slop_reflect::{Transfer, TypeId, TypeInfo};

use crate::{ElementTicks, Tick};

/// Elements reserved by the first allocation.
///
/// Small: most archetypes in a real scene hold a handful of entities, and the
/// long tail of one-entity archetypes is what an over-eager initial capacity
/// wastes memory on.
const INITIAL_CAPACITY: usize = 4;

/// A contiguous array of one component type.
///
/// # Invariants
///
/// Held by every method, and what the `unsafe` blocks below rely on:
///
/// 1. `data` is dangling-but-aligned when `capacity == 0`, and otherwise points
///    at an allocation of exactly `layout.size() * capacity` bytes aligned to
///    `layout.align()`.
/// 2. Elements `0..len` are initialized. Elements `len..capacity` are not.
/// 3. `len <= capacity`.
/// 4. `layout.size()` may be zero, in which case no allocation is ever made and
///    `capacity` is `usize::MAX` — a zero-sized type needs no memory and must
///    never be pointer-arithmetic'd over.
/// 5. `added` and `changed` each hold exactly `len` entries, and entry `n`
///    describes element `n`. They travel with the elements through every
///    operation, which is why they live here rather than beside the column.
pub struct Column {
    type_id: TypeId,
    /// One element's layout, not the whole array's.
    layout: Layout,
    drop_in_place: Option<unsafe fn(*mut u8)>,
    transfer: Transfer,
    data: NonNull<u8>,
    len: usize,
    capacity: usize,
    /// When each element was attached to its entity.
    added: Vec<Tick>,
    /// When each element was last written.
    ///
    /// `Cell` because a mutable query resolves its columns through `&Archetype`
    /// and stamps them from there — the same reason [`as_ptr`](Self::as_ptr)
    /// hands out a `*mut u8` from `&self`.
    ///
    /// A plain `Vec<Tick>` would also work today: `Vec::as_ptr` yields a pointer
    /// whose provenance comes from the vector's own internal raw pointer rather
    /// than from the `&self` borrow, so casting it mutable and writing through
    /// it passes Miri under both Stacked and Tree Borrows. That was measured,
    /// not assumed. It is still the wrong choice — it rests on an implementation
    /// detail of `Vec` that nothing guarantees, and it needs an `unsafe` block
    /// to express. `Cell` states the shared mutation in the type, needs no
    /// `unsafe`, and costs nothing: it has the same size and alignment as what
    /// it wraps.
    changed: Vec<Cell<Tick>>,
}

// SAFETY: a `Column` owns its allocation exclusively and hands out raw pointers
// only through methods that borrow it. Whether the component type is itself
// `Send`/`Sync` is not knowable here — a runtime-declared type has no Rust type
// to ask — so this is asserted at the level above: `docs/DESIGN.md` §2.3 makes
// component data plain data, and §2.5's scheduler hands disjoint archetypes to
// disjoint threads without ever sharing a column.
unsafe impl Send for Column {}
// SAFETY: as above, and note what the `Cell` in `changed` widens this to: a
// shared reference now permits writing a stamp, so `Sync` asserts that no two
// threads hold `&Column` while either mutates. That holds by construction —
// component data is only written through `World::query_mut`, which takes
// `&mut World`, and §2.5's scheduler hands disjoint archetypes to disjoint
// threads without ever sharing a column.
unsafe impl Sync for Column {}

impl Column {
    /// An empty column for the type `info` describes.
    ///
    /// Allocates nothing until the first push.
    pub fn new(info: &TypeInfo) -> Self {
        let layout = info.layout();

        Self {
            type_id: info.id(),
            layout,
            drop_in_place: info.drop_in_place(),
            transfer: info.transfer(),
            // Dangling but correctly aligned, which is what Rust requires of a
            // pointer to zero elements.
            data: NonNull::dangling(),
            len: 0,
            // A zero-sized type never allocates, so it can hold any number of
            // elements. Saying so up front removes every "is it zero-sized?"
            // branch from `reserve`.
            capacity: if layout.size() == 0 { usize::MAX } else { 0 },
            added: Vec::new(),
            changed: Vec::new(),
        }
    }

    /// The component type this column holds.
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// One element's size and alignment.
    pub fn element_layout(&self) -> Layout {
        self.layout
    }

    /// How many elements are stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the column is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether this column's bytes mean anything outside this address space.
    pub fn is_blittable(&self) -> bool {
        self.transfer.is_blittable()
    }

    /// Append one element by copying `value`'s bytes.
    ///
    /// Takes ownership: the caller must not drop or otherwise use the value
    /// afterward. This is a move, and a Rust move is a bitwise copy.
    ///
    /// # Safety
    ///
    /// `value` must point at an initialized, properly aligned value of exactly
    /// this column's component type, and must be treated as moved-from
    /// afterward — `std::mem::forget` it, or read it from a `ManuallyDrop`.
    pub unsafe fn push(&mut self, value: *const u8, tick: Tick) {
        self.reserve_one();

        // SAFETY: `reserve_one` guarantees element `len` is within the
        // allocation, the two regions are distinct allocations so they cannot
        // overlap, and the caller guarantees `value` is an initialized value of
        // the right type and size.
        unsafe {
            std::ptr::copy_nonoverlapping(value, self.element_ptr(self.len), self.layout.size());
        }

        self.push_ticks(ElementTicks::new(tick));
        self.len += 1;
    }

    /// Reserve one element and return a pointer to write it into.
    ///
    /// The migration path's destination: moving a component between archetypes
    /// means relocating bytes, and there is no owned value to hand to
    /// [`push`](Self::push) in between. Pairing this with
    /// [`swap_remove_to`](Self::swap_remove_to) moves a component with no
    /// intermediate buffer and no destructor run.
    ///
    /// # Safety
    ///
    /// The returned slot is **uninitialized**, and the column's length already
    /// counts it. The caller must write a complete, valid value of this
    /// column's component type before anything else touches the column —
    /// including before it can be dropped, because the destructor will run over
    /// this element.
    ///
    /// Violating that leaves invariant 2 broken, which is undefined behaviour
    /// the next time the column is read, dropped, or grown.
    pub unsafe fn push_uninit(&mut self, tick: Tick) -> *mut u8 {
        self.reserve_one();

        // SAFETY: `reserve_one` guarantees element `len` is within the
        // allocation. The element is uninitialized, which is exactly what this
        // returns a pointer to and what the caller undertakes to fix.
        let slot = unsafe { self.element_ptr(self.len) };

        self.push_ticks(ElementTicks::new(tick));
        self.len += 1;

        slot
    }

    /// When element `index` was added and last changed.
    pub fn ticks(&self, index: usize) -> Option<ElementTicks> {
        Some(ElementTicks {
            added: *self.added.get(index)?,
            changed: self.changed.get(index)?.get(),
        })
    }

    /// Overwrite element `index`'s stamps.
    ///
    /// The migration path: a component relocating to another archetype has not
    /// been written, so it keeps the stamps it arrived with. Resetting them
    /// would report every component of an entity as changed the moment it gained
    /// an unrelated one, which is exactly the false positive change detection
    /// exists to avoid.
    ///
    /// Out of range is a no-op, matching the rest of the indexed API.
    pub fn set_ticks(&mut self, index: usize, ticks: ElementTicks) {
        if index >= self.len {
            return;
        }

        self.added[index] = ticks.added;
        self.changed[index].set(ticks.changed);
    }

    /// Stamp element `index` as written at `tick`.
    ///
    /// Takes `&self` because that is what a mutable query holds — see the note
    /// on the `changed` field. Out of range is a no-op.
    pub fn mark_changed(&self, index: usize, tick: Tick) {
        if let Some(changed) = self.changed.get(index) {
            changed.set(tick);
        }
    }

    /// A pointer to element zero's added-stamp.
    ///
    /// What a query resolves once per archetype and then strides over, exactly
    /// as it does with [`as_ptr`](Self::as_ptr).
    pub fn added_ticks_ptr(&self) -> *const Tick {
        self.added.as_ptr()
    }

    /// A pointer to element zero's changed-stamp.
    pub fn changed_ticks_ptr(&self) -> *const Cell<Tick> {
        self.changed.as_ptr()
    }

    /// Check invariant 5 — that the stamps have not drifted from the elements.
    ///
    /// Debug-only. Drift here does not crash: it silently reports the wrong
    /// element as changed, or panics much later on an index that used to exist.
    #[cfg(debug_assertions)]
    pub fn assert_consistent(&self) {
        assert_eq!(
            self.added.len(),
            self.len,
            "added stamps drifted from the elements"
        );
        assert_eq!(
            self.changed.len(),
            self.len,
            "changed stamps drifted from the elements"
        );
    }

    /// Append both stamps for a new element.
    fn push_ticks(&mut self, ticks: ElementTicks) {
        self.added.push(ticks.added);
        self.changed.push(Cell::new(ticks.changed));
    }

    /// A pointer to element zero.
    ///
    /// What a query resolves once per archetype and then strides over, so that
    /// the per-row cost is one add rather than a binary search through the
    /// signature. Dangling but aligned for an empty column, which is never
    /// indexed because iteration is bounded by [`len`](Self::len).
    pub fn as_ptr(&self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// A pointer to element `index`, or `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<*const u8> {
        (index < self.len).then(|| {
            // SAFETY: bounds were just checked, and elements below `len` are
            // initialized by invariant 2.
            unsafe { self.element_ptr(index).cast_const() }
        })
    }

    /// A mutable pointer to element `index`, or `None` if out of bounds.
    pub fn get_mut(&mut self, index: usize) -> Option<*mut u8> {
        (index < self.len).then(|| {
            // SAFETY: as `get`.
            unsafe { self.element_ptr(index) }
        })
    }

    /// Remove element `index`, dropping it, and move the last element into its
    /// place.
    ///
    /// Swap-remove rather than shift: an archetype's rows are identified by
    /// position, and shifting would renumber every entity after the hole. The
    /// caller is responsible for updating the entity that was moved, which is
    /// why [`swap_remove`](Self::swap_remove) is paired with the archetype
    /// knowing which entity now occupies `index`.
    ///
    /// Returns whether anything was removed.
    pub fn swap_remove(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }

        if let Some(drop_in_place) = self.drop_in_place {
            // SAFETY: `index` is in bounds and its element is initialized, and
            // `drop_in_place` came from the `TypeInfo` describing this exact
            // type. It runs at most once because the element is immediately
            // overwritten or the length is reduced past it.
            unsafe { drop_in_place(self.element_ptr(index)) };
        }

        self.move_last_into(index);

        true
    }

    /// Remove element `index` without dropping it, copying it to `out`.
    ///
    /// The archetype migration path: a component moving to a different
    /// archetype must not be destroyed, only relocated.
    ///
    /// Returns the stamps the element carried, so the destination can keep them
    /// — relocating is not writing. `None` if `index` was out of bounds.
    ///
    /// # Safety
    ///
    /// `out` must point at writable, properly aligned space for one element of
    /// this column's type, and the value written there becomes the caller's to
    /// drop.
    pub unsafe fn swap_remove_to(&mut self, index: usize, out: *mut u8) -> Option<ElementTicks> {
        if index >= self.len {
            return None;
        }

        let ticks = self.ticks(index).expect("the bounds were just checked");

        // SAFETY: `index` is in bounds and initialized; `out` is valid for one
        // element by the caller's guarantee; the column's allocation and `out`
        // are distinct, since `out` belongs to another archetype's column or to
        // caller stack space.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.element_ptr(index).cast_const(),
                out,
                self.layout.size(),
            );
        }

        self.move_last_into(index);

        Some(ticks)
    }

    /// Overwrite element `index`, dropping the value it held.
    ///
    /// Assigning to an entity that already has this component: no table changes,
    /// so none of the migration machinery runs, but the old value is still a
    /// value and has to be destroyed.
    ///
    /// Stamps the element as changed at `tick`. The added-stamp is left alone —
    /// overwriting a component is not gaining one.
    ///
    /// # Safety
    ///
    /// `value` must point at an initialized, properly aligned value of exactly
    /// this column's component type, must be treated as moved-from afterward,
    /// and must not point into this column — the write is a
    /// `copy_nonoverlapping`, and self-assignment through it is undefined.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds. Unlike [`get_mut`](Self::get_mut) this
    /// cannot report failure by returning nothing, because failing would mean
    /// silently leaking the caller's value.
    pub unsafe fn replace(&mut self, index: usize, value: *const u8, tick: Tick) {
        assert!(
            index < self.len,
            "replace index {index} is out of bounds for a column of {} elements",
            self.len
        );

        // SAFETY: the bounds check above, and elements below `len` are
        // initialized by invariant 2.
        let slot = unsafe { self.element_ptr(index) };

        if let Some(drop_in_place) = self.drop_in_place {
            // SAFETY: the slot holds an initialized value of this column's type,
            // and it is overwritten immediately below so this runs once.
            unsafe { drop_in_place(slot) };
        }

        // SAFETY: the slot is now uninitialized space for one element, and the
        // caller guarantees `value` is a distinct, initialized value of the
        // right type.
        unsafe { std::ptr::copy_nonoverlapping(value, slot, self.layout.size()) };

        self.changed[index].set(tick);
    }

    /// Every element as raw bytes.
    ///
    /// This is what `docs/DESIGN.md` §2.3's columnar boundary hands to a guest
    /// module: one contiguous run the guest iterates natively, rather than a
    /// call per entity.
    ///
    /// Returns `None` for a column that is not [`Transfer::Blittable`]. That is
    /// the enforcement point for the whole design — a column of `String` holds
    /// pointers into the host heap, and those bytes mean nothing inside a
    /// guest's linear memory. Checking here rather than trusting the caller is
    /// deliberate: the caller is often the module loader, working from a guest's
    /// own declaration of what it wants.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if !self.transfer.is_blittable() {
            return None;
        }

        // SAFETY: elements `0..len` are initialized and contiguous by
        // invariants 1 and 2, and blittable types contain no padding that would
        // make reading them as bytes undefined. A zero-sized type yields a
        // zero-length slice from a dangling-but-aligned pointer, which is
        // valid.
        Some(unsafe {
            std::slice::from_raw_parts(self.data.as_ptr(), self.len * self.layout.size())
        })
    }

    /// Every element as mutable raw bytes.
    ///
    /// The write-back half of the columnar boundary: a guest system that
    /// mutated its slice writes through this.
    ///
    /// Returns `None` for a non-blittable column, as [`as_bytes`](Self::as_bytes).
    pub fn as_bytes_mut(&mut self) -> Option<&mut [u8]> {
        if !self.transfer.is_blittable() {
            return None;
        }

        // SAFETY: as `as_bytes`, and `&mut self` guarantees exclusive access.
        // Blittable implies no destructor, so a caller overwriting these bytes
        // cannot leak anything.
        Some(unsafe {
            std::slice::from_raw_parts_mut(self.data.as_ptr(), self.len * self.layout.size())
        })
    }

    /// Drop every element, keeping the allocation.
    pub fn clear(&mut self) {
        if let Some(drop_in_place) = self.drop_in_place {
            for index in 0..self.len {
                // SAFETY: every index below `len` is initialized, and each is
                // dropped exactly once because `len` is zeroed afterward.
                unsafe { drop_in_place(self.element_ptr(index)) };
            }
        }

        self.len = 0;
        self.added.clear();
        self.changed.clear();
    }

    /// A pointer to element `index`, without a bounds check.
    ///
    /// # Safety
    ///
    /// `index` must be within the allocation — at most `capacity`. The element
    /// need not be initialized; `push` uses this to find where to write.
    unsafe fn element_ptr(&self, index: usize) -> *mut u8 {
        // SAFETY: the caller guarantees the offset is within the allocation, so
        // the result is in bounds or one past the end. For a zero-sized type
        // the offset is zero and the dangling pointer is returned unchanged,
        // which is what Rust expects of a pointer to a zero-sized value.
        unsafe { self.data.as_ptr().add(index * self.layout.size()) }
    }

    /// Move the last element into `index`, shrinking by one.
    ///
    /// The element previously at `index` must already have been dropped or moved
    /// out; this only relocates the tail over it — and its stamps with it, which
    /// is what keeps invariant 5 true through a swap-remove.
    fn move_last_into(&mut self, index: usize) {
        let last = self.len - 1;

        self.added.swap_remove(index);
        self.changed.swap_remove(index);

        if index != last && self.layout.size() != 0 {
            // SAFETY: both indices are within the allocation and `last` is
            // initialized. The regions are within one allocation and may not
            // overlap because `index != last` and elements do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.element_ptr(last).cast_const(),
                    self.element_ptr(index),
                    self.layout.size(),
                );
            }
        }

        self.len = last;
    }

    /// Make room for one more element.
    fn reserve_one(&mut self) {
        if self.len < self.capacity {
            return;
        }

        // A zero-sized type has `usize::MAX` capacity and can never reach this.
        debug_assert_ne!(self.layout.size(), 0, "a zero-sized column cannot grow");

        let new_capacity = if self.capacity == 0 {
            INITIAL_CAPACITY
        } else {
            // Doubling, which amortizes push to O(1). Overflow here would mean
            // an archetype holding 2^63 entities, but checking costs nothing on
            // a path that runs once per doubling.
            self.capacity
                .checked_mul(2)
                .expect("component column capacity overflowed")
        };

        let new_layout = Layout::from_size_align(
            self.layout
                .size()
                .checked_mul(new_capacity)
                .expect("component column size overflowed"),
            self.layout.align(),
        )
        .expect("a valid element layout scales to a valid array layout");

        // SAFETY: `new_layout` has non-zero size, since the element size is
        // non-zero and the capacity is at least `INITIAL_CAPACITY`.
        let new_data = unsafe { alloc(new_layout) };
        let Some(new_data) = NonNull::new(new_data) else {
            handle_alloc_error(new_layout)
        };

        if self.capacity > 0 {
            // SAFETY: the old allocation holds `len` initialized elements and
            // the new one is strictly larger; the two are distinct allocations.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data.as_ptr().cast_const(),
                    new_data.as_ptr(),
                    self.len * self.layout.size(),
                );
            }

            // SAFETY: `self.data` came from `alloc` with exactly this layout,
            // and every element has been relocated so nothing is left to drop.
            unsafe { dealloc(self.data.as_ptr(), self.array_layout(self.capacity)) };
        }

        self.data = new_data;
        self.capacity = new_capacity;
    }

    /// The layout of the whole allocation at a given capacity.
    fn array_layout(&self, capacity: usize) -> Layout {
        Layout::from_size_align(self.layout.size() * capacity, self.layout.align())
            .expect("this layout was already used to allocate")
    }
}

impl Drop for Column {
    fn drop(&mut self) {
        self.clear();

        if self.capacity > 0 && self.layout.size() != 0 {
            // SAFETY: `clear` dropped every element, `data` came from `alloc`
            // with this exact layout, and this runs once.
            unsafe { dealloc(self.data.as_ptr(), self.array_layout(self.capacity)) };
        }
    }
}

impl std::fmt::Debug for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Column")
            .field("type_id", &self.type_id)
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .field("element_size", &self.layout.size())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_reflect::{Reflect, TypeKind};
    use std::rc::Rc;

    /// Push a typed value, handing the column ownership.
    fn push<T>(column: &mut Column, value: T) {
        push_at(column, value, Tick::new(1));
    }

    /// Push a typed value stamped at `tick`.
    fn push_at<T>(column: &mut Column, value: T, tick: Tick) {
        let value = std::mem::ManuallyDrop::new(value);

        // SAFETY: `value` is an initialized `T`, the column was built from
        // `T`'s own `TypeInfo`, and `ManuallyDrop` stops it being dropped here
        // after the column takes ownership.
        unsafe { column.push(std::ptr::from_ref(&*value).cast::<u8>(), tick) };
    }

    /// Read element `index` as a `T`.
    fn read<T: Copy>(column: &Column, index: usize) -> Option<T> {
        // SAFETY: the column was built from `T`'s `TypeInfo`, and `get` returns
        // a pointer to an initialized element or `None`.
        column
            .get(index)
            .map(|pointer| unsafe { *pointer.cast::<T>() })
    }

    fn column_of<T: Reflect>() -> Column {
        Column::new(&T::type_info())
    }

    #[test]
    fn a_fresh_column_is_empty_and_allocates_nothing() {
        let column = column_of::<u32>();

        assert!(column.is_empty());
        assert_eq!(column.len(), 0);
        assert_eq!(column.get(0), None);
    }

    #[test]
    fn pushed_values_come_back_in_order() {
        let mut column = column_of::<u32>();

        for value in 0..64_u32 {
            push(&mut column, value);
        }

        assert_eq!(column.len(), 64);
        for value in 0..64_u32 {
            assert_eq!(read::<u32>(&column, value as usize), Some(value));
        }
    }

    #[test]
    fn growth_preserves_every_element() {
        // The reallocation path: doubling from 4 means several moves, and a
        // mistake shows up as garbage in the earliest elements.
        let mut column = column_of::<u64>();

        for value in 0..1000_u64 {
            push(&mut column, value.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }

        for value in 0..1000_u64 {
            assert_eq!(
                read::<u64>(&column, value as usize),
                Some(value.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
                "element {value} survived reallocation"
            );
        }
    }

    #[test]
    fn out_of_bounds_access_is_none_rather_than_garbage() {
        let mut column = column_of::<u32>();
        push(&mut column, 7_u32);

        assert_eq!(read::<u32>(&column, 0), Some(7));
        assert_eq!(column.get(1), None);
        assert_eq!(column.get(usize::MAX), None);
    }

    #[test]
    fn swap_remove_moves_the_last_element_into_the_hole() {
        // The behaviour the archetype layer depends on: removing a row does not
        // renumber every row after it, but it does move exactly one.
        let mut column = column_of::<u32>();
        for value in [10_u32, 20, 30, 40] {
            push(&mut column, value);
        }

        assert!(column.swap_remove(1));

        assert_eq!(column.len(), 3);
        assert_eq!(read::<u32>(&column, 0), Some(10));
        assert_eq!(read::<u32>(&column, 1), Some(40), "the last moved into 1");
        assert_eq!(read::<u32>(&column, 2), Some(30));
    }

    #[test]
    fn removing_the_last_element_needs_no_move() {
        let mut column = column_of::<u32>();
        for value in [10_u32, 20] {
            push(&mut column, value);
        }

        assert!(column.swap_remove(1));

        assert_eq!(column.len(), 1);
        assert_eq!(read::<u32>(&column, 0), Some(10));
    }

    #[test]
    fn removing_out_of_bounds_reports_false_rather_than_panicking() {
        let mut column = column_of::<u32>();

        assert!(!column.swap_remove(0));
        assert_eq!(column.len(), 0);
    }

    #[test]
    fn swap_remove_to_relocates_without_dropping() {
        // The archetype migration path. If this dropped, a component moving
        // between archetypes would be destroyed and the destination would hold
        // a use-after-free.
        let witness = Rc::new(());
        let mut column = column_of::<Witness>();
        push(&mut column, Witness(Rc::clone(&witness)));

        assert_eq!(Rc::strong_count(&witness), 2);

        let mut moved = std::mem::MaybeUninit::<Witness>::uninit();
        // SAFETY: `moved` is properly aligned space for one `Witness`, and the
        // column holds `Witness` values.
        let removed = unsafe { column.swap_remove_to(0, moved.as_mut_ptr().cast::<u8>()) };

        assert!(removed.is_some(), "the element's stamps come back with it");
        assert_eq!(column.len(), 0);
        assert_eq!(
            Rc::strong_count(&witness),
            2,
            "the value must have been moved, not dropped"
        );

        // SAFETY: `swap_remove_to` returned `Some`, so `moved` is initialized.
        let moved = unsafe { moved.assume_init() };
        drop(moved);

        assert_eq!(Rc::strong_count(&witness), 1);
    }

    #[test]
    fn dropping_a_column_runs_every_destructor() {
        // The leak check. A column of a type with a destructor must run every
        // one of them, including after growth has relocated the elements.
        let witness = Rc::new(());

        {
            let mut column = column_of::<Witness>();
            for _ in 0..100 {
                push(&mut column, Witness(Rc::clone(&witness)));
            }

            assert_eq!(Rc::strong_count(&witness), 101);
        }

        assert_eq!(
            Rc::strong_count(&witness),
            1,
            "every element should have been dropped"
        );
    }

    #[test]
    fn swap_remove_drops_the_element_it_removes() {
        let witness = Rc::new(());
        let mut column = column_of::<Witness>();

        for _ in 0..3 {
            push(&mut column, Witness(Rc::clone(&witness)));
        }
        assert_eq!(Rc::strong_count(&witness), 4);

        column.swap_remove(0);

        assert_eq!(Rc::strong_count(&witness), 3, "exactly one was dropped");
    }

    #[test]
    fn clear_drops_everything_and_keeps_the_column_usable() {
        let witness = Rc::new(());
        let mut column = column_of::<Witness>();

        for _ in 0..8 {
            push(&mut column, Witness(Rc::clone(&witness)));
        }

        column.clear();

        assert_eq!(Rc::strong_count(&witness), 1);
        assert!(column.is_empty());

        push(&mut column, Witness(Rc::clone(&witness)));
        assert_eq!(column.len(), 1);
    }

    #[test]
    fn a_zero_sized_component_stores_a_count_and_allocates_nothing() {
        // Marker components — `Player`, `Static`, `Hidden` — are the common
        // case, and pointer arithmetic over a zero stride would be undefined.
        let mut column = Column::new(&Marker::type_info());

        assert_eq!(column.element_layout().size(), 0);

        for _ in 0..1000 {
            push(&mut column, Marker {});
        }

        assert_eq!(column.len(), 1000);
        assert!(column.get(999).is_some());
        assert!(column.get(1000).is_none());

        assert!(column.swap_remove(0));
        assert_eq!(column.len(), 999);

        column.clear();
        assert!(column.is_empty());
    }

    #[test]
    fn a_blittable_column_exposes_its_bytes() {
        // The columnar WASM boundary. One contiguous run, no gather step.
        let mut column = column_of::<u32>();
        for value in [1_u32, 2, 3] {
            push(&mut column, value);
        }

        let bytes = column.as_bytes().expect("u32 is blittable");

        assert_eq!(bytes.len(), 12);
        assert_eq!(&bytes[0..4], &1_u32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &3_u32.to_ne_bytes());
    }

    #[test]
    fn a_non_blittable_column_refuses_to_expose_its_bytes() {
        // The enforcement point for the whole `Transfer` design. A column of
        // `String` holds pointers into the host heap; handing those to a guest
        // as raw memory would be both meaningless and a disclosure of host
        // addresses.
        let mut column = column_of::<String>();
        push(&mut column, String::from("not for the guest"));

        assert!(column.as_bytes().is_none());
        assert!(column.as_bytes_mut().is_none());
        assert!(!column.is_blittable());
    }

    #[test]
    fn writes_through_the_byte_view_are_visible_as_values() {
        // The write-back half: a guest system mutates its slice, and the host
        // sees typed values change.
        let mut column = column_of::<u32>();
        push(&mut column, 1_u32);

        let bytes = column.as_bytes_mut().expect("u32 is blittable");
        bytes[0..4].copy_from_slice(&99_u32.to_ne_bytes());

        assert_eq!(read::<u32>(&column, 0), Some(99));
    }

    #[test]
    fn alignment_is_respected_for_an_over_aligned_type() {
        // A 16-byte-aligned component is normal — anything holding a SIMD
        // vector. An allocation aligned only to 8 would be undefined behaviour
        // to write through, and the failure is silent on x86.
        let mut column = Column::new(&Aligned::type_info());

        for value in 0..64_u64 {
            push(&mut column, Aligned { value });
        }

        for index in 0..64_usize {
            let pointer = column.get(index).expect("in bounds");

            assert_eq!(
                pointer as usize % 16,
                0,
                "element {index} is misaligned at {pointer:p}"
            );
        }
    }

    #[test]
    fn the_column_reports_the_type_it_was_built_for() {
        let column = column_of::<u32>();

        assert_eq!(column.type_id(), u32::type_id());
        assert_eq!(column.element_layout(), Layout::new::<u32>());
    }

    /// Holds a clone so tests can observe drops without a global counter.
    struct Witness(#[expect(dead_code, reason = "held to keep the Rc alive")] Rc<()>);

    // SAFETY: the layout is `Witness`'s own and the destructor drops a
    // `Witness` in place. Hand-written rather than derived because the derive
    // requires every field to be `Reflect`, and `Rc<()>` deliberately is not.
    unsafe impl Reflect for Witness {
        const PATH: &'static str = "test::Witness";
        const TRANSFER: Transfer = Transfer::Owning;

        fn type_info() -> TypeInfo {
            // SAFETY: as above.
            unsafe {
                TypeInfo::with_drop(
                    Self::PATH,
                    Layout::new::<Self>(),
                    Self::TRANSFER,
                    TypeKind::Opaque,
                    |pointer| std::ptr::drop_in_place(pointer.cast::<Self>()),
                )
            }
        }
    }

    /// A zero-sized marker component.
    ///
    /// Braces rather than `struct Marker;` because the derive rejects unit
    /// structs, and an empty named-field struct is the same thing with a shape
    /// the reflection model can describe.
    #[derive(slop_reflect::Reflect)]
    #[repr(C)]
    struct Marker {}

    /// Over-aligned, as anything holding a SIMD vector would be.
    #[derive(slop_reflect::Reflect)]
    #[repr(C, align(16))]
    struct Aligned {
        value: u64,
    }
}
