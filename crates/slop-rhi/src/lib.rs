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

mod command;
mod descriptor;
mod device;
mod error;
mod instance;
mod pipeline;
mod resource;
mod shader;
mod surface;
mod swapchain;
mod sync;

pub use command::{BufferState, CommandBuffer, CommandPool, ImageState, submit_and_wait};
pub use descriptor::{
    BindlessHeap, BindlessHeapConfig, HEAP_SET, SAMPLED_IMAGE_BINDING, SAMPLER_BINDING,
    STORAGE_IMAGE_BINDING, SampledImage, Sampler, StorageImage,
};
pub use device::{
    Device, DeviceInfo, DeviceKind, DeviceSelection, QueueFamilies, Queues, Rejection, enumerate,
    select,
};
pub use error::RhiError;
pub use instance::{Instance, InstanceConfig, Validation};
pub use pipeline::{
    Blend, DEPTH_CLEAR, DEPTH_COMPARE, GraphicsPipeline, GraphicsPipelineConfig, PipelineLayout,
    PipelineLayoutConfig, ShaderStage, VertexLayout,
};
pub use resource::{
    Allocator, AllocatorStats, Buffer, BufferConfig, Image, ImageConfig, MemoryLocation, aspect_of,
    preferred_depth_format,
};
pub use shader::ShaderModule;
pub use surface::{Surface, required_surface_extensions};
pub use swapchain::{AcquireOutcome, PresentMode, PresentOutcome, Swapchain, SwapchainConfig};
pub use sync::{BinarySemaphore, TimelineSemaphore};

/// Re-exported so consumers can name Vulkan types — extents, formats, handles —
/// without their own `ash` dependency, and so the engine cannot end up split
/// across two versions of it.
pub use ash::vk;

pub(crate) use instance::REQUIRED_API_VERSION;
