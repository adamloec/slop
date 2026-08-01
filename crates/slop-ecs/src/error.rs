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
}
