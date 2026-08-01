//! Foundational primitives shared by every other Slop crate.
//!
//! Arenas, generational-index slotmaps and handles (`docs/DESIGN.md` §2.6), string
//! interning, the job system (§2.5), time and frame pacing, and tracing and
//! profiling markers. See `docs/DESIGN.md` §4.
//!
//! # Job system
//!
//! M0 lands the scheduler's *API shape* backed by a plain thread pool; the
//! work-stealing implementation follows in M1, once ECS system scheduling
//! supplies real requirements. The API must not assume single-threaded
//! execution — that assumption is the part which becomes unfixable later.

mod alloc;
mod arena;
mod handle;
mod jobs;
mod slotmap;
mod time;

pub use alloc::HandleAllocator;
pub use arena::FrameArena;
pub use handle::{Handle, RawHandle};
pub use jobs::{JobSystem, Scope};
pub use slotmap::SlotMap;
pub use time::{Clock, FixedTimestep};
