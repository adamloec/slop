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
//!
//! # Filters narrow what is visited, without yielding anything
//!
//! ```ignore
//! for position in world.query::<&Position>().with::<Player>().without::<Frozen>() {
//!     // Every player that is not frozen, and no `()` in the tuple to unpack.
//! }
//! ```
//!
//! [`With`] and [`Without`] are [`QueryFilter`]s rather than [`QueryData`], and
//! the difference is exactly that a filter yields nothing. Modelling `With<T>` as
//! query data that happens to produce `()` would put a `()` in every tuple
//! pattern at every call site.
//!
//! A filter contributes **no [`Access`]**. `With<Player>` inspects whether an
//! archetype's signature holds `Player`; it never reads a `Player`, so a system
//! writing `Player` does not conflict with one filtering on it. What could
//! invalidate that — a structural change while the query runs — cannot happen,
//! because structural change needs `&mut World` and is deferred to a sync point
//! ([`CommandBuffer`](crate::CommandBuffer)).

//!
//! # Change detection rides on the same two traits
//!
//! [`Changed<T>`] and [`Added<T>`] are filters, and they are why [`QueryFilter`]
//! resolves per-archetype state and then answers **per row** — `With<Player>`
//! could have been a plain `fn(&Archetype) -> bool`, but "was this component
//! written since I last looked?" cannot. Both shapes share one trait so a filter
//! is one concept rather than two.
//!
//! `&mut T` yields [`Mut<T>`] rather than `&mut T` for the same reason: a stamp
//! has to be written when the component is reached mutably, and only a wrapper
//! can notice that. `position.x += 1.0` reads identically through `DerefMut`;
//! what changes is that a loop writing one row in a hundred marks one row, not a
//! hundred.

use std::marker::PhantomData;

use slop_reflect::{Reflect, TypeId};

use crate::{Archetype, Entity, Tick, Ticks};

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
    ///
    /// `ticks` is the window the query was built with; only `&mut T` uses it,
    /// to know what stamp to write when the component is reached.
    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State>;

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

    fn state(archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
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

/// Exclusive access to a component, which stamps it changed when reached.
///
/// What a `&mut T` query yields. `Deref` reads without stamping; `DerefMut`
/// stamps, so
///
/// ```ignore
/// for mut position in world.query_mut::<&mut Position>() {
///     if position.x < 0.0 {
///         position.x = 0.0;    // only these rows are marked changed
///     }
/// }
/// ```
///
/// marks the rows the branch was taken on rather than every row visited. That
/// precision is the entire value of change detection — a conditional write is
/// the common case, and the alternative marks everything the loop touched.
///
/// The cost is that a binding needs `mut` to be written through, and code
/// wanting a bare `&mut T` says [`into_inner`](Self::into_inner).
pub struct Mut<'w, T> {
    value: &'w mut T,
    changed: &'w std::cell::Cell<Tick>,
    this_run: Tick,
}

impl<'w, T> Mut<'w, T> {
    /// Take the reference out, stamping the component as changed.
    ///
    /// For handing a component to something that takes `&mut T`.
    pub fn into_inner(self) -> &'w mut T {
        self.changed.set(self.this_run);

        self.value
    }

    /// Mutate without stamping.
    ///
    /// For writes that are genuinely not a change — recomputing a cache into a
    /// field, or restoring a value that was already there. Reach for it rarely:
    /// a missed stamp is a system that silently stops seeing updates, which is
    /// far harder to notice than a spurious one.
    pub fn bypass_change_detection(&mut self) -> &mut T {
        self.value
    }

    /// Assign, and stamp only if the value actually differs.
    ///
    /// Returns whether it changed. The idiom for a system that recomputes a
    /// value every frame and writes the same answer most of the time.
    pub fn set_if_neq(&mut self, value: T) -> bool
    where
        T: PartialEq,
    {
        if *self.value == value {
            return false;
        }

        *self.value = value;
        self.changed.set(self.this_run);

        true
    }
}

impl<T> std::ops::Deref for Mut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> std::ops::DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.changed.set(self.this_run);

        self.value
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Through `Deref`, so inspecting a component in a debugger or a log does
        // not mark it changed.
        f.debug_tuple("Mut").field(&self.value).finish()
    }
}

// SAFETY: as the shared impl; the exclusivity of the yielded reference rests on
// the conflict check in `Query::new` and on `World::query_mut` taking
// `&mut self`.
unsafe impl<T: Reflect> QueryData for &mut T {
    type Item<'w> = Mut<'w, T>;
    type State = (*mut T, *const std::cell::Cell<Tick>, Tick);

    fn collect_access(out: &mut Vec<Access>) {
        out.push(Access {
            type_id: T::type_id(),
            mutable: true,
        });
    }

    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
        archetype.column(T::type_id()).map(|column| {
            (
                column.as_ptr().cast::<T>(),
                column.changed_ticks_ptr(),
                ticks.this_run,
            )
        })
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        let (values, changed, this_run) = state;

        // SAFETY: as the shared impl, and no other live reference addresses this
        // element — the query holds `&mut World` and no component type appears
        // twice in one query. The stamp array is exactly as long as the column,
        // so the same row is in bounds for both.
        unsafe {
            Mut {
                value: &mut *values.add(row),
                changed: &*changed.add(row),
                this_run,
            }
        }
    }
}

// SAFETY: `state` is always `Some`, carrying `Some(column)` only when the
// archetype genuinely holds `T`, so `get` dereferences only a real column.
unsafe impl<T: Reflect> QueryData for Option<&T> {
    type Item<'w> = Option<&'w T>;
    /// `None` for an archetype without `T`. The outer `Option` in
    /// [`state`](QueryData::state) says whether the archetype matched at all;
    /// this inner one says whether it has the component.
    type State = Option<*const T>;

    fn collect_access(out: &mut Vec<Access>) {
        // Declared even though the component is optional: where it *is* present
        // this reads it, and a scheduler that let another system write `T`
        // concurrently would be wrong for those archetypes.
        <&T as QueryData>::collect_access(out);
    }

    fn state(archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
        // Always matches. That is the whole point — an optional component must
        // not narrow which archetypes are visited.
        Some(
            archetype
                .column(T::type_id())
                .map(|column| column.as_ptr().cast_const().cast::<T>()),
        )
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        // SAFETY: as the `&T` impl, for the archetypes that have the column.
        state.map(|column| unsafe { &*column.add(row) })
    }
}

// SAFETY: yields only shared references.
unsafe impl<T: Reflect> ReadOnlyQueryData for Option<&T> {}

// SAFETY: as the shared optional impl; exclusivity rests on the same conflict
// check and on `World::query_mut` taking `&mut self`.
unsafe impl<T: Reflect> QueryData for Option<&mut T> {
    type Item<'w> = Option<Mut<'w, T>>;
    type State = Option<<&'static mut T as QueryData>::State>;

    fn collect_access(out: &mut Vec<Access>) {
        <&mut T as QueryData>::collect_access(out);
    }

    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
        Some(<&mut T as QueryData>::state(archetype, ticks))
    }

    unsafe fn get<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        // SAFETY: as the `&mut T` impl, for the archetypes that have the column.
        state.map(|state| unsafe { <&mut T as QueryData>::get(state, row) })
    }
}

// SAFETY: an entity id is a `Copy` value read from the archetype's own roster;
// it names no component and so constrains nothing.
unsafe impl QueryData for Entity {
    type Item<'w> = Entity;
    type State = *const Entity;

    fn collect_access(_out: &mut Vec<Access>) {}

    fn state(archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
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

            fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
                Some(($($name::state(archetype, ticks)?,)+))
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

/// A constraint on which entities a query visits.
///
/// Unlike [`QueryData`] this yields nothing, which is the reason it is a
/// separate trait: `With<Player>` modelled as query data would have to produce
/// `()`, and every call site would unpack it.
///
/// The shape mirrors [`QueryData`] — resolve per-archetype state once, then
/// answer per row — because [`Changed`] and [`Added`] genuinely need the row.
/// `With` and `Without` answer from the signature alone and their per-row hook
/// is a constant `true`, which the optimizer removes.
///
/// # Safety
///
/// [`state`](Self::state) must return `Some` only for archetypes whose columns
/// [`matches`](Self::matches) can index, and the pointers it captures must be
/// that archetype's own. `matches` then trusts both and reads without checking.
///
/// Implemented for [`With`], [`Without`], [`Changed`], [`Added`], [`Or`], `()`,
/// and tuples of filters (which conjoin).
pub unsafe trait QueryFilter {
    /// Whatever resolving one archetype produced.
    type State: Copy;

    /// Resolve `archetype`, or `None` to skip it entirely.
    ///
    /// Skipping a whole archetype is what makes `With` and `Without` free.
    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State>;

    /// Record every component this inspects the *data* of.
    ///
    /// `With` and `Without` record nothing: they read a signature, never a
    /// component, so a system writing `Player` does not conflict with one
    /// filtering on it. `Changed` and `Added` read a stamp that travels with the
    /// component, so they record a read.
    ///
    /// This is for the scheduler and is deliberately **not** fed into the
    /// aliasing check a query performs on its own data. `&mut Position` filtered
    /// by `Changed<Position>` is an ordinary query, not an aliasing pair.
    fn collect_access(out: &mut Vec<Access>);

    /// Whether row `row` passes.
    ///
    /// # Safety
    ///
    /// `state` must have come from [`state`](Self::state) for an archetype that
    /// is still alive and unmodified, and `row` must be below its length.
    unsafe fn matches(state: Self::State, row: usize) -> bool;
}

/// Visit only archetypes holding `T`, without reading it.
///
/// ```ignore
/// world.query::<&Position>().with::<Player>()
/// ```
///
/// Asking for `&Player` instead would work and would also declare a read of
/// `Player`, which needlessly conflicts with any system writing it.
#[derive(Debug)]
pub struct With<T>(PhantomData<fn() -> T>);

// SAFETY: `State` is `()` and `matches` reads nothing.
unsafe impl<T: Reflect> QueryFilter for With<T> {
    type State = ();

    fn state(archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
        archetype.signature().contains(T::type_id()).then_some(())
    }

    fn collect_access(_out: &mut Vec<Access>) {}

    unsafe fn matches(_state: Self::State, _row: usize) -> bool {
        true
    }
}

/// Visit only archetypes **not** holding `T`.
///
/// The negation is free: an archetype's signature is its identity, so this is a
/// membership test done once per archetype rather than once per entity.
#[derive(Debug)]
pub struct Without<T>(PhantomData<fn() -> T>);

// SAFETY: as `With`.
unsafe impl<T: Reflect> QueryFilter for Without<T> {
    type State = ();

    fn state(archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
        (!archetype.signature().contains(T::type_id())).then_some(())
    }

    fn collect_access(_out: &mut Vec<Access>) {}

    unsafe fn matches(_state: Self::State, _row: usize) -> bool {
        true
    }
}

/// Visit only entities whose `T` was written since the query's `last_run`.
///
/// ```ignore
/// for mesh in world.query::<&Mesh>().since(last_upload).filtered::<Changed<Mesh>>() {
///     upload(mesh);
/// }
/// ```
///
/// An insert counts as a write, so a newly added component is also changed.
/// [`Added`] is what distinguishes the two.
///
/// A query built without [`since`](Query::since) compares against
/// [`Tick::ZERO`](crate::Tick::ZERO) and therefore matches everything — a caller
/// that has never run has not seen anything yet.
#[derive(Debug)]
pub struct Changed<T>(PhantomData<fn() -> T>);

// SAFETY: `state` returns `Some` only for an archetype holding `T`, capturing
// that column's own stamp array, which is exactly as long as the column.
unsafe impl<T: Reflect> QueryFilter for Changed<T> {
    type State = (*const std::cell::Cell<Tick>, Ticks);

    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
        archetype
            .column(T::type_id())
            .map(|column| (column.changed_ticks_ptr(), ticks))
    }

    fn collect_access(out: &mut Vec<Access>) {
        <&T as QueryData>::collect_access(out);
    }

    unsafe fn matches(state: Self::State, row: usize) -> bool {
        let (stamps, ticks) = state;

        // SAFETY: the caller guarantees `row` is within the archetype this state
        // came from, and the stamp array is as long as the column.
        let stamp = unsafe { (*stamps.add(row)).get() };

        stamp.is_newer_than(ticks.last_run, ticks.this_run)
    }
}

/// Visit only entities that gained a `T` since the query's `last_run`.
///
/// Distinct from [`Changed`], and not derivable from it: an insert is also a
/// write, so everything added is changed but not everything changed was added.
/// The upload-on-first-sight system wants this one.
///
/// A component that migrates between archetypes keeps the stamp it was first
/// added with, so gaining an unrelated component does not make an entity look
/// newly added.
#[derive(Debug)]
pub struct Added<T>(PhantomData<fn() -> T>);

// SAFETY: as `Changed`.
unsafe impl<T: Reflect> QueryFilter for Added<T> {
    type State = (*const Tick, Ticks);

    fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
        archetype
            .column(T::type_id())
            .map(|column| (column.added_ticks_ptr(), ticks))
    }

    fn collect_access(out: &mut Vec<Access>) {
        <&T as QueryData>::collect_access(out);
    }

    unsafe fn matches(state: Self::State, row: usize) -> bool {
        let (stamps, ticks) = state;

        // SAFETY: as `Changed`.
        let stamp = unsafe { *stamps.add(row) };

        stamp.is_newer_than(ticks.last_run, ticks.this_run)
    }
}

/// Pass if **any** member of the tuple `F` passes.
///
/// ```ignore
/// world.query::<&Position>().filtered::<Or<(With<Player>, With<Enemy>)>>()
/// ```
///
/// Tuples conjoin, so this is the only way to express a disjunction. There is
/// deliberately no impl for `Or<()>`: the identity of a disjunction is "matches
/// nothing", and a filter that silently discards everything is not worth being
/// able to write.
///
/// An archetype is visited if *any* member resolved for it, and each member that
/// did not is simply false for every row there. That is what lets
/// `Or<(With<A>, With<B>)>` visit archetypes holding only one of them.
#[derive(Debug)]
pub struct Or<F>(PhantomData<fn() -> F>);

/// The absence of a filter. Matches everything.
// SAFETY: reads nothing.
unsafe impl QueryFilter for () {
    type State = ();

    fn state(_archetype: &Archetype, _ticks: Ticks) -> Option<Self::State> {
        Some(())
    }

    fn collect_access(_out: &mut Vec<Access>) {}

    unsafe fn matches(_state: Self::State, _row: usize) -> bool {
        true
    }
}

/// Implement [`QueryFilter`] for a tuple and for [`Or`] of that tuple.
macro_rules! tuple_filter {
    ($($name:ident),+) => {
        /// A tuple conjoins: every member must pass.
        // SAFETY: `state` is `Some` only when every member resolved, so each
        // member's `matches` runs against state it produced itself.
        #[allow(non_snake_case, reason = "the macro names bindings after the type parameters")]
        unsafe impl<$($name: QueryFilter),+> QueryFilter for ($($name,)+) {
            type State = ($($name::State,)+);

            fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
                Some(($($name::state(archetype, ticks)?,)+))
            }

            fn collect_access(out: &mut Vec<Access>) {
                $($name::collect_access(out);)+
            }

            unsafe fn matches(state: Self::State, row: usize) -> bool {
                let ($($name,)+) = state;

                // SAFETY: the caller's obligations pass to each member unchanged.
                unsafe { $($name::matches($name, row))&&+ }
            }
        }

        // SAFETY: a member that did not resolve is stored as `None` and never
        // asked; one that did is asked with its own state.
        #[allow(non_snake_case, reason = "the macro names bindings after the type parameters")]
        unsafe impl<$($name: QueryFilter),+> QueryFilter for Or<($($name,)+)> {
            type State = ($(Option<$name::State>,)+);

            fn state(archetype: &Archetype, ticks: Ticks) -> Option<Self::State> {
                let state = ($($name::state(archetype, ticks),)+);
                let ($($name,)+) = &state;

                // Skip the archetype only when no member resolved for it.
                ($($name.is_some())||+).then_some(state)
            }

            fn collect_access(out: &mut Vec<Access>) {
                $($name::collect_access(out);)+
            }

            unsafe fn matches(state: Self::State, row: usize) -> bool {
                let ($($name,)+) = state;

                // SAFETY: as the conjoining impl.
                unsafe {
                    $($name.is_some_and(|state| $name::matches(state, row)))||+
                }
            }
        }
    };
}

tuple_filter!(A);
tuple_filter!(A, B);
tuple_filter!(A, B, C);
tuple_filter!(A, B, C, D);
tuple_filter!(A, B, C, D, E);
tuple_filter!(A, B, C, D, E, F);
tuple_filter!(A, B, C, D, E, F, G);
tuple_filter!(A, B, C, D, E, F, G, H);

/// Iterates every entity matching `D` and passing `F`.
///
/// Yields nothing for an archetype it does not match, and walks the rest row by
/// row over contiguous memory.
///
/// `F` defaults to `()`, which matches everything, so an unfiltered query is
/// written `Query<'_, D>` and costs nothing for the parameter it does not use.
pub struct Query<'w, D: QueryData, F: QueryFilter = ()> {
    archetypes: &'w [Archetype],
    /// The next archetype to consider.
    next_archetype: usize,
    /// Resolved columns for the archetype being walked.
    state: Option<D::State>,
    /// Resolved filter state for the same archetype.
    filter: Option<F::State>,
    row: usize,
    rows: usize,
    ticks: Ticks,
    _data: PhantomData<fn() -> (D, F)>,
}

impl<'w, D: QueryData, F: QueryFilter> Query<'w, D, F> {
    /// Build a query over `archetypes`.
    ///
    /// # Panics
    ///
    /// If the same component type is named twice and either is mutable. That
    /// would hand out two references to one element, one of them exclusive —
    /// undefined behaviour, and a property of the code as written rather than of
    /// the data, so it fails the first time the line runs.
    pub(crate) fn new(archetypes: &'w [Archetype], ticks: Ticks) -> Self {
        assert_no_conflicts::<D>();

        Self {
            archetypes,
            next_archetype: 0,
            state: None,
            filter: None,
            row: 0,
            rows: 0,
            ticks,
            _data: PhantomData,
        }
    }

    /// Ask change-detection questions relative to `last_run`.
    ///
    /// [`Changed`] and [`Added`] compare against this. Without it a query uses
    /// [`Tick::ZERO`](crate::Tick::ZERO), which reports everything as new.
    ///
    /// Once a scheduler exists it supplies this from the system's own last run
    /// and callers stop writing it by hand; the filters do not change.
    ///
    /// # Panics
    ///
    /// If the query has already been iterated, as [`filtered`](Self::filtered).
    pub fn since(mut self, last_run: Tick) -> Self {
        assert_eq!(
            self.next_archetype, 0,
            "a query's tick window must be set before it is iterated"
        );

        self.ticks.last_run = last_run;

        self
    }

    /// Narrow to archetypes holding `T`, without reading it.
    ///
    /// ```ignore
    /// for position in world.query::<&Position>().with::<Player>() {
    ///     // every player's position
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// If the query has already been iterated — see [`filtered`](Self::filtered).
    pub fn with<T: Reflect>(self) -> Query<'w, D, (F, With<T>)> {
        self.filtered()
    }

    /// Narrow to archetypes **not** holding `T`.
    ///
    /// # Panics
    ///
    /// If the query has already been iterated — see [`filtered`](Self::filtered).
    pub fn without<T: Reflect>(self) -> Query<'w, D, (F, Without<T>)> {
        self.filtered()
    }

    /// Narrow by an arbitrary [`QueryFilter`], which is how [`Or`] is used.
    ///
    /// ```ignore
    /// world.query::<&Position>().filtered::<Or<(With<Player>, With<Enemy>)>>()
    /// ```
    ///
    /// Named `filtered` rather than `filter` because an inherent method would
    /// shadow [`Iterator::filter`], which is genuinely useful on a query.
    ///
    /// # Panics
    ///
    /// If the query has already been iterated. Narrowing produces a fresh query
    /// rather than modifying this one, so rows already yielded would be visited
    /// a second time. That is a mistake worth reporting rather than a rewind
    /// worth performing silently.
    pub fn filtered<G: QueryFilter>(self) -> Query<'w, D, (F, G)> {
        assert_eq!(
            self.next_archetype, 0,
            "a query must be narrowed before it is iterated; \
             narrowing builds a fresh query, which would revisit rows already yielded"
        );

        Query::new(self.archetypes, self.ticks)
    }

    /// Move to the next archetype that matches, if any.
    fn advance_archetype(&mut self) -> bool {
        while let Some(archetype) = self.archetypes.get(self.next_archetype) {
            self.next_archetype += 1;

            // An empty archetype matches but yields nothing; skipping it here
            // saves two state resolves and keeps `next` from recursing.
            if archetype.is_empty() {
                continue;
            }

            // Resolved once per archetype, which is what makes a signature
            // filter free and a stamp filter one comparison per row.
            let Some(filter) = F::state(archetype, self.ticks) else {
                continue;
            };

            if let Some(state) = D::state(archetype, self.ticks) {
                self.state = Some(state);
                self.filter = Some(filter);
                self.row = 0;
                self.rows = archetype.len();

                return true;
            }
        }

        false
    }
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for Query<'w, D, F> {
    type Item = D::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let (Some(state), Some(filter)) = (self.state, self.filter) {
                while self.row < self.rows {
                    let row = self.row;
                    self.row += 1;

                    // SAFETY: `filter` was resolved from the archetype now being
                    // walked and `row` is below its length.
                    if !unsafe { F::matches(filter, row) } {
                        continue;
                    }

                    // SAFETY: `state` was resolved from the same archetype,
                    // `row` is below its length, and the yielded lifetime is
                    // `'w` — the borrow of the archetype slice the query holds.
                    // No structural change can happen during iteration, because
                    // that needs `&mut World` and the query borrows it.
                    return Some(unsafe { D::get(state, row) });
                }
            }

            if !self.advance_archetype() {
                return None;
            }
        }
    }
}

impl<D: QueryData, F: QueryFilter> std::fmt::Debug for Query<'_, D, F> {
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
