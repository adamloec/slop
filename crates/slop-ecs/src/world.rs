//! The world: entities, their components, and where each one lives.
//!
//! Three structures, and the whole of this module is keeping them agreed:
//!
//! | | Answers |
//! |---|---|
//! | [`HandleAllocator`] | which entity ids are live |
//! | `archetypes` | which entities hold which components, stored column-wise |
//! | `locations` | where a given entity's row is |
//!
//! # Structural change means physically moving an entity
//!
//! `docs/DESIGN.md` §2.10 took archetype storage knowing this: adding or
//! removing a component changes which table an entity belongs to, so its
//! components are relocated between tables. That is the cost paid for iteration
//! being a linear scan, and §2.10's stated mitigation — queueing structural
//! changes into command buffers applied at a sync point — is a layer above this
//! one. What lives here is the move itself, done correctly.
//!
//! The move has four steps and every one of them can leave the world
//! inconsistent if skipped:
//!
//! 1. Reserve a row in the destination archetype.
//! 2. Relocate every shared component out of the source, without dropping it.
//! 3. Drop any component the destination does not have, and write any it gains.
//! 4. Patch the location index — for the migrating entity **and** for whichever
//!    entity the source's swap-remove moved into the hole.
//!
//! Step 4 is the one that looks optional and is not. Missing it leaves another
//! entity's location pointing at a row that now belongs to someone else, which
//! reads as one entity mysteriously acquiring another's components.

use slop_core::{FxHashMap, HandleAllocator};
use slop_reflect::{Reflect, TypeId, TypeInfo, TypeRegistry, Value};

use crate::query::{Query, QueryData, ReadOnlyQueryData};
use crate::{
    Archetype, EcsError, ElementTicks, Entity, EntityTag, Row, Signature, Tick, Ticks, serialize,
};

/// Where an entity's components are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Location {
    /// Index into `World::archetypes`.
    archetype: usize,
    row: Row,
}

/// Entities, their components, and the type registry describing them.
///
/// The registry is owned rather than borrowed: `docs/DESIGN.md` §2.12's editor
/// opens several projects at once, each with its own guest modules declaring
/// their own component types, and a world sharing a registry with another
/// project would let one project's `Inventory` resolve to the other's.
pub struct World {
    entities: HandleAllocator<EntityTag>,
    archetypes: Vec<Archetype>,
    /// Signature to index in `archetypes`, so a structural change finds its
    /// destination table without a scan.
    by_signature: FxHashMap<Signature, usize>,
    locations: FxHashMap<Entity, Location>,
    registry: TypeRegistry,
    /// Data the world holds exactly one of — see the `resource` module.
    resources: crate::resource::Resources,
    /// What every write is stamped with — see [`advance_tick`](Self::advance_tick).
    tick: Tick,
}

impl World {
    /// A world holding nothing, with `registry` describing its component types.
    ///
    /// The empty archetype is created up front, so an entity spawned with no
    /// components has somewhere to be from the start.
    pub fn new(registry: TypeRegistry) -> Self {
        let empty = Archetype::new(Signature::empty(), &registry)
            .expect("the empty signature resolves no types");

        let mut by_signature = FxHashMap::default();
        by_signature.insert(Signature::empty(), 0);

        Self {
            entities: HandleAllocator::new(),
            archetypes: vec![empty],
            by_signature,
            locations: FxHashMap::default(),
            registry,
            resources: crate::resource::Resources::default(),
            // One, not zero. A query that has never run compares against
            // `Tick::ZERO`, and starting there would make the world's first
            // writes indistinguishable from nothing having happened.
            tick: Tick::ZERO.next(),
        }
    }

    /// The tick every write is currently stamped with.
    pub fn tick(&self) -> Tick {
        self.tick
    }

    /// Move to the next tick.
    ///
    /// Everything written from now on is stamped later than everything written
    /// before. Nothing calls this for you — how often a tick passes is a
    /// property of the caller's loop, not of the world, and a caller that never
    /// advances simply has every write share one stamp.
    ///
    /// The frame loop advances once per frame; a scheduler running systems in
    /// sequence advances once per system, which is what lets each see the
    /// previous one's writes as changes.
    pub fn advance_tick(&mut self) -> Tick {
        self.tick = self.tick.next();

        self.tick
    }

    /// A world whose registry knows the built-in types and nothing else.
    pub fn with_builtins() -> Self {
        let mut registry = TypeRegistry::new();
        slop_reflect::register_builtins(&mut registry).expect("a fresh registry cannot conflict");

        Self::new(registry)
    }

    /// The type registry.
    pub fn registry(&self) -> &TypeRegistry {
        &self.registry
    }

    /// The type registry, mutably, for registering component types.
    ///
    /// Registering after entities exist is allowed and normal — a guest module
    /// loaded mid-session brings its own types. Existing archetypes are
    /// unaffected, since a type nobody has used yet has no columns.
    pub fn registry_mut(&mut self) -> &mut TypeRegistry {
        &mut self.registry
    }

    /// How many entities are alive.
    pub fn len(&self) -> usize {
        self.locations.len()
    }

    /// Whether no entities are alive.
    pub fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }

    /// Whether `entity` is alive.
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.is_live(entity)
    }

    /// Every archetype, for queries to walk.
    pub fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// Iterate every entity holding the components `D` names, reading only.
    ///
    /// ```ignore
    /// for (entity, position) in world.query::<(Entity, &Position)>() {
    ///     println!("{entity:?} is at {position:?}");
    /// }
    /// ```
    ///
    /// Takes `&self`, so several read-only queries may be live at once. `D` must
    /// be [`ReadOnlyQueryData`], which `&mut T` deliberately is not — asking for
    /// mutation here fails to compile rather than at runtime.
    ///
    /// # Panics
    ///
    /// If `D` names one component type twice with mutable access, which is
    /// impossible here since mutable access does not typecheck.
    pub fn query<D: ReadOnlyQueryData>(&self) -> Query<'_, D> {
        Query::new(&self.archetypes, Ticks::everything(self.tick))
    }

    /// Iterate every entity holding the components `D` names, with mutation.
    ///
    /// ```ignore
    /// for (position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
    ///     position.x += velocity.dx;
    /// }
    /// ```
    ///
    /// Takes `&mut self`, which is what makes the yielded `&mut` references
    /// sound: no other query, no `get`, and no structural change can be in
    /// flight while this iterates.
    ///
    /// # Panics
    ///
    /// If `D` names one component type twice and either access is mutable —
    /// `(&mut Position, &Position)` would hand out an aliasing pair. That is a
    /// property of the code as written rather than of the data, so it fails the
    /// first time the line runs.
    pub fn query_mut<D: QueryData>(&mut self) -> Query<'_, D> {
        Query::new(&self.archetypes, Ticks::everything(self.tick))
    }

    /// Create an entity with no components.
    pub fn spawn(&mut self) -> Entity {
        let entity = self.entities.allocate();

        // SAFETY: the empty archetype has no columns, so there are no slots to
        // initialize.
        let (row, slots) = unsafe { self.archetypes[0].begin_row(entity, self.tick) };
        debug_assert!(slots.is_empty(), "the empty archetype has no columns");

        self.locations
            .insert(entity, Location { archetype: 0, row });

        entity
    }

    /// Destroy `entity` and drop its components.
    ///
    /// Returns whether it was alive. Despawning twice is not an error — a
    /// gameplay system holding a handle to something already destroyed is
    /// routine, and `docs/PLAN.md` §4.1-C chose checked access over panicking
    /// for exactly that reason.
    pub fn despawn(&mut self, entity: Entity) -> bool {
        let Some(location) = self.locations.remove(&entity) else {
            return false;
        };

        let moved = self.archetypes[location.archetype].remove_row(location.row);
        self.patch_moved(location, moved);
        self.entities.free(entity);

        true
    }

    /// Whether `entity` holds a component of type `T`.
    pub fn has<T: Reflect>(&self, entity: Entity) -> bool {
        self.locations.get(&entity).is_some_and(|location| {
            self.archetypes[location.archetype]
                .signature()
                .contains(T::type_id())
        })
    }

    /// Read `entity`'s `T`, if it has one.
    pub fn get<T: Reflect>(&self, entity: Entity) -> Option<&T> {
        let location = self.locations.get(&entity)?;
        let column = self.archetypes[location.archetype].column(T::type_id())?;
        let pointer = column.get(location.row.0)?;

        // SAFETY: the column was built from the `TypeInfo` registered for
        // `T::type_id()`, which is `T`'s own, so the element is an initialized
        // `T`. The borrow is tied to `&self`, so nothing can mutate or move it
        // while the reference lives.
        Some(unsafe { &*pointer.cast::<T>() })
    }

    /// Mutate `entity`'s `T`, if it has one.
    ///
    /// **Stamps the component as changed whether or not it is written.** A query
    /// is precise about this — [`Mut`](crate::Mut) stamps only when the value is
    /// actually reached mutably — but a point lookup is a caller who has already
    /// said which single component they intend to write, so the eager stamp
    /// costs at most one false positive per call and keeps this returning a
    /// plain `&mut T`.
    pub fn get_mut<T: Reflect>(&mut self, entity: Entity) -> Option<&mut T> {
        let tick = self.tick;
        let location = *self.locations.get(&entity)?;
        let column = self.archetypes[location.archetype].column_mut(T::type_id())?;
        column.mark_changed(location.row.0, tick);
        let pointer = column.get_mut(location.row.0)?;

        // SAFETY: as `get`, and the borrow is tied to `&mut self` so it is
        // exclusive.
        Some(unsafe { &mut *pointer.cast::<T>() })
    }

    /// Give `entity` a component, moving it to the archetype that holds one.
    ///
    /// Replaces the existing value if it already has one, which is a write
    /// rather than a move.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredComponent`] if `T` is not registered, or
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive.
    pub fn insert<T: Reflect>(&mut self, entity: Entity, component: T) -> Result<(), EcsError> {
        let mut component = std::mem::ManuallyDrop::new(component);
        let pointer = std::ptr::from_mut(&mut *component)
            .cast::<u8>()
            .cast_const();

        // SAFETY: `pointer` is an initialized, aligned `T`, and `T::type_id()`
        // names `T` by `Reflect`'s contract.
        match unsafe { self.insert_raw(entity, T::type_id(), pointer) } {
            Ok(()) => Ok(()),
            Err(error) => {
                // `insert_raw` moves the value out only on success, so on this
                // path it is still live and still ours.
                //
                // SAFETY: nothing took ownership, and `component` is not used
                // again.
                unsafe { std::mem::ManuallyDrop::drop(&mut component) };

                Err(error)
            }
        }
    }

    /// Give `entity` a component described at runtime.
    ///
    /// The untyped half of [`insert`](Self::insert), and the path §2.4's guest
    /// components take: a WASM module's component type has no Rust type behind
    /// it, so the value arrives as bytes plus the [`TypeId`] naming their
    /// layout. Deferred structural change ([`CommandBuffer`](crate::CommandBuffer))
    /// takes the same path for the same reason — a recorded component is bytes
    /// in a staging area by the time it is applied.
    ///
    /// # Safety
    ///
    /// `component` must point at an initialized, properly aligned value of
    /// exactly the type `type_id` names, and must be treated as moved-from
    /// afterward — but **only if this returns `Ok`**. On an error the value was
    /// not taken and remains the caller's to drop.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredComponent`] if `type_id` is not registered, or
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive.
    pub unsafe fn insert_raw(
        &mut self,
        entity: Entity,
        type_id: TypeId,
        component: *const u8,
    ) -> Result<(), EcsError> {
        let size = self
            .registry
            .get(type_id)
            .ok_or(EcsError::UnregisteredComponent { type_id })?
            .layout()
            .size();

        let location = *self
            .locations
            .get(&entity)
            .ok_or(EcsError::NoSuchEntity { entity })?;

        let tick = self.tick;

        // Already present: overwrite in place. No table changes, so none of the
        // migration machinery runs.
        if let Some(column) = self.archetypes[location.archetype].column_mut(type_id) {
            // SAFETY: the column holds exactly this type, the row is occupied,
            // and `component` is the caller's value living outside this column.
            unsafe { column.replace(location.row.0, component, tick) };
            return Ok(());
        }

        let destination = self.archetypes[location.archetype]
            .signature()
            .with(type_id)
            .expect("the component is absent, so the signature must grow");
        let destination = self.archetype_index(&destination)?;

        // SAFETY: every slot returned below is written exactly once — the
        // shared components by `relocate_into`, and the new component by the
        // explicit write, which is the caller's value being moved in.
        unsafe {
            let (row, slots) = self.archetypes[destination].begin_row(entity, tick);
            let (shared, ticks) = self.relocate_into(location, destination, &slots, Some(type_id));

            let slot = self.slot_for(destination, &slots, type_id);
            std::ptr::copy_nonoverlapping(component, slot, size);

            // The relocated components keep the stamps they arrived with; the
            // one being added keeps the fresh stamp `begin_row` gave it, which
            // is why its entry in `ticks` is `None`.
            self.archetypes[destination].set_row_ticks(row, &ticks);

            self.finish_move(entity, location, destination, row, shared);
        }

        Ok(())
    }

    /// Take a component away from `entity`, moving it to the archetype without
    /// one.
    ///
    /// Returns whether it had one. The component is dropped; recovering the
    /// value would mean the caller owning it, which needs a typed path this does
    /// not yet have.
    pub fn remove<T: Reflect>(&mut self, entity: Entity) -> bool {
        self.remove_by_id(entity, T::type_id())
    }

    /// Take a component away from `entity`, naming it at runtime.
    ///
    /// The untyped half of [`remove`](Self::remove) — see
    /// [`insert_raw`](Self::insert_raw) for why an untyped path exists. Safe,
    /// unlike its counterpart: removing needs no value from the caller, so
    /// there is nothing to get wrong about it.
    ///
    /// Returns whether the entity had one.
    pub fn remove_by_id(&mut self, entity: Entity, type_id: TypeId) -> bool {
        let Some(&location) = self.locations.get(&entity) else {
            return false;
        };

        let Some(destination) = self.archetypes[location.archetype]
            .signature()
            .without(type_id)
        else {
            return false;
        };

        let Ok(destination) = self.archetype_index(&destination) else {
            // Unreachable: a signature that already exists as an archetype
            // resolved all its types when it was built.
            return false;
        };

        // SAFETY: every destination slot is written by `relocate_into`, which
        // covers exactly the destination's columns. The removed component is
        // dropped rather than relocated, because the destination has no column
        // for it.
        unsafe {
            let (row, slots) = self.archetypes[destination].begin_row(entity, self.tick);
            let (shared, ticks) = self.relocate_into(location, destination, &slots, None);

            self.archetypes[destination].set_row_ticks(row, &ticks);
            self.finish_move(entity, location, destination, row, shared);
        }

        true
    }

    /// Move every component the destination wants out of the source row.
    ///
    /// Components the destination lacks are dropped. `skip` names a type the
    /// destination has but the source does not, which the caller writes itself.
    ///
    /// Returns the entity the source's swap-remove moved into the vacated row,
    /// and the stamps each relocated component arrived with — one entry per
    /// destination column, `None` for the one the caller writes itself.
    ///
    /// The stamps are collected rather than applied here because applying them
    /// means borrowing the destination archetype, which this is already
    /// borrowing the source out of.
    ///
    /// # Safety
    ///
    /// Every slot in `slots` other than `skip`'s must correspond to a
    /// destination column, and this writes each one exactly once.
    unsafe fn relocate_into(
        &mut self,
        location: Location,
        destination: usize,
        slots: &[*mut u8],
        skip: Option<TypeId>,
    ) -> (Option<Entity>, Vec<Option<ElementTicks>>) {
        let source_types: Vec<TypeId> = self.archetypes[location.archetype]
            .signature()
            .types()
            .to_vec();
        let destination_signature = self.archetypes[destination].signature().clone();

        let mut ticks = vec![None; destination_signature.len()];

        for &type_id in &source_types {
            let column = self.archetypes[location.archetype]
                .column_mut(type_id)
                .expect("the type came from this archetype's own signature");

            match destination_signature.position(type_id) {
                // Shared: relocate the bytes, no destructor, and carry the
                // stamps across — relocating is not writing.
                Some(index) => {
                    // SAFETY: `slots[index]` is the destination column for this
                    // exact type, uninitialized and correctly aligned.
                    ticks[index] = unsafe { column.swap_remove_to(location.row.0, slots[index]) };
                }
                // The destination does not want it, so it is destroyed.
                None => {
                    column.swap_remove(location.row.0);
                }
            }
        }

        debug_assert!(
            skip.is_none_or(|type_id| !source_types.contains(&type_id)),
            "the skipped type must be the one the source lacks"
        );

        // The columns are done; the entity list still has to shed its row, and
        // it is what reports who moved.
        let moved = self.archetypes[location.archetype].take_row(location.row);

        (moved, ticks)
    }

    /// Update the location index after a completed move.
    fn finish_move(
        &mut self,
        entity: Entity,
        from: Location,
        archetype: usize,
        row: Row,
        moved: Option<Entity>,
    ) {
        self.locations.insert(entity, Location { archetype, row });

        self.patch_moved(from, moved);
    }

    /// Point `moved` at the row it was swapped into.
    ///
    /// The step that looks optional and is not: without it, another entity's
    /// location refers to a row that now belongs to someone else, and that
    /// entity appears to acquire another's components.
    fn patch_moved(&mut self, vacated: Location, moved: Option<Entity>) {
        let Some(moved) = moved else {
            return;
        };

        if let Some(location) = self.locations.get_mut(&moved) {
            location.archetype = vacated.archetype;
            location.row = vacated.row;
        }
    }

    /// The slot for `type_id` among a `begin_row` result.
    fn slot_for(&self, archetype: usize, slots: &[*mut u8], type_id: TypeId) -> *mut u8 {
        let index = self.archetypes[archetype]
            .signature()
            .position(type_id)
            .expect("the destination archetype holds this type");

        slots[index]
    }

    /// The index of the archetype for `signature`, creating it if needed.
    fn archetype_index(&mut self, signature: &Signature) -> Result<usize, EcsError> {
        if let Some(&index) = self.by_signature.get(signature) {
            return Ok(index);
        }

        let archetype = Archetype::new(signature.clone(), &self.registry)?;
        let index = self.archetypes.len();

        self.archetypes.push(archetype);
        self.by_signature.insert(signature.clone(), index);

        Ok(index)
    }

    /// Read a component as a [`Value`].
    ///
    /// The reflection path: no `T`, so a component declared by a guest module
    /// reads back exactly as a host-native one does.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredComponent`] if `type_id` is not registered,
    /// [`EcsError::NoSuchEntity`] if the entity is not alive, and
    /// [`EcsError::Value`] if the type cannot be described — an opaque component
    /// has nothing to read out.
    pub fn component_value(&self, entity: Entity, type_id: TypeId) -> Result<Value, EcsError> {
        let info = self
            .registry
            .get(type_id)
            .ok_or(EcsError::UnregisteredComponent { type_id })?;

        let location = self
            .locations
            .get(&entity)
            .ok_or(EcsError::NoSuchEntity { entity })?;

        let pointer = self.archetypes[location.archetype]
            .column(type_id)
            .and_then(|column| column.get(location.row.0))
            .ok_or(EcsError::MissingComponent { entity, type_id })?;

        // SAFETY: the column was built from this exact `TypeInfo`, so the
        // element is an initialized value of it, and the borrow of `self` keeps
        // it alive for the read. Nothing is moved out — owning fields are
        // cloned.
        Ok(unsafe { serialize::to_value(info, pointer, &self.registry) }?)
    }

    /// Give `entity` a component built from a [`Value`].
    ///
    /// The other half of [`component_value`](Self::component_value), and what a
    /// scene loader inserts through.
    ///
    /// # Errors
    ///
    /// As [`component_value`](Self::component_value), plus [`EcsError::Value`]
    /// if the value does not match the type. **Nothing is inserted on an
    /// error** — the value is checked in full before any memory is written.
    pub fn insert_value(
        &mut self,
        entity: Entity,
        type_id: TypeId,
        value: &Value,
    ) -> Result<(), EcsError> {
        let info = self
            .registry
            .get(type_id)
            .ok_or(EcsError::UnregisteredComponent { type_id })?;
        let layout = info.layout();

        // Both failure paths are taken before anything is written, so the write
        // below cannot leave a value stranded in scratch space.
        serialize::validate(value, info, &self.registry)?;

        if !self.contains(entity) {
            return Err(EcsError::NoSuchEntity { entity });
        }

        // SAFETY: `with_scratch` hands out uninitialized space for exactly this
        // layout; `write` fills it, having been validated above; and
        // `insert_raw` moves out of it, so nothing is left for `with_scratch` to
        // free beyond the allocation itself.
        unsafe {
            serialize::with_scratch(layout, |scratch| {
                let info = self
                    .registry
                    .get(type_id)
                    .expect("resolved immediately above");
                serialize::write_value(value, info, scratch, &self.registry);

                self.insert_raw(entity, type_id, scratch)
                    .expect("the entity is alive and the type is registered");
            });
        }

        Ok(())
    }

    /// Read a resource as a [`Value`].
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredResource`] if `type_id` is not registered, and
    /// [`EcsError::Value`] if the type cannot be described.
    pub fn resource_value(&self, type_id: TypeId) -> Result<Option<Value>, EcsError> {
        let info = self
            .registry
            .get(type_id)
            .ok_or(EcsError::UnregisteredResource { type_id })?;

        let Some(pointer) = self.resources.get(type_id) else {
            return Ok(None);
        };

        // SAFETY: as `component_value` — the column was built from this
        // `TypeInfo` and holds one initialized element.
        Ok(Some(unsafe {
            serialize::to_value(info, pointer, &self.registry)
        }?))
    }

    /// Install a resource built from a [`Value`].
    ///
    /// # Errors
    ///
    /// As [`resource_value`](Self::resource_value), plus [`EcsError::Value`] if
    /// the value does not match the type. Nothing is installed on an error.
    pub fn insert_resource_value(
        &mut self,
        type_id: TypeId,
        value: &Value,
    ) -> Result<(), EcsError> {
        let info = self
            .registry
            .get(type_id)
            .ok_or(EcsError::UnregisteredResource { type_id })?;

        serialize::validate(value, info, &self.registry)?;

        let info = info.clone();
        let tick = self.tick;

        // SAFETY: `with_scratch` hands out uninitialized space for exactly this
        // layout, `write` fills it after validation, and `Resources::insert`
        // moves out of it.
        unsafe {
            serialize::with_scratch(info.layout(), |scratch| {
                serialize::write_value(value, &info, scratch, &self.registry);
                self.resources.insert(&info, scratch, tick);
            });
        }

        Ok(())
    }

    /// Install a resource, replacing and dropping any previous value.
    ///
    /// A resource is data the world holds exactly one of — the clock, the input
    /// state, the active camera. See the `resource` module for
    /// why it is not a component on a singleton entity.
    ///
    /// The type must be registered, as a component type must, and for the same
    /// reason: a column cannot be allocated without a layout or freed without a
    /// destructor, and `docs/DESIGN.md` §2.4 requires both to arrive as data.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredResource`] if `T` is not registered.
    pub fn insert_resource<T: Reflect>(&mut self, value: T) -> Result<(), EcsError> {
        let tick = self.tick;
        let info = crate::resource::require_registered::<T>(&self.registry)?.clone();

        let value = std::mem::ManuallyDrop::new(value);

        // SAFETY: `value` is an initialized `T`, `info` is `T`'s own registration
        // so the column is built for exactly this layout, and `ManuallyDrop`
        // stops the local destructor running after the resource takes ownership.
        unsafe {
            self.resources
                .insert(&info, std::ptr::from_ref(&*value).cast::<u8>(), tick);
        }

        Ok(())
    }

    /// Read the resource of type `T`, if there is one.
    pub fn resource<T: Reflect>(&self) -> Option<&T> {
        let pointer = self.resources.get(T::type_id())?;

        // SAFETY: the column was built from the `TypeInfo` registered for
        // `T::type_id()`, which is `T`'s own, so the element is an initialized
        // `T`. The borrow is tied to `&self`.
        Some(unsafe { &*pointer.cast::<T>() })
    }

    /// Mutate the resource of type `T`, if there is one.
    ///
    /// Stamps it changed whether or not it is written, as
    /// [`get_mut`](Self::get_mut) does and for the same reason.
    pub fn resource_mut<T: Reflect>(&mut self) -> Option<&mut T> {
        let tick = self.tick;
        let pointer = self.resources.get_mut(T::type_id(), tick)?;

        // SAFETY: as `resource`, and the borrow is tied to `&mut self`.
        Some(unsafe { &mut *pointer.cast::<T>() })
    }

    /// Whether a resource of type `T` is present.
    pub fn contains_resource<T: Reflect>(&self) -> bool {
        self.resources.contains(T::type_id())
    }

    /// Drop the resource of type `T`. Returns whether there was one.
    pub fn remove_resource<T: Reflect>(&mut self) -> bool {
        self.resources.remove(T::type_id())
    }

    /// How many resources the world holds.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Every resource type currently present, ordered by id.
    ///
    /// What a serializer walks. Ordered rather than merely reproducible, because
    /// a file format needs a defined order (`docs/DESIGN.md` §2.14).
    pub fn resource_types(&self) -> Vec<TypeId> {
        self.resources.type_ids()
    }

    /// When the resource of type `T` was added and last changed.
    pub fn resource_ticks<T: Reflect>(&self) -> Option<crate::ElementTicks> {
        self.resources.ticks(T::type_id())
    }

    /// The resource store, for [`WorldCell`](crate::WorldCell) to read through.
    pub(crate) fn resources(&self) -> &crate::resource::Resources {
        &self.resources
    }

    /// Register a component type, for convenience at call sites that would
    /// otherwise reach through [`registry_mut`](Self::registry_mut).
    ///
    /// # Errors
    ///
    /// As [`TypeRegistry::register`].
    pub fn register<T: Reflect>(&mut self) -> Result<(), slop_reflect::RegistryError> {
        self.registry.register(T::type_info())
    }

    /// Register a component type described at runtime.
    ///
    /// The WASM guest path: a module's exported type table becomes components
    /// with no Rust type behind them (`docs/DESIGN.md` §2.4).
    ///
    /// # Errors
    ///
    /// As [`TypeRegistry::register`].
    pub fn register_info(&mut self, info: TypeInfo) -> Result<(), slop_reflect::RegistryError> {
        self.registry.register(info)
    }

    /// Check every invariant tying the three structures together.
    ///
    /// Debug-only. Called after every structural change in tests, because the
    /// failure mode here is not a crash — it is one entity reading another's
    /// components, which looks like a gameplay bug.
    #[cfg(debug_assertions)]
    pub fn assert_consistent(&self) {
        for archetype in &self.archetypes {
            archetype.assert_consistent();
        }

        self.resources.assert_consistent();

        let rows: usize = self.archetypes.iter().map(Archetype::len).sum();
        assert_eq!(
            rows,
            self.locations.len(),
            "every live entity must occupy exactly one row"
        );

        for (&entity, location) in &self.locations {
            let archetype = &self.archetypes[location.archetype];

            assert_eq!(
                archetype.entity_at(location.row),
                Some(entity),
                "entity {entity:?} claims a row that holds someone else"
            );
            assert!(
                self.entities.is_live(entity),
                "a dead entity still has a location"
            );
        }
    }
}

impl std::fmt::Debug for World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.locations.len())
            .field("archetypes", &self.archetypes.len())
            .field("types", &self.registry.len())
            .finish_non_exhaustive()
    }
}
