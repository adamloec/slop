//! GPU memory and the resources that live in it.
//!
//! Three modules sharing one subject, per `docs/CONVENTIONS.md` §2.3: something
//! hands out memory, and two kinds of object consume it.
//!
//! # Why an allocator rather than `vkAllocateMemory` per resource
//!
//! Vulkan reports `maxMemoryAllocationCount`, and on desktop drivers it is
//! commonly 4096. One device allocation per buffer and per texture therefore
//! stops working somewhere around the first real scene, and the failure is a
//! hard `ERROR_TOO_MANY_OBJECTS` rather than a slowdown. Every resource here
//! suballocates from larger blocks from the start, so there is no point at which
//! the model has to change.
//!
//! `docs/DESIGN.md` §2.2's transient resource aliasing — render targets sharing
//! memory across passes that never overlap — also needs suballocation to be
//! expressible at all. That lands with the render graph at M3; what matters now
//! is that resources already carry an allocation rather than a bare
//! `vk::DeviceMemory`, so adding aliasing is a change to the allocator and not to
//! every call site.
//!
//! # Ownership
//!
//! [`Allocator`] holds an `Arc<Device>`; [`Buffer`] and [`Image`] hold an
//! `Arc<Allocator>`. That is the same downward-pointing shape the rest of the
//! crate uses, and it is what makes destruction order correct without anyone
//! having to think about it: a resource cannot outlive the allocator that must
//! free it, and the allocator cannot outlive the device that owns the memory.
//!
//! The `Arc<Device>` is deliberately *not* held the other way around. If
//! [`Device`](crate::Device) owned its allocator, the cycle would keep both alive
//! forever.

mod allocator;
mod buffer;
mod image;

pub use allocator::{Allocator, AllocatorStats, MemoryLocation};
pub use buffer::{Buffer, BufferConfig};
pub use image::{Image, ImageConfig, preferred_depth_format};
