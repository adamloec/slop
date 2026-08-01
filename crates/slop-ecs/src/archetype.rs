//! One table: every entity holding exactly one component set.
//!
//! Columns are parallel arrays, and **row `n` is one entity across all of
//! them**. That invariant is the whole point of the structure — it is what makes
//! a query a linear scan, and what lets `docs/DESIGN.md` §2.3 hand a guest
//! module a set of columns it can iterate in lockstep without an index.
//!
//! Keeping it is this module's entire job, and the reason rows are added and
//! removed as whole rows rather than column by column: a half-populated row is
//! not a recoverable state, it is a column whose element `n` is uninitialized
//! while its length says otherwise.

use slop_reflect::{TypeId, TypeRegistry};

use crate::{Column, EcsError, ElementTicks, Entity, Signature, Tick};

/// A position within an archetype's columns.
///
/// Not stable: removing any row may move another entity into its place, which
/// is what makes removal O(1). The world's entity index is what stays
/// authoritative, and a `Row` is only meaningful alongside the archetype it came
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row(pub usize);

/// Every entity with one exact component set, stored column-wise.
///
/// # Invariants
///
/// 1. `entities.len()` equals every column's `len()`.
/// 2. `columns` is parallel to `signature.types()` — column `i` holds the
///    component type `signature.types()[i]`.
/// 3. Row `n` of every column belongs to `entities[n]`.
pub struct Archetype {
    signature: Signature,
    columns: Vec<Column>,
    entities: Vec<Entity>,
}

impl Archetype {
    /// Build an empty archetype for `signature`.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredComponent`] if any type in the signature is not
    /// in the registry. A column cannot be allocated without a layout, so this
    /// is not a check that could be deferred.
    pub fn new(signature: Signature, registry: &TypeRegistry) -> Result<Self, EcsError> {
        let columns = signature
            .types()
            .iter()
            .map(|&type_id| {
                registry
                    .get(type_id)
                    .map(Column::new)
                    .ok_or(EcsError::UnregisteredComponent { type_id })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            signature,
            columns,
            entities: Vec::new(),
        })
    }

    /// The component set this archetype holds.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// How many entities are stored.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the archetype holds no entities.
    ///
    /// Empty archetypes are kept rather than reclaimed: an entity set that
    /// oscillates across a boundary — a component added and removed every frame
    /// — would otherwise pay a table rebuild each time.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// The entities, indexed by row.
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// The entity at `row`.
    pub fn entity_at(&self, row: Row) -> Option<Entity> {
        self.entities.get(row.0).copied()
    }

    /// The column holding `type_id`.
    pub fn column(&self, type_id: TypeId) -> Option<&Column> {
        self.signature
            .position(type_id)
            .and_then(|index| self.columns.get(index))
    }

    /// The column holding `type_id`, mutably.
    pub fn column_mut(&mut self, type_id: TypeId) -> Option<&mut Column> {
        self.signature
            .position(type_id)
            .and_then(|index| self.columns.get_mut(index))
    }

    /// Every column, in signature order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Begin a row for `entity`, returning uninitialized slots to fill.
    ///
    /// Returns one slot per column, in signature order. Every one must be
    /// written before the archetype is read, dropped, or added to again.
    ///
    /// This shape rather than a value-taking `push` because the two callers
    /// supply components differently: spawning writes owned values, while
    /// migration relocates bytes out of another archetype's columns. Both need
    /// somewhere to write, and neither has a uniform typed value to hand over.
    ///
    /// Every component in the row is stamped added-and-changed at `tick`. A
    /// migration that wants to preserve the stamps its components arrived with
    /// overwrites them afterwards through [`Column::set_ticks`].
    ///
    /// # Safety
    ///
    /// The caller must initialize **every** returned slot with a valid value of
    /// that column's component type, before doing anything else with this
    /// archetype. Leaving one unwritten breaks invariant 2 of [`Column`], which
    /// is undefined behaviour the next time the column is read or dropped.
    ///
    /// [`Column`]: crate::Column
    /// [`Column::set_ticks`]: crate::Column::set_ticks
    pub unsafe fn begin_row(&mut self, entity: Entity, tick: Tick) -> (Row, Vec<*mut u8>) {
        let row = Row(self.entities.len());
        self.entities.push(entity);

        let slots = self
            .columns
            .iter_mut()
            // SAFETY: the caller undertakes to initialize every slot, which is
            // exactly `push_uninit`'s obligation passed along.
            .map(|column| unsafe { column.push_uninit(tick) })
            .collect();

        (row, slots)
    }

    /// Overwrite one row's change-detection stamps, column by column.
    ///
    /// `ticks` is parallel to [`columns`](Self::columns), as
    /// [`begin_row`](Self::begin_row)'s slots are. Used by migration, which
    /// relocates components rather than writing them and so must not report them
    /// as changed.
    pub fn set_row_ticks(&mut self, row: Row, ticks: &[Option<ElementTicks>]) {
        debug_assert_eq!(
            ticks.len(),
            self.columns.len(),
            "one entry per column is required"
        );

        for (column, ticks) in self.columns.iter_mut().zip(ticks) {
            if let Some(ticks) = *ticks {
                column.set_ticks(row.0, ticks);
            }
        }
    }

    /// Remove `row`, dropping its components.
    ///
    /// Returns the entity that was moved into `row`, if any — the caller must
    /// update its index for that entity. Returns `None` when `row` was last, or
    /// out of bounds.
    ///
    /// Swap-remove rather than shift, so removal is O(components) rather than
    /// O(entities). The cost is that one other entity's row number changes,
    /// which is why the moved entity is reported rather than left to be
    /// discovered.
    pub fn remove_row(&mut self, row: Row) -> Option<Entity> {
        if row.0 >= self.entities.len() {
            return None;
        }

        for column in &mut self.columns {
            column.swap_remove(row.0);
        }

        self.entities.swap_remove(row.0);

        // `swap_remove` on the entity list moved the last entity into `row`,
        // unless `row` *was* the last.
        self.entities.get(row.0).copied()
    }

    /// Remove `row` without dropping its components, writing each into `out`.
    ///
    /// The migration source. `out` must hold one destination pointer per column
    /// in signature order — which is exactly what [`begin_row`](Self::begin_row)
    /// returns for an archetype whose signature is a superset.
    ///
    /// Returns the entity moved into `row`, as [`remove_row`](Self::remove_row).
    ///
    /// # Safety
    ///
    /// `out` must have one entry per column, each pointing at writable,
    /// correctly aligned space for that column's component type. Ownership of
    /// each value transfers to the caller.
    pub unsafe fn move_row_out(&mut self, row: Row, out: &[*mut u8]) -> Option<Entity> {
        debug_assert_eq!(
            out.len(),
            self.columns.len(),
            "one destination per column is required"
        );

        if row.0 >= self.entities.len() {
            return None;
        }

        for (column, &destination) in self.columns.iter_mut().zip(out) {
            // SAFETY: `row` is in bounds, and the caller guarantees each
            // destination is valid space for that column's type.
            unsafe { column.swap_remove_to(row.0, destination) };
        }

        self.entities.swap_remove(row.0);

        self.entities.get(row.0).copied()
    }

    /// Drop `row` from the entity list only, leaving the columns alone.
    ///
    /// Returns the entity swapped into `row`, as [`remove_row`](Self::remove_row).
    ///
    /// The other half of a migration whose columns the caller has already
    /// emptied one at a time. Splitting it out is what lets a move relocate some
    /// components and drop others in one pass, which
    /// [`move_row_out`](Self::move_row_out) cannot express because it does the
    /// same thing to every column.
    ///
    /// `pub(crate)` because calling it without having emptied the columns leaves
    /// invariant 1 broken — the entity list would be one shorter than every
    /// column, and the next row's components would belong to the wrong entity.
    pub(crate) fn take_row(&mut self, row: Row) -> Option<Entity> {
        if row.0 >= self.entities.len() {
            return None;
        }

        self.entities.swap_remove(row.0);

        self.entities.get(row.0).copied()
    }

    /// Drop every entity's components and forget every entity.
    pub fn clear(&mut self) {
        for column in &mut self.columns {
            column.clear();
        }

        self.entities.clear();
    }

    /// Check invariant 1 — that every column agrees with the entity count.
    ///
    /// Debug-only, and called after every structural change in tests. A column
    /// out of step with the others is the failure this whole module exists to
    /// prevent, and it presents as reading another entity's component rather
    /// than as a crash.
    #[cfg(debug_assertions)]
    pub fn assert_consistent(&self) {
        for (index, column) in self.columns.iter().enumerate() {
            assert_eq!(
                column.len(),
                self.entities.len(),
                "column {index} holds {} elements but the archetype has {} entities",
                column.len(),
                self.entities.len(),
            );
            column.assert_consistent();
        }

        assert_eq!(
            self.columns.len(),
            self.signature.len(),
            "columns must be parallel to the signature"
        );
    }
}

impl std::fmt::Debug for Archetype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archetype")
            .field("components", &self.signature.len())
            .field("entities", &self.entities.len())
            .finish_non_exhaustive()
    }
}

/// The tag distinguishing an entity handle from every other handle.
///
/// Never constructed. `docs/DESIGN.md` §2.6's `Handle<T>` carries a
/// `PhantomData<fn() -> T>`, so an uninhabited type is enough to make
/// `Handle<EntityTag>` a distinct type from `Handle<Texture>` at compile time.
#[derive(Debug)]
pub enum EntityTag {}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_reflect::{Reflect, register_builtins};

    #[derive(slop_reflect::Reflect, Debug, Clone, Copy, PartialEq)]
    #[repr(C)]
    struct Position {
        x: f32,
        y: f32,
    }

    #[derive(slop_reflect::Reflect, Debug, Clone, Copy, PartialEq)]
    #[repr(C)]
    struct Health {
        value: u32,
    }

    fn registry() -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        register_builtins(&mut registry).expect("fresh");
        registry.register_native::<Position>().expect("fresh");
        registry.register_native::<Health>().expect("fresh");

        registry
    }

    fn entity(index: u32) -> Entity {
        let mut allocator = slop_core::HandleAllocator::<EntityTag>::new();
        let mut handle = allocator.allocate();

        for _ in 0..index {
            handle = allocator.allocate();
        }

        handle
    }

    /// Spawn one row, writing both components.
    fn spawn(archetype: &mut Archetype, entity: Entity, position: Position, health: Health) -> Row {
        // SAFETY: every slot is written immediately below, in signature order,
        // with a value of that column's type.
        unsafe {
            let (row, slots) = archetype.begin_row(entity, Tick::new(1));

            for (&slot, &type_id) in slots.iter().zip(archetype.signature().types()) {
                if type_id == Position::type_id() {
                    slot.cast::<Position>().write(position);
                } else if type_id == Health::type_id() {
                    slot.cast::<Health>().write(health);
                } else {
                    unreachable!("this archetype holds only Position and Health");
                }
            }

            row
        }
    }

    fn read<T: Copy + Reflect>(archetype: &Archetype, row: Row) -> Option<T> {
        let column = archetype.column(T::type_id())?;

        // SAFETY: the column was built from `T`'s `TypeInfo`, and `get`
        // returns a pointer to an initialized element or `None`.
        column
            .get(row.0)
            .map(|pointer| unsafe { *pointer.cast::<T>() })
    }

    fn two_component_archetype() -> Archetype {
        let signature = Signature::new([Position::type_id(), Health::type_id()]);

        Archetype::new(signature, &registry()).expect("both types are registered")
    }

    #[test]
    fn an_unregistered_component_is_refused_rather_than_guessed() {
        // A column cannot be allocated without a layout, so this cannot be
        // deferred to first use.
        let signature = Signature::new([TypeId::from_path("game::NeverRegistered")]);

        assert!(matches!(
            Archetype::new(signature, &registry()),
            Err(EcsError::UnregisteredComponent { .. })
        ));
    }

    #[test]
    fn a_fresh_archetype_has_a_column_per_component_and_no_rows() {
        let archetype = two_component_archetype();

        assert_eq!(archetype.columns().len(), 2);
        assert!(archetype.is_empty());
        assert!(archetype.column(Position::type_id()).is_some());
        assert!(archetype.column(Health::type_id()).is_some());
        assert!(
            archetype.column(u32::type_id()).is_none(),
            "a type outside the signature has no column"
        );
        archetype.assert_consistent();
    }

    #[test]
    fn a_spawned_row_holds_both_components() {
        let mut archetype = two_component_archetype();
        let row = spawn(
            &mut archetype,
            entity(0),
            Position { x: 1.0, y: 2.0 },
            Health { value: 100 },
        );

        assert_eq!(archetype.len(), 1);
        assert_eq!(
            read::<Position>(&archetype, row),
            Some(Position { x: 1.0, y: 2.0 })
        );
        assert_eq!(read::<Health>(&archetype, row), Some(Health { value: 100 }));
        archetype.assert_consistent();
    }

    #[test]
    fn rows_stay_in_lockstep_across_columns() {
        // Invariant 3, and the reason the structure works at all. If the
        // columns drifted, row 5 of Position would belong to a different entity
        // than row 5 of Health — which reads as a gameplay bug, not a crash.
        let mut archetype = two_component_archetype();

        for index in 0..50_u32 {
            spawn(
                &mut archetype,
                entity(index),
                Position {
                    x: index as f32,
                    y: -(index as f32),
                },
                Health { value: index * 10 },
            );
        }

        archetype.assert_consistent();

        for index in 0..50_usize {
            let position = read::<Position>(&archetype, Row(index)).expect("in bounds");
            let health = read::<Health>(&archetype, Row(index)).expect("in bounds");

            assert_eq!(position.x, index as f32);
            assert_eq!(health.value, index as u32 * 10, "row {index} drifted");
        }
    }

    #[test]
    fn removing_a_row_reports_the_entity_that_moved() {
        // The caller has to update its index for the moved entity, so it has to
        // be told which one moved. Discovering it later is not possible — the
        // row number is the only link.
        let mut archetype = two_component_archetype();

        let first = entity(0);
        let second = entity(1);
        let third = entity(2);

        for (index, id) in [first, second, third].into_iter().enumerate() {
            spawn(
                &mut archetype,
                id,
                Position {
                    x: index as f32,
                    y: 0.0,
                },
                Health {
                    value: index as u32,
                },
            );
        }

        let moved = archetype.remove_row(Row(0));

        assert_eq!(moved, Some(third), "the last entity filled the hole");
        assert_eq!(archetype.len(), 2);
        assert_eq!(archetype.entity_at(Row(0)), Some(third));
        // And its components moved with it.
        assert_eq!(
            read::<Health>(&archetype, Row(0)),
            Some(Health { value: 2 })
        );
        archetype.assert_consistent();
    }

    #[test]
    fn removing_the_last_row_moves_nothing() {
        let mut archetype = two_component_archetype();
        spawn(
            &mut archetype,
            entity(0),
            Position { x: 0.0, y: 0.0 },
            Health { value: 1 },
        );

        assert_eq!(archetype.remove_row(Row(0)), None, "nothing moved");
        assert!(archetype.is_empty());
        archetype.assert_consistent();
    }

    #[test]
    fn removing_out_of_bounds_is_none_and_changes_nothing() {
        let mut archetype = two_component_archetype();
        spawn(
            &mut archetype,
            entity(0),
            Position { x: 0.0, y: 0.0 },
            Health { value: 1 },
        );

        assert_eq!(archetype.remove_row(Row(9)), None);
        assert_eq!(archetype.len(), 1, "the archetype is untouched");
        archetype.assert_consistent();
    }

    #[test]
    fn a_row_can_be_moved_out_without_being_dropped() {
        // The migration path. Adding a component moves an entity to a different
        // archetype, and its existing components must be relocated rather than
        // destroyed and rebuilt.
        let mut source = two_component_archetype();
        spawn(
            &mut source,
            entity(0),
            Position { x: 7.0, y: 8.0 },
            Health { value: 42 },
        );

        let mut destination = two_component_archetype();

        // SAFETY: `begin_row` returns one slot per column in signature order,
        // both archetypes have the same signature, and `move_row_out` writes
        // every slot with a value of that column's type.
        unsafe {
            let (row, slots) = destination.begin_row(entity(0), Tick::new(1));
            source.move_row_out(Row(0), &slots);

            assert_eq!(
                read::<Position>(&destination, row),
                Some(Position { x: 7.0, y: 8.0 })
            );
            assert_eq!(
                read::<Health>(&destination, row),
                Some(Health { value: 42 })
            );
        }

        assert!(source.is_empty());
        source.assert_consistent();
        destination.assert_consistent();
    }

    #[test]
    fn an_empty_signature_archetype_holds_entities_with_no_components() {
        // A real archetype rather than a special case: an entity that exists
        // but holds nothing still has to be somewhere.
        let mut archetype =
            Archetype::new(Signature::empty(), &registry()).expect("no types to resolve");

        // SAFETY: there are no slots to initialize.
        let (row, slots) = unsafe { archetype.begin_row(entity(0), Tick::new(1)) };

        assert!(slots.is_empty());
        assert_eq!(row, Row(0));
        assert_eq!(archetype.len(), 1);
        assert_eq!(archetype.entity_at(Row(0)), Some(entity(0)));
        archetype.assert_consistent();
    }

    #[test]
    fn clearing_empties_every_column_together() {
        let mut archetype = two_component_archetype();

        for index in 0..10_u32 {
            spawn(
                &mut archetype,
                entity(index),
                Position { x: 0.0, y: 0.0 },
                Health { value: index },
            );
        }

        archetype.clear();

        assert!(archetype.is_empty());
        assert_eq!(archetype.entities(), &[]);
        archetype.assert_consistent();
    }

    #[test]
    fn columns_are_parallel_to_the_signature() {
        // Invariant 2. Column lookup is a binary search on the signature and an
        // index into the column list, so a mismatch would return another
        // component's storage.
        let archetype = two_component_archetype();

        for (index, &type_id) in archetype.signature().types().iter().enumerate() {
            assert_eq!(archetype.columns()[index].type_id(), type_id);
        }
    }
}
