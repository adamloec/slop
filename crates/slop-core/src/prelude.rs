//! The types a caller of `slop-core` cannot avoid.
//!
//! Types only, never functions, and deliberately not everything in the crate —
//! see `docs/CONVENTIONS.md` §2.5. Anything used once at startup or reached for
//! rarely is imported by its full path instead, so that `use
//! slop_core::prelude::*` stays a statement about what this crate is *for*.
//!
//! Excluded on purpose:
//!
//! - [`RawHandle`](crate::RawHandle) — only appears at ABI boundaries
//! - [`Clock`](crate::Clock) — constructed once by an application
//! - [`Scope`](crate::Scope) — inferred as a closure parameter, rarely named
//! - [`diagnostics`](crate::diagnostics) — a namespace, and a decision an
//!   application makes explicitly

pub use crate::{FixedTimestep, FrameArena, Handle, HandleAllocator, JobSystem, SlotMap};
