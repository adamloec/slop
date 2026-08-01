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
