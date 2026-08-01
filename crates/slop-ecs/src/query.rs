//! Iterating entities by the components they hold.
//!
//! ```ignore
//! for (position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
//!     position.x += velocity.dx;
//! }
//! ```
//!
//! # Where the archetype decision pays off
//!
//! A query resolves each matching archetype's columns to base pointers **once**,
//! then strides. The per-row cost is one pointer add per component — no lookup,
//! no indirection, no bounds check — over memory that is already contiguous.
//! `docs/DESIGN.md` §2.10 chose archetype storage for exactly this, and this
//! module is where the choice becomes visible: sparse-set storage would hop
//! between arrays through an index on every row.
//!
//! Type erasure is paid for once per archetype too. [`QueryData::state`]
//! resolves a `TypeId` to a column and casts it; after that the iteration is as
//! typed as a slice walk, which is the answer to the risk that a data-driven
//! core makes the ordinary path unpleasant.
//!
//! # Aliasing
//!
//! Two rules, enforced differently:
//!
//! - **A read-only query cannot request `&mut`.** Enforced by the type system:
//!   [`World::query`](crate::World::query) takes `&self` and requires
//!   [`ReadOnlyQueryData`], which `&mut T` does not implement.
//! - **One query cannot name the same component twice** if either is mutable.
//!   `(&mut Position, &Position)` would hand out an aliasing pair. Checked when
//!   the query is built, and a panic rather than an error: it is a property of
//!   the code as written, always wrong, and caught the first time the line runs.

use std::marker::PhantomData;

use slop_reflect::{Reflect, TypeId};

use crate::{Archetype, Entity};

/// What a query wants from one component type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// The component type.
    pub type_id: TypeId,
    /// Whether it is requested mutably.
    pub mutable: bool,
}

/// Something a query can yield per entity.
///
/// Implemented for `&T`, `&mut T`, [`Entity`], and tuples of those.
///
/// # Safety
///
/// [`state`](Self::state) must return `Some` only when the archetype genuinely
/// holds every component [`collect_access`](Self::collect_access) names, and the
/// pointers it captures must be that archetype's own columns. [`get`](Self::get)
/// then trusts both, and dereferences without checking.
pub unsafe trait QueryData {
    /// What one entity yields.
    type Item<'w>;

    /// Column pointers resolved for one archetype.
    type State: Copy;

    /// Record every component this reads or writes.
    fn collect_access(out: &mut Vec<Access>);

    /// Resolve `archetype`'s columns, or `None` if it does not match.
    fn state(archetype: &Archetype) -> Option<Self::State>;

    /// Read row `row`.
    ///
    /// # Safety
    ///
    /// `state` must have come from [`state`](Self::state) for an archetype that
    /// is still alive and unmodified, and `row` must be below that archetype's
    /// length. The returned lifetime is chosen by the caller, so it must not
    /// outlive the borrow the state was derived from.
    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w>;
}

/// A [`QueryData`] that only reads.
///
/// The marker that lets [`World::query`](crate::World::query) take `&self`. It
/// is not implemented for `&mut T`, so a read-only query naming one fails to
/// compile rather than failing at runtime.
///
/// # Safety
///
/// Implementors must never hand out a `&mut` to component data.
pub unsafe trait ReadOnlyQueryData: QueryData {}

// SAFETY: `state` resolves the column for `T` and returns `None` when the
// archetype lacks it, so `get` only ever runs against a real column of `T`.
unsafe impl<T: Reflect> QueryData for &T {
    type Item<'w> = &'w T;
    type State = *const T;

    fn collect_access(out: &mut Vec<Access>) {
        out.push(Access {
            type_id: T::type_id(),
            mutable: false,
        });
    }

    fn state(archetype: &Archetype) -> Option<Self::State> {
        archetype
            .column(T::type_id())
            .map(|column| column.as_ptr().cast_const().cast::<T>())
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        // SAFETY: the caller guarantees `row` is in bounds for the archetype
        // this state came from, and that archetype's column holds initialized
        // `T` values at every row below its length.
        unsafe { &*state.add(row) }
    }
}

// SAFETY: `&T` yields only shared references.
unsafe impl<T: Reflect> ReadOnlyQueryData for &T {}

// SAFETY: as the shared impl; the exclusivity of the yielded reference rests on
// the conflict check in `QueryPlan::new` and on `World::query_mut` taking
// `&mut self`.
unsafe impl<T: Reflect> QueryData for &mut T {
    type Item<'w> = &'w mut T;
    type State = *mut T;

    fn collect_access(out: &mut Vec<Access>) {
        out.push(Access {
            type_id: T::type_id(),
            mutable: true,
        });
    }

    fn state(archetype: &Archetype) -> Option<Self::State> {
        archetype
            .column(T::type_id())
            .map(|column| column.as_ptr().cast::<T>())
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        // SAFETY: as the shared impl, and no other live reference addresses this
        // element — the query holds `&mut World` and no component type appears
        // twice in one query.
        unsafe { &mut *state.add(row) }
    }
}

// SAFETY: an entity id is a `Copy` value read from the archetype's own roster;
// it names no component and so constrains nothing.
unsafe impl QueryData for Entity {
    type Item<'w> = Entity;
    type State = *const Entity;

    fn collect_access(_out: &mut Vec<Access>) {}

    fn state(archetype: &Archetype) -> Option<Self::State> {
        Some(archetype.entities().as_ptr())
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        // SAFETY: the roster is exactly as long as the archetype, and the
        // caller guarantees `row` is within it.
        unsafe { *state.add(row) }
    }
}

// SAFETY: yields a `Copy` id, never a reference to component data.
unsafe impl ReadOnlyQueryData for Entity {}

/// Implement [`QueryData`] for a tuple.
macro_rules! tuple_query {
    ($($name:ident),+) => {
        // SAFETY: `state` returns `Some` only when every member matched, so
        // `get` runs against an archetype holding all of them.
        #[allow(non_snake_case, reason = "the macro names bindings after the type parameters")]
        unsafe impl<$($name: QueryData),+> QueryData for ($($name,)+) {
            type Item<'w> = ($($name::Item<'w>,)+);
            type State = ($($name::State,)+);

            fn collect_access(out: &mut Vec<Access>) {
                $($name::collect_access(out);)+
            }

            fn state(archetype: &Archetype) -> Option<Self::State> {
                Some(($($name::state(archetype)?,)+))
            }

            unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
                let ($($name,)+) = state;

                // SAFETY: the caller's obligations are passed to each member
                // unchanged.
                unsafe { ($($name::get($name, row),)+) }
            }
        }

        // SAFETY: a tuple yields only what its members yield.
        unsafe impl<$($name: ReadOnlyQueryData),+> ReadOnlyQueryData for ($($name,)+) {}
    };
}

tuple_query!(A);
tuple_query!(A, B);
tuple_query!(A, B, C);
tuple_query!(A, B, C, D);
tuple_query!(A, B, C, D, E);
tuple_query!(A, B, C, D, E, F);
tuple_query!(A, B, C, D, E, F, G);
tuple_query!(A, B, C, D, E, F, G, H);

/// Iterates every entity matching `D`.
///
/// Yields nothing for an archetype it does not match, and walks the rest row by
/// row over contiguous memory.
pub struct Query<'w, D: QueryData> {
    archetypes: &'w [Archetype],
    /// The next archetype to consider.
    next_archetype: usize,
    /// Resolved columns for the archetype being walked.
    state: Option<D::State>,
    row: usize,
    rows: usize,
    _data: PhantomData<fn() -> D>,
}

impl<'w, D: QueryData> Query<'w, D> {
    /// Build a query over `archetypes`.
    ///
    /// # Panics
    ///
    /// If the same component type is named twice and either is mutable. That
    /// would hand out two references to one element, one of them exclusive —
    /// undefined behaviour, and a property of the code as written rather than of
    /// the data, so it fails the first time the line runs.
    pub(crate) fn new(archetypes: &'w [Archetype]) -> Self {
        assert_no_conflicts::<D>();

        Self {
            archetypes,
            next_archetype: 0,
            state: None,
            row: 0,
            rows: 0,
            _data: PhantomData,
        }
    }

    /// Move to the next archetype that matches, if any.
    fn advance_archetype(&mut self) -> bool {
        while let Some(archetype) = self.archetypes.get(self.next_archetype) {
            self.next_archetype += 1;

            // An empty archetype matches but yields nothing; skipping it here
            // saves a state resolve and keeps `next` from recursing.
            if archetype.is_empty() {
                continue;
            }

            if let Some(state) = D::state(archetype) {
                self.state = Some(state);
                self.row = 0;
                self.rows = archetype.len();

                return true;
            }
        }

        false
    }
}

impl<'w, D: QueryData> Iterator for Query<'w, D> {
    type Item = D::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(state) = self.state
                && self.row < self.rows
            {
                let row = self.row;
                self.row += 1;

                // SAFETY: `state` was resolved from the archetype now being
                // walked, `row` is below its length, and the yielded lifetime is
                // `'w` — the borrow of the archetype slice the query holds. No
                // structural change can happen during iteration, because that
                // needs `&mut World` and the query borrows it.
                return Some(unsafe { D::get(state, row) });
            }

            if !self.advance_archetype() {
                return None;
            }
        }
    }
}

impl<D: QueryData> std::fmt::Debug for Query<'_, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Query")
            .field("archetypes", &self.archetypes.len())
            .field("at", &self.next_archetype)
            .finish_non_exhaustive()
    }
}

/// Panic if `D` names a component twice with either access mutable.
fn assert_no_conflicts<D: QueryData>() {
    let mut access = Vec::new();
    D::collect_access(&mut access);

    for (index, left) in access.iter().enumerate() {
        for right in &access[index + 1..] {
            if left.type_id == right.type_id && (left.mutable || right.mutable) {
                panic!(
                    "query names component {} twice with mutable access; \
                     that would alias one element",
                    left.type_id
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_access_is_recorded_without_mutability() {
        let mut access = Vec::new();
        <&u32 as QueryData>::collect_access(&mut access);

        assert_eq!(access.len(), 1);
        assert_eq!(access[0].type_id, u32::type_id());
        assert!(!access[0].mutable);
    }

    #[test]
    fn exclusive_access_is_recorded_as_mutable() {
        let mut access = Vec::new();
        <&mut u32 as QueryData>::collect_access(&mut access);

        assert!(access[0].mutable);
    }

    #[test]
    fn an_entity_constrains_nothing() {
        // `Entity` matches every archetype and names no component, so a query
        // of `(Entity,)` alone visits everything.
        let mut access = Vec::new();
        <Entity as QueryData>::collect_access(&mut access);

        assert!(access.is_empty());
    }

    #[test]
    fn a_tuple_collects_every_members_access() {
        let mut access = Vec::new();
        <(&u32, &mut f32, Entity) as QueryData>::collect_access(&mut access);

        assert_eq!(access.len(), 2, "Entity contributes nothing");
        assert!(!access[0].mutable);
        assert!(access[1].mutable);
    }

    #[test]
    fn reading_the_same_component_twice_is_allowed() {
        // Two shared references to one element are fine, so this must not be
        // rejected — an over-strict check would forbid legitimate queries.
        assert_no_conflicts::<(&u32, &u32)>();
    }

    #[test]
    #[should_panic(expected = "twice with mutable access")]
    fn writing_and_reading_the_same_component_is_rejected() {
        assert_no_conflicts::<(&mut u32, &u32)>();
    }

    #[test]
    #[should_panic(expected = "twice with mutable access")]
    fn writing_the_same_component_twice_is_rejected() {
        assert_no_conflicts::<(&mut u32, &mut u32)>();
    }

    #[test]
    fn distinct_components_never_conflict() {
        assert_no_conflicts::<(&mut u32, &mut f32, &u64, Entity)>();
    }
}
