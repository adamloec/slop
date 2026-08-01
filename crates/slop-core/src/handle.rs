//! Generational handles — `DESIGN.md` §2.6.
//!
//! A [`Handle<T>`] is a typed, `Copy`, 8-byte reference to something owned
//! elsewhere: an index plus the generation the slot held when the handle was
//! issued. Looking one up compares generations, so a handle to a freed slot
//! resolves to `None` rather than silently aliasing whatever took its place.
//!
//! This is the engine's answer to graph-shaped data. `Rc<RefCell<_>>` would
//! model the same relationships while costing refcount traffic, runtime borrow
//! panics, and a serialization path that has to rebuild pointers. Handles are
//! plain data: they serialize as-is, cross the `DESIGN.md` §2.3 WASM boundary as
//! integers, and never keep anything alive.
//!
//! The design decisions behind the specific layout are recorded in `PLAN.md`
//! §4.1-C.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::num::NonZeroU32;

/// A typed reference to a slot in a [`SlotMap`](crate::SlotMap) or
/// [`HandleAllocator`](crate::HandleAllocator).
///
/// Eight bytes, and `Option<Handle<T>>` is also eight — the generation is
/// [`NonZeroU32`], so the `None` case costs nothing.
///
/// The `T` is a compile-time tag only. No `T` is ever stored, which is why a
/// `Handle<T>` is `Copy`, `Send`, and `Sync` no matter what `T` is.
pub struct Handle<T> {
    index: u32,
    generation: NonZeroU32,
    /// `fn() -> T` rather than `T`: it makes the marker unconditionally `Send`,
    /// `Sync`, and `Copy`, and leaves `T` covariant. A bare `PhantomData<T>`
    /// would leak `T`'s auto-traits onto the handle, so `Handle<*const u8>`
    /// would stop being `Send` despite containing nothing but integers.
    _tag: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// Issued only by the containers in this crate. A handle built from
    /// arbitrary parts would claim a provenance it does not have; crossing an
    /// ABI boundary goes through [`Handle::from_raw`] instead, which is honest
    /// about being unvalidated.
    pub(crate) const fn new(index: u32, generation: NonZeroU32) -> Self {
        Self {
            index,
            generation,
            _tag: PhantomData,
        }
    }

    /// Slot this handle points at. Only meaningful to the container that issued
    /// it.
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Generation the slot held when this handle was issued.
    pub const fn generation(self) -> NonZeroU32 {
        self.generation
    }

    /// Erase the type tag for transport across an ABI boundary (`DESIGN.md`
    /// §2.3), where handles are opaque integers.
    pub const fn to_raw(self) -> RawHandle {
        RawHandle(((self.generation.get() as u64) << 32) | self.index as u64)
    }

    /// Restore a typed handle from its erased form.
    ///
    /// Returns `None` if the generation field is zero, which no issued handle
    /// ever has. That check rejects null and obviously-garbage values, and
    /// nothing more: this cannot tell whether the handle came from *this*
    /// container, or whether `T` matches what it originally pointed at. Guest
    /// modules are untrusted (§2.3), so treat the result as a claim to be
    /// validated by lookup, not as proof of anything.
    pub const fn from_raw(raw: RawHandle) -> Option<Self> {
        match NonZeroU32::new((raw.0 >> 32) as u32) {
            Some(generation) => Some(Self::new(raw.0 as u32, generation)),
            None => None,
        }
    }
}

/// A [`Handle`] with its type tag erased, for crossing ABI boundaries.
///
/// The bit layout — generation in the high 32 bits, index in the low 32 — is
/// stable and part of the `slop-abi` contract once that crate exists.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct RawHandle(u64);

impl RawHandle {
    /// The wire representation.
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Reconstruct from a wire value. Validity is checked by
    /// [`Handle::from_raw`], not here.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

// The impls below are written by hand rather than derived. `derive` would
// generate `impl<T: Clone> Clone for Handle<T>`, bounding the handle on a type
// it does not contain — so `Handle<NotClone>` would not be `Copy`.

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_raw().hash(state);
    }
}

impl<T> PartialOrd for Handle<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Handle<T> {
    /// Ordered by slot, then generation. Sorting by handle is how iteration
    /// order is made stable for the deterministic headless mode in `DESIGN.md`
    /// §5, so the ordering must not depend on anything but these two fields.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index
            .cmp(&other.index)
            .then(self.generation.cmp(&other.generation))
    }
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Carries the type name because a bare `Handle(3v1)` in a log or a panic
        // message is not diagnosable — see CONVENTIONS.md §13.
        write!(
            f,
            "Handle<{}>({}v{})",
            short_type_name::<T>(),
            self.index,
            self.generation
        )
    }
}

/// `std::any::type_name` returns a fully qualified path; the last segment is
/// what makes a log line readable.
fn short_type_name<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    match full.rfind("::") {
        Some(pos) => &full[pos + 2..],
        None => full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN_1: NonZeroU32 = NonZeroU32::new(1).unwrap();
    const GEN_2: NonZeroU32 = NonZeroU32::new(2).unwrap();

    struct Payload;

    fn assert_copy<T: Copy>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn handle_is_eight_bytes() {
        assert_eq!(size_of::<Handle<Payload>>(), 8);
    }

    #[test]
    fn optional_handle_costs_nothing_extra() {
        // Relies on the NonZeroU32 niche. If this regresses, every Option<Handle>
        // field in the engine silently doubles.
        assert_eq!(
            size_of::<Option<Handle<Payload>>>(),
            size_of::<Handle<Payload>>()
        );
    }

    #[test]
    fn handle_traits_do_not_depend_on_payload_type() {
        // `*const ()` is neither Send nor Sync and is not Copy-friendly as a
        // payload; the handle must be all three regardless, because it stores no T.
        assert_copy::<Handle<*const ()>>();
        assert_send_sync::<Handle<*const ()>>();
    }

    #[test]
    fn raw_round_trip_preserves_index_and_generation() {
        let handle = Handle::<Payload>::new(0xDEAD_BEEF, GEN_2);
        let restored = Handle::<Payload>::from_raw(handle.to_raw()).expect("generation is nonzero");

        assert_eq!(restored, handle);
        assert_eq!(restored.index(), 0xDEAD_BEEF);
        assert_eq!(restored.generation(), GEN_2);
    }

    #[test]
    fn raw_survives_a_trip_through_its_wire_form() {
        let handle = Handle::<Payload>::new(7, GEN_1);
        let bits = handle.to_raw().to_bits();

        let restored = Handle::<Payload>::from_raw(RawHandle::from_bits(bits));

        assert_eq!(restored, Some(handle));
    }

    #[test]
    fn from_raw_rejects_zero_generation() {
        // Zero is the one value no issued handle can carry, so it is what a
        // null or zeroed buffer from a guest module looks like.
        assert!(Handle::<Payload>::from_raw(RawHandle::from_bits(0)).is_none());
        assert!(Handle::<Payload>::from_raw(RawHandle::from_bits(0xFFFF_FFFF)).is_none());
    }

    #[test]
    fn same_slot_different_generation_are_not_equal() {
        let old = Handle::<Payload>::new(4, GEN_1);
        let new = Handle::<Payload>::new(4, GEN_2);

        assert_ne!(old, new);
    }

    #[test]
    fn ordering_is_by_slot_then_generation() {
        let mut handles = [
            Handle::<Payload>::new(2, GEN_1),
            Handle::<Payload>::new(1, GEN_2),
            Handle::<Payload>::new(1, GEN_1),
        ];
        handles.sort();

        assert_eq!(
            handles,
            [
                Handle::<Payload>::new(1, GEN_1),
                Handle::<Payload>::new(1, GEN_2),
                Handle::<Payload>::new(2, GEN_1),
            ]
        );
    }

    #[test]
    fn debug_output_names_the_payload_type() {
        let handle = Handle::<Payload>::new(3, GEN_1);

        assert_eq!(format!("{handle:?}"), "Handle<Payload>(3v1)");
    }
}
