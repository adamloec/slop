//! A system: declared access, plus something to run.
//!
//! `docs/DESIGN.md` §2.5 asks for systems that declare read/write dependencies
//! so a scheduler can auto-parallelize them. This is that declaration, and the
//! shape it takes is decided by §2.3 rather than by Rust: **a system may be a
//! WASM guest export with no Rust type behind it.** So a system is data — an
//! access set and an opaque callable — and a Rust closure is the thin wrapper,
//! exactly as [`TypeInfo`](slop_reflect::TypeInfo) is a value rather than a
//! generic and [`insert`](crate::World::insert) is a wrapper over
//! [`insert_raw`](crate::World::insert_raw).
//!
//! Deriving the access set from the query types a Rust function uses is the
//! conventional alternative, and it is what Bevy's `IntoSystem` machinery
//! exists for. It cannot describe a guest system, so it would be a second
//! mechanism rather than a nicer front end for this one.
//!
//! # Declaring wrongly is caught, not undefined
//!
//! The scheduler proves two systems may run together by comparing their
//! *declared* access. If a system then queries something it did not declare, the
//! proof was about the wrong thing. That is why [`WorldCell::query`] checks:
//!
//! | | Result |
//! |---|---|
//! | Declared `&mut T`, queries `&T` | Fine. Over-declaring costs parallelism, never soundness |
//! | Declared `&T`, queries `&T` | Fine |
//! | Declared `&T`, queries `&mut T` | **Panics** |
//! | Did not declare `T`, queries it | **Panics** |
//!
//! The check runs once per query rather than once per row, and it converts the
//! one mistake this design makes possible from undefined behaviour into a
//! failure that names the system and the component.

use slop_reflect::{Reflect, TypeId};

use crate::query::{Access, AccessKind, Query, QueryData};
use crate::{CommandBuffer, Entity, Tick, Ticks, World};

impl Access {
    /// Declare a read of component `T`.
    pub fn read<T: Reflect>() -> Self {
        Self::read_id(T::type_id())
    }

    /// Declare a write of component `T`.
    pub fn write<T: Reflect>() -> Self {
        Self::write_id(T::type_id())
    }

    /// Declare a read of a component named at runtime — the guest path.
    pub fn read_id(type_id: TypeId) -> Self {
        Self {
            kind: AccessKind::Component,
            type_id,
            mutable: false,
        }
    }

    /// Declare a write of a component named at runtime.
    pub fn write_id(type_id: TypeId) -> Self {
        Self {
            kind: AccessKind::Component,
            type_id,
            mutable: true,
        }
    }

    /// Declare a read of resource `T`.
    pub fn read_resource<T: Reflect>() -> Self {
        Self::read_resource_id(T::type_id())
    }

    /// Declare a write of resource `T`.
    pub fn write_resource<T: Reflect>() -> Self {
        Self::write_resource_id(T::type_id())
    }

    /// Declare a read of a resource named at runtime.
    pub fn read_resource_id(type_id: TypeId) -> Self {
        Self {
            kind: AccessKind::Resource,
            type_id,
            mutable: false,
        }
    }

    /// Declare a write of a resource named at runtime.
    pub fn write_resource_id(type_id: TypeId) -> Self {
        Self {
            kind: AccessKind::Resource,
            type_id,
            mutable: true,
        }
    }
}

/// Whether two access sets may not run at the same time.
///
/// Two reads of the same component are compatible; anything involving a write
/// is not. This is the entire basis on which the scheduler parallelizes, so it
/// is deliberately the most conservative reading of the sets it is given —
/// component granularity, not archetype granularity.
///
/// Archetype granularity would find more parallelism (two systems writing
/// `Position` on disjoint archetypes cannot actually collide) and would stop
/// being sound the moment a structural change moved an entity between them.
/// Structural change is deferred to a sync point precisely so that could be
/// revisited, but it is not revisited here.
pub fn conflicts(left: &[Access], right: &[Access]) -> bool {
    left.iter().any(|left| {
        right.iter().any(|right| {
            left.kind == right.kind
                && left.type_id == right.type_id
                && (left.mutable || right.mutable)
        })
    })
}

/// Access to a world granted to one system, bounded by what it declared.
///
/// Handed to a system while other systems run concurrently. It grants queries —
/// including mutating ones — from a shared borrow, which is sound only because
/// the scheduler proved this system's declared access is disjoint from every
/// system running beside it. [`new`](Self::new) is where that promise is made.
///
/// It deliberately grants **no structural change**. Spawning, despawning,
/// inserting and removing all move entities between tables, which would
/// invalidate another system's in-flight query. Those go through the
/// [`CommandBuffer`] handed alongside, and land at the sync point
/// (`docs/DESIGN.md` §2.10).
#[derive(Clone, Copy)]
pub struct WorldCell<'w> {
    world: &'w World,
    access: &'w [Access],
    ticks: Ticks,
}

impl<'w> WorldCell<'w> {
    /// Grant access to `world` bounded by `access`.
    ///
    /// # Safety
    ///
    /// For as long as this cell or anything derived from it is live:
    ///
    /// 1. No `&mut World` may exist.
    /// 2. Every other thread touching this world must hold access disjoint from
    ///    `access` — no two of them naming the same component with either one
    ///    mutable.
    /// 3. Nothing may perform a structural change, which would move rows out
    ///    from under an in-flight query.
    ///
    /// [`Schedule`](crate::Schedule) is what discharges all three, and is the
    /// only caller in the engine. Constructing one by hand means taking on the
    /// proof yourself.
    pub unsafe fn new(world: &'w World, access: &'w [Access], ticks: Ticks) -> Self {
        Self {
            world,
            access,
            ticks,
        }
    }

    /// What this system declared.
    pub fn access(&self) -> &'w [Access] {
        self.access
    }

    /// The change-detection window this system is running in.
    pub fn ticks(&self) -> Ticks {
        self.ticks
    }

    /// Whether `entity` is alive.
    pub fn contains(&self, entity: Entity) -> bool {
        self.world.contains(entity)
    }

    /// How many entities are alive.
    pub fn len(&self) -> usize {
        self.world.len()
    }

    /// Whether the world holds no entities.
    pub fn is_empty(&self) -> bool {
        self.world.is_empty()
    }

    /// The type registry, for a system that resolves components at runtime.
    pub fn registry(&self) -> &'w slop_reflect::TypeRegistry {
        self.world.registry()
    }

    /// Iterate every entity holding the components `D` names.
    ///
    /// One method rather than the world's `query`/`query_mut` pair: the
    /// read-only split exists so `&World` and `&mut World` can distinguish
    /// themselves, and here neither is what bounds the access — the declaration
    /// is.
    ///
    /// The window from [`ticks`](Self::ticks) is applied, so
    /// [`Changed<T>`](crate::Changed) means "since this system last ran" without
    /// the system saying so.
    ///
    /// # Panics
    ///
    /// If `D` asks for anything this system did not declare, or asks mutably for
    /// something declared read-only. See the module documentation — this is the
    /// check that keeps a wrong declaration from being undefined behaviour.
    ///
    /// Also if `D` names one component twice with mutable access, as
    /// [`World::query_mut`].
    pub fn query<D: QueryData>(&self) -> Query<'w, D> {
        self.assert_declared::<D>();

        // SAFETY (of the `&mut` this may hand out, not of this call): `new`'s
        // contract says nothing conflicting with `self.access` runs beside us,
        // and the check above says `D` is within `self.access`.
        Query::new(self.world.archetypes(), self.ticks)
    }

    /// Read the resource of type `T`, if there is one.
    ///
    /// # Panics
    ///
    /// If this system did not declare [`Access::read_resource::<T>()`] or
    /// [`Access::write_resource::<T>()`]. Resources go through the same check as
    /// components, because the scheduler reasons about them the same way.
    pub fn resource<T: Reflect>(&self) -> Option<&'w T> {
        self.assert_covers(Access::read_resource::<T>());

        let pointer = self.world.resources().get(T::type_id())?;

        // SAFETY: the column was built from `T`'s own registration, so the
        // element is an initialized `T`. `new`'s contract says nothing
        // conflicting with this system's declared access runs beside it, and the
        // check above says this resource is within it.
        Some(unsafe { &*pointer.cast::<T>() })
    }

    /// Mutate the resource of type `T`, if there is one.
    ///
    /// Yields [`Mut<T>`](crate::Mut), so the change stamp is written when the
    /// value is reached rather than when it is looked up — as a query does, and
    /// unlike [`World::resource_mut`], which is a point lookup outside any
    /// schedule.
    ///
    /// # Panics
    ///
    /// If this system did not declare [`Access::write_resource::<T>()`].
    pub fn resource_mut<T: Reflect>(&self) -> Option<crate::Mut<'w, T>> {
        self.assert_covers(Access::write_resource::<T>());

        let column = self.world.resources().column(T::type_id())?;

        // SAFETY: as `resource`, and the declaration check above established
        // exclusive access to this resource for the duration of the batch. The
        // stamp at index 0 belongs to the value at index 0, which is the only
        // element a resource column ever holds.
        Some(unsafe {
            crate::Mut::new(
                &mut *column.as_ptr().cast::<T>(),
                &*column.changed_ticks_ptr(),
                self.ticks.this_run,
            )
        })
    }

    /// Whether a resource of type `T` is present.
    ///
    /// # Panics
    ///
    /// If this system did not declare access to it. Presence is information
    /// about the resource, and a system that did not declare it may be running
    /// beside one installing it.
    pub fn contains_resource<T: Reflect>(&self) -> bool {
        self.assert_covers(Access::read_resource::<T>());

        self.world.resources().contains(T::type_id())
    }

    /// Panic unless `D` is covered by what this system declared.
    /// Panic unless `D` is covered by what this system declared.
    ///
    /// Visits `D`'s access rather than collecting it. This ran once per query
    /// per frame and allocated a `Vec` each time, in the frame loop, which
    /// `docs/CONVENTIONS.md` §8 says allocates nothing — `CONSIDERATIONS.md`
    /// item 7. The set is a pure function of `D`, so there was never anything
    /// to keep; the `Vec` existed only because the trait's shape asked for one.
    fn assert_declared<D: QueryData>(&self) {
        D::each_access(&mut |wanted| self.assert_covers(wanted));
    }

    /// Panic unless `wanted` is covered by what this system declared.
    ///
    /// Covered means: same kind, same type, and mutable if `wanted` is mutable.
    /// Declaring a write and performing a read is fine — over-declaring costs
    /// parallelism, never soundness.
    fn assert_covers(&self, wanted: Access) {
        let covered = self.access.iter().any(|declared| {
            declared.kind == wanted.kind
                && declared.type_id == wanted.type_id
                && (declared.mutable || !wanted.mutable)
        });

        assert!(
            covered,
            "a system used {} {} {} without declaring it; \
             the scheduler decided what could run alongside this system \
             from the declaration, so this is outside what was proved",
            match wanted.kind {
                AccessKind::Component => "component",
                AccessKind::Resource => "resource",
            },
            wanted.type_id,
            if wanted.mutable {
                "mutably"
            } else {
                "immutably"
            },
        );
    }
}

impl std::fmt::Debug for WorldCell<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorldCell")
            .field("access", &self.access.len())
            .field("ticks", &self.ticks)
            .finish_non_exhaustive()
    }
}

/// What a system does, type-erased.
///
/// `Send + Sync` because the scheduler runs it on a worker thread, and `Fn`
/// rather than `FnMut` because several may run at once and a system's own state
/// is not a thing this design has — state lives in the world.
type Run = dyn Fn(WorldCell<'_>, &mut CommandBuffer) + Send + Sync;

/// A unit of work over the world, with the access it needs stated up front.
pub struct System {
    name: Box<str>,
    access: Vec<Access>,
    run: Box<Run>,
    /// When this system last ran, which is what its `Changed` filters compare
    /// against. Owned here rather than passed in, because "since I last ran" is
    /// a property of the system and nothing else knows it.
    last_run: Tick,
}

impl System {
    /// Build a system from its declared access and a body.
    ///
    /// ```ignore
    /// System::new(
    ///     "integrate velocity",
    ///     vec![Access::write::<Position>(), Access::read::<Velocity>()],
    ///     |world, _commands| {
    ///         for (mut position, velocity) in world.query::<(&mut Position, &Velocity)>() {
    ///             position.x += velocity.dx;
    ///         }
    ///     },
    /// )
    /// ```
    ///
    /// The access set is duplicated in the body's query types and is not derived
    /// from them, for the reason in the module documentation: a guest system has
    /// no Rust query types to derive from. Over-declaring is safe and costs
    /// parallelism; under-declaring panics when the query runs.
    pub fn new<F>(name: impl Into<Box<str>>, access: Vec<Access>, run: F) -> Self
    where
        F: Fn(WorldCell<'_>, &mut CommandBuffer) + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            access,
            run: Box::new(run),
            last_run: Tick::ZERO,
        }
    }

    /// What this system is called, for diagnostics and for reading a schedule.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The components it reads and writes.
    pub fn access(&self) -> &[Access] {
        &self.access
    }

    /// When it last ran. [`Tick::ZERO`] until it has.
    pub fn last_run(&self) -> Tick {
        self.last_run
    }

    /// Whether this system and `other` may not run at the same time.
    pub fn conflicts_with(&self, other: &System) -> bool {
        conflicts(&self.access, &other.access)
    }

    /// Run it against `world`, recording structural change into `commands`.
    ///
    /// # Safety
    ///
    /// As [`WorldCell::new`], whose contract this passes straight through.
    pub(crate) unsafe fn run(&self, world: &World, this_run: Tick, commands: &mut CommandBuffer) {
        let ticks = Ticks {
            last_run: self.last_run,
            this_run,
        };

        // SAFETY: the caller's obligation, unchanged.
        let cell = unsafe { WorldCell::new(world, &self.access, ticks) };

        (self.run)(cell, commands);
    }

    /// Record that it ran at `tick`.
    pub(crate) fn mark_run(&mut self, tick: Tick) {
        self.last_run = tick;
    }
}

impl std::fmt::Debug for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("System")
            .field("name", &self.name)
            .field("access", &self.access.len())
            .field("last_run", &self.last_run)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_reads_of_one_component_do_not_conflict() {
        let left = [Access::read::<u32>()];
        let right = [Access::read::<u32>()];

        assert!(!conflicts(&left, &right));
    }

    #[test]
    fn a_write_conflicts_with_a_read_of_the_same_component() {
        let left = [Access::write::<u32>()];
        let right = [Access::read::<u32>()];

        assert!(conflicts(&left, &right));
        assert!(conflicts(&right, &left));
    }

    #[test]
    fn two_writes_of_one_component_conflict() {
        let left = [Access::write::<u32>()];
        let right = [Access::write::<u32>()];

        assert!(conflicts(&left, &right));
    }

    #[test]
    fn writes_of_different_components_never_conflict() {
        let left = [Access::write::<u32>(), Access::read::<f32>()];
        let right = [Access::write::<u64>(), Access::read::<f32>()];

        assert!(!conflicts(&left, &right));
    }

    #[test]
    fn an_empty_access_set_conflicts_with_nothing() {
        assert!(!conflicts(&[], &[Access::write::<u32>()]));
        assert!(!conflicts(&[Access::write::<u32>()], &[]));
    }

    #[test]
    fn a_runtime_named_component_declares_like_any_other() {
        // The guest path: no Rust type, just an id.
        let id = <u32 as Reflect>::type_id();

        assert_eq!(Access::read_id(id), Access::read::<u32>());
        assert_eq!(Access::write_id(id), Access::write::<u32>());
    }
}
