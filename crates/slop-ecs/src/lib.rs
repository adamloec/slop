//! The entity component system — `docs/DESIGN.md` §2.10.
//!
//! Structurally an in-memory database. An *entity* is an id and nothing more; a
//! *component* is plain data attached to one; a *system* is a function over
//! every entity holding a given set of components. Entities are rows, components
//! are columns, systems are queries.
//!
//! # Archetype storage, and the constraint that forced it
//!
//! Entities are grouped by their exact component set, so all entities with
//! `{Position, Velocity, Mesh}` share one table of parallel arrays. Queries walk
//! contiguous memory; adding or removing a component physically moves an
//! entity's data between tables.
//!
//! §2.10 gives three reasons, and the second is the binding one: **§2.3's WASM
//! boundary requires handing guest modules contiguous columns of component
//! data.** Archetype storage produces that natively. Sparse-set storage would
//! need a gather into a temporary buffer every frame — exactly the per-frame
//! cost the columnar ABI exists to avoid.
//!
//! That is why §2.10 insists archetype storage and the columnar boundary be
//! designed *together* rather than in sequence. [`Column`] is where they meet:
//! the same array that makes a query a linear scan is the one handed across the
//! boundary, and it is handed across only when
//! [`Transfer::Blittable`](slop_reflect::Transfer::Blittable) says its bytes
//! mean something outside this address space.
//!
//! # Components are reflected, always
//!
//! Every component type must be described by a
//! [`TypeInfo`](slop_reflect::TypeInfo). Not a convenience — a column cannot be
//! allocated without a layout or freed without a destructor, and §2.4 requires
//! both to arrive as data so that a WASM guest's own types are first-class
//! components rather than a second tier.
//!
//! The consequence worth stating plainly: there is no "unregistered component".
//! A type the editor cannot inspect and the serializer cannot write would be a
//! component that silently vanishes from a save file.

mod archetype;
mod column;
mod error;
mod signature;

pub use archetype::{Archetype, EntityTag, Row};
pub use column::Column;
pub use error::EcsError;
pub use signature::Signature;

/// An entity: an id, and nothing else.
///
/// `docs/PLAN.md` §4.1-C built `HandleAllocator` for exactly this — generation
/// bookkeeping with no payload, because an entity's component data lives in
/// archetype columns rather than in one array. A despawned entity's handle stops
/// resolving immediately, rather than when its slot is next reused.
pub type Entity = slop_core::Handle<EntityTag>;
