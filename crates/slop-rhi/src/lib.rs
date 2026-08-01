//! Render hardware interface — Vulkan backend via `ash`.
//!
//! See `docs/DESIGN.md` §2.2. The RHI is designed against the modern explicit model:
//! explicit barriers, timeline semaphores, bindless descriptor heaps, transient
//! resource aliasing, and multiple queues.
//!
//! # Scope at M0
//!
//! M0 deliberately ships *primitives, not abstraction* — device and queue
//! acquisition, swapchain, command buffers, synchronization, and a minimal
//! pipeline path sitting close to `ash`. The consumer-facing RHI API is
//! extracted at M3, when the render graph and frame renderer exist to define
//! what it needs to be.
//!
//! What M0 must get right is the feature model, because that is what cannot be
//! retrofitted:
//!
//! - Timeline semaphores, not fences plus binary semaphores
//! - Explicit barriers, never implicit synchronization
//! - A bindless descriptor heap allocated from the start
//! - Graphics, compute, and transfer queues acquired up front
//! - Physical device selection scoring on device type — the primary development
//!   machine also exposes an integrated GPU, so index 0 is not the discrete one
//!
//! This crate is the sanctioned home for `unsafe` (`docs/PLAN.md` §7); every block
//! carries a `// SAFETY:` comment, enforced by
//! `clippy::undocumented_unsafe_blocks`.

mod error;
mod instance;
mod physical;
mod queues;

pub use error::RhiError;
pub use instance::{Instance, InstanceConfig, Validation};
pub use physical::{DeviceInfo, DeviceKind, DeviceSelection, Rejection, enumerate, select};
pub use queues::QueueFamilies;

pub(crate) use instance::REQUIRED_API_VERSION;
