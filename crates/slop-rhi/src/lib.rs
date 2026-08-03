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
mod compute;
mod descriptor;
mod device;
mod error;
mod format;
mod geometry;
/// Public for its module documentation, not for its contents — every type in it
/// is re-exported at the crate root and that is how callers should name them.
///
/// Seven `to_vk` methods across this crate cite it as "the escape hatch", and
/// what they are citing is the module's own explanation of why a named escape
/// hatch beats callers taking their own `ash` dependency. A private module makes
/// those links dangle, which rustdoc reports and `-D warnings` rejects.
pub mod handle;
mod instance;
mod pass;
mod pipeline;
mod resource;
mod sampler;
mod shader;
mod surface;
mod swapchain;
mod sync;
mod usage;

pub use command::{
    BufferState, CommandBuffer, CommandPool, ImageState, Stage, Submission, WaitStage,
    submit_and_wait, submit_recorded_and_wait,
};
pub use compute::{Compute, ComputePipeline, workgroups};
pub use descriptor::{
    BindlessHeap, BindlessHeapConfig, HEAP_SET, SAMPLED_IMAGE_BINDING, SAMPLER_BINDING,
    STORAGE_BUFFER_BINDING, STORAGE_IMAGE_BINDING, SampledImage, Sampler, StorageBuffer,
    StorageImage,
};
pub use device::{
    Device, DeviceInfo, DeviceKind, DeviceSelection, QueueFamilies, Queues, Rejection, enumerate,
    select,
};
pub use error::RhiError;
pub use format::{Format, ImageAspect, aspect_of};
pub use geometry::{Extent2D, Offset2D, Rect2D};
pub use handle::{
    BufferHandle, ImageHandle, ImageViewHandle, QueueHandle, SamplerHandle, SemaphoreHandle,
};
pub use instance::{Instance, InstanceConfig, Validation};
pub use pass::{Attachments, ClearValue, ColorAttachment, DepthAttachment, Load, Pass};
pub use pipeline::{
    Blend, DEPTH_CLEAR, DEPTH_COMPARE, GraphicsPipeline, GraphicsPipelineConfig, PipelineLayout,
    PipelineLayoutConfig, ShaderStage, VertexLayout,
};
pub use resource::{
    Allocator, AllocatorStats, Buffer, BufferConfig, Image, ImageConfig, MemoryLocation,
    preferred_depth_format,
};
pub use sampler::{Filter, SamplerConfig, TextureSampler, Wrap};
pub use shader::ShaderModule;
pub use surface::{Surface, required_surface_extensions};
pub use swapchain::{AcquireOutcome, PresentMode, PresentOutcome, Swapchain, SwapchainConfig};
pub use sync::{BinarySemaphore, TimelineSemaphore};
pub use usage::{BufferUsage, ImageUsage};

/// Re-exported so an application that genuinely needs to reach past the RHI —
/// a vendor extension, a debugging tool — can do so without its own `ash`
/// dependency, and so the engine cannot end up split across two versions of it.
///
/// **Not the way to name a format, an extent, or a usage.** Those are
/// [`Format`], [`Extent2D`], [`BufferUsage`] and [`ImageUsage`], and the reason
/// is `docs/DESIGN.md` §2.2: this crate was bought on the promise that a second
/// backend slots in cleanly, which holds only while the layers above name types
/// defined here. Every consumer above `slop-rhi` once named `vk::` types in its
/// own public signatures, and closing that is what those four types are for.
pub use ash::vk;

pub(crate) use instance::REQUIRED_API_VERSION;
