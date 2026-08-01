//! Foundational primitives shared by every other Slop crate.
//!
//! Arenas, generational-index slotmaps and handles (`DESIGN.md` §2.6), string
//! interning, the job system (§2.5), time and frame pacing, and tracing and
//! profiling markers. See `DESIGN.md` §4.
//!
//! # Job system
//!
//! M0 lands the scheduler's *API shape* backed by a plain thread pool; the
//! work-stealing implementation follows in M1, once ECS system scheduling
//! supplies real requirements. The API must not assume single-threaded
//! execution — that assumption is the part which becomes unfixable later.

mod alloc;
mod handle;
mod slotmap;

pub use alloc::HandleAllocator;
pub use handle::{Handle, RawHandle};
pub use slotmap::SlotMap;
