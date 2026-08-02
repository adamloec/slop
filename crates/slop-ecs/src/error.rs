//! What the ECS can refuse to do.

use slop_reflect::TypeId;
use thiserror::Error;

/// Anything the entity component system can fail at.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EcsError {
    /// A component type is not in the registry.
    ///
    /// Not deferrable: a column allocates by the type's layout and frees each
    /// element through its destructor, and neither is knowable without a
    /// [`TypeInfo`](slop_reflect::TypeInfo). Reported by id rather than by name
    /// because the registry is precisely what would have supplied the name.
    #[error("component type {type_id} is not registered")]
    UnregisteredComponent {
        /// The type that could not be resolved.
        type_id: TypeId,
    },

    /// A resource type is not in the registry.
    ///
    /// Separate from [`UnregisteredComponent`](Self::UnregisteredComponent)
    /// because the two are separate namespaces — a component `Time` and a
    /// resource `Time` are different things — and an error that said the wrong
    /// one would send the reader to the wrong registration call.
    #[error("resource type {type_id} is not registered")]
    UnregisteredResource {
        /// The type that could not be resolved.
        type_id: TypeId,
    },

    /// A value and the type it was to be written as do not agree.
    ///
    /// From `docs/DESIGN.md` §5's round trip and from scene loading: a file
    /// naming a field the type does not have, or holding a number too large for
    /// it. Reported rather than coerced, because a save that loads wrong is
    /// worse than one that refuses to load.
    #[error(transparent)]
    Value(#[from] crate::ValueError),

    /// The entity is alive but does not hold the component asked for.
    #[error("entity {entity:?} has no component {type_id}")]
    MissingComponent {
        /// The entity that was asked.
        entity: crate::Entity,
        /// The component it does not have.
        type_id: TypeId,
    },

    /// The entity is not alive.
    ///
    /// Either never spawned, or despawned since the handle was issued —
    /// `Handle`'s generation distinguishes a stale handle from a fresh one
    /// reusing the same slot, so this is never a case of silently addressing
    /// the wrong entity.
    ///
    /// An error rather than a panic on the paths that return `Result`, per
    /// `docs/PLAN.md` §4.1-C: holding a handle to something already destroyed
    /// is routine in an editor and during hot reload.
    #[error("entity {entity:?} is not alive")]
    NoSuchEntity {
        /// The handle that did not resolve.
        entity: crate::Entity,
    },
}
