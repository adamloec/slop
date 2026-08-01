//! Foundational primitives shared by every other Slop crate.
//!
//! Identity without pointers, memory without per-frame allocation, and time
//! without wall-clock nondeterminism. Nothing here knows what a mesh, an entity,
//! or a GPU is. See `docs/DESIGN.md` §4 and `docs/slop-core/README.md`.
//!
//! Three of these exist because of `docs/DESIGN.md` §2.14: [`Rng`],
//! [`FxHashMap`], and the caller-side contract in the `jobs` module docs. Each
//! replaces a `std` default whose behaviour varies per run or per platform, and
//! each is easy to bypass by accident — which is why `clippy.toml` disallows the
//! alternatives rather than leaving it to review.
//!
//! | Module | Owns |
//! |---|---|
//! | [`handle`](Handle) | Generational handles — `docs/DESIGN.md` §2.6 |
//! | [`SlotMap`] | Generational storage that owns its values |
//! | [`HandleAllocator`] | Generation bookkeeping with no payload, for the ECS |
//! | [`FrameArena`] | Fixed-capacity bump allocator, reset per frame |
//! | [`FixedTimestep`] | Fixed-step accumulation — `docs/DESIGN.md` §2.7 |
//! | [`JobSystem`] | Task dispatch — `docs/DESIGN.md` §2.5 |
//! | [`Rng`] | Seeded pseudorandom numbers — `docs/DESIGN.md` §2.14 |
//! | [`FxHashMap`] | Hash containers with reproducible iteration — `docs/DESIGN.md` §2.14 |
//! | [`diagnostics`] | Structured logging — `docs/CONVENTIONS.md` §13 |
//!
//! # Two implementations here are provisional
//!
//! Both are `docs/DESIGN.md` §1.2 principle 6 applied deliberately — the seam is
//! final, the implementation behind it is not. Callers do not change when either
//! is replaced.
//!
//! - **[`JobSystem`] is backed by [`std::thread::scope`]**, which spawns OS
//!   threads per call. The work-stealing pool lands at M1, once ECS system
//!   scheduling supplies real requirements. Do not optimize against the current
//!   dispatch cost.
//! - **[`HandleAllocator`] tracks liveness in a `Vec<bool>`.** A bitset is
//!   eight times denser and live-entity iteration is a hot ECS path, but that is
//!   entirely behind the API.

mod alloc;
mod arena;
mod handle;
mod hash;
mod jobs;
mod rng;
mod slotmap;
mod time;

/// Structured logging and profiling markers.
///
/// A public module rather than flat re-exports, unlike everything else in this
/// crate: `diagnostics::init()` reads better than a bare `init()`, and the
/// module namespaces a `tracing` re-export alongside it.
pub mod diagnostics;

pub mod prelude;

pub use alloc::HandleAllocator;
pub use arena::FrameArena;
pub use handle::{Handle, RawHandle};
pub use hash::{FxHashMap, FxHashSet, FxHasher};
pub use jobs::{JobSystem, Scope};
pub use rng::Rng;
pub use slotmap::SlotMap;
pub use time::{Clock, FixedTimestep};
