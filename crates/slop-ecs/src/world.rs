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
use slop_reflect::{Reflect, TypeId, TypeInfo, TypeRegistry};

use crate::query::{Query, QueryData, ReadOnlyQueryData};
use crate::{Archetype, EcsError, Entity, EntityTag, Row, Signature};

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
        }
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
        Query::new(&self.archetypes)
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
        Query::new(&self.archetypes)
    }

    /// Create an entity with no components.
    pub fn spawn(&mut self) -> Entity {
        let entity = self.entities.allocate();

        // SAFETY: the empty archetype has no columns, so there are no slots to
        // initialize.
        let (row, slots) = unsafe { self.archetypes[0].begin_row(entity) };
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
    pub fn get_mut<T: Reflect>(&mut self, entity: Entity) -> Option<&mut T> {
        let location = *self.locations.get(&entity)?;
        let column = self.archetypes[location.archetype].column_mut(T::type_id())?;
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
        let type_id = T::type_id();

        if !self.registry.contains(type_id) {
            return Err(EcsError::UnregisteredComponent { type_id });
        }

        let location = *self
            .locations
            .get(&entity)
            .ok_or(EcsError::NoSuchEntity { entity })?;

        // Already present: overwrite in place. No table changes, so none of the
        // migration machinery runs.
        if let Some(existing) = self.get_mut::<T>(entity) {
            *existing = component;
            return Ok(());
        }

        let destination = self.archetypes[location.archetype]
            .signature()
            .with(type_id)
            .expect("the component is absent, so the signature must grow");
        let destination = self.archetype_index(&destination)?;

        // SAFETY: every slot returned below is written exactly once — the
        // shared components by `move_row_out`, and the new component by the
        // explicit write. `component` is forgotten afterward so its destructor
        // does not also run.
        unsafe {
            let component = std::mem::ManuallyDrop::new(component);

            let (row, slots) = self.archetypes[destination].begin_row(entity);
            let shared = self.relocate_into(location, destination, &slots, Some(type_id));

            let slot = self.slot_for(destination, &slots, type_id);
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&*component).cast::<u8>(),
                slot,
                size_of::<T>(),
            );

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
        let type_id = T::type_id();

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
            let (row, slots) = self.archetypes[destination].begin_row(entity);
            let shared = self.relocate_into(location, destination, &slots, None);

            self.finish_move(entity, location, destination, row, shared);
        }

        true
    }

    /// Move every component the destination wants out of the source row.
    ///
    /// Components the destination lacks are dropped. `skip` names a type the
    /// destination has but the source does not, which the caller writes itself.
    ///
    /// Returns the entity the source's swap-remove moved into the vacated row.
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
    ) -> Option<Entity> {
        let source_types: Vec<TypeId> = self.archetypes[location.archetype]
            .signature()
            .types()
            .to_vec();
        let destination_signature = self.archetypes[destination].signature().clone();

        for &type_id in &source_types {
            let column = self.archetypes[location.archetype]
                .column_mut(type_id)
                .expect("the type came from this archetype's own signature");

            match destination_signature.position(type_id) {
                // Shared: relocate the bytes, no destructor.
                Some(index) => {
                    // SAFETY: `slots[index]` is the destination column for this
                    // exact type, uninitialized and correctly aligned.
                    unsafe { column.swap_remove_to(location.row.0, slots[index]) };
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
        self.archetypes[location.archetype].take_row(location.row)
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
