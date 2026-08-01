//! Data the world holds exactly one of.
//!
//! An entity component system stores what there are many of. An engine also has
//! things there is precisely one of — the clock, the input state, the asset
//! registry, the active camera — and they need a home that the scheduler
//! understands, or systems touching them cannot be parallelized safely.
//!
//! # Why not a component on a singleton entity
//!
//! It would work, and it would need no new machinery at all: access is already
//! per component type, so two systems using different resources would already
//! not conflict. It was rejected because the singleton leaks. It would be
//! counted by [`World::len`](crate::World::len), yielded by
//! `query::<Entity>()`, and visible to every system that iterates entities — so
//! every one of those would need a caveat, forever, to hide something that is
//! not an entity in the first place.
//!
//! # Why the storage is a [`Column`]
//!
//! A resource is one value of a reflected type, which is a column of length one.
//! Reusing it is not a trick: it is exactly the same problem the ECS already
//! solved, and it brings layout, destructors, and change-detection stamps along
//! without a second implementation to keep in step. A resource is
//! [`Changed`](crate::Changed)-detectable for free, because the stamp lives
//! where every other stamp lives.
//!
//! # What the scheduler sees
//!
//! [`AccessKind::Resource`](crate::AccessKind::Resource) alongside
//! `AccessKind::Component`, in the same [`Access`](crate::Access) list. Two
//! systems writing the same resource conflict and are put in different batches;
//! two writing different resources do not. Nothing in the scheduler needed to
//! change to make that true, which was the point of giving `Access` a kind
//! before the scheduler hardened rather than after.

use slop_reflect::{Reflect, TypeId, TypeInfo};

use slop_core::FxHashMap;

use crate::{Column, EcsError, ElementTicks, Tick};

/// Every resource the world holds, keyed by type.
///
/// Each [`Column`] holds either nothing or exactly one element.
#[derive(Debug, Default)]
pub(crate) struct Resources {
    columns: FxHashMap<TypeId, Column>,
}

impl Resources {
    /// How many resources are present.
    pub(crate) fn len(&self) -> usize {
        self.columns
            .values()
            .filter(|column| !column.is_empty())
            .count()
    }

    /// Whether a resource of this type is present.
    pub(crate) fn contains(&self, type_id: TypeId) -> bool {
        self.columns
            .get(&type_id)
            .is_some_and(|column| !column.is_empty())
    }

    /// The column holding `type_id`, if a value is present.
    pub(crate) fn column(&self, type_id: TypeId) -> Option<&Column> {
        self.columns
            .get(&type_id)
            .filter(|column| !column.is_empty())
    }

    /// Install a value, replacing and dropping any previous one.
    ///
    /// # Safety
    ///
    /// `value` must point at an initialized, properly aligned value of exactly
    /// the type `info` describes, and is moved out of.
    pub(crate) unsafe fn insert(&mut self, info: &TypeInfo, value: *const u8, tick: Tick) {
        let column = self
            .columns
            .entry(info.id())
            .or_insert_with(|| Column::new(info));

        if column.is_empty() {
            // SAFETY: the caller's guarantee about `value`, and the column was
            // built from this exact `TypeInfo`.
            unsafe { column.push(value, tick) };
        } else {
            // Replacing rather than pushing keeps the length at one, and drops
            // what was there. The added-stamp survives, matching a component:
            // overwriting a resource is not gaining one.
            //
            // SAFETY: as above, and index 0 is occupied.
            unsafe { column.replace(0, value, tick) };
        }
    }

    /// A pointer to the value, if present.
    pub(crate) fn get(&self, type_id: TypeId) -> Option<*const u8> {
        self.column(type_id)?.get(0)
    }

    /// A mutable pointer to the value, stamping it changed.
    pub(crate) fn get_mut(&mut self, type_id: TypeId, tick: Tick) -> Option<*mut u8> {
        let column = self
            .columns
            .get_mut(&type_id)
            .filter(|column| !column.is_empty())?;

        column.mark_changed(0, tick);

        column.get_mut(0)
    }

    /// Drop the value, if there is one. Returns whether there was.
    ///
    /// The column stays, empty, so a resource that is removed and reinstalled
    /// does not churn an allocation.
    pub(crate) fn remove(&mut self, type_id: TypeId) -> bool {
        self.columns
            .get_mut(&type_id)
            .is_some_and(|column| column.swap_remove(0))
    }

    /// When the value was added and last changed.
    pub(crate) fn ticks(&self, type_id: TypeId) -> Option<ElementTicks> {
        self.column(type_id)?.ticks(0)
    }

    /// Every resource type currently holding a value, in a defined order.
    pub(crate) fn type_ids(&self) -> Vec<TypeId> {
        let mut ids: Vec<TypeId> = self
            .columns
            .iter()
            .filter(|(_, column)| !column.is_empty())
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();

        ids
    }

    /// Check that no column holds more than one value.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_consistent(&self) {
        for column in self.columns.values() {
            assert!(
                column.len() <= 1,
                "a resource column holds {} values; there is exactly one of a resource",
                column.len()
            );
            column.assert_consistent();
        }
    }
}

/// Resolve `T`'s registration, or say which type was missing.
pub(crate) fn require_registered<T: Reflect>(
    registry: &slop_reflect::TypeRegistry,
) -> Result<&TypeInfo, EcsError> {
    registry
        .get(T::type_id())
        .ok_or(EcsError::UnregisteredResource {
            type_id: T::type_id(),
        })
}
