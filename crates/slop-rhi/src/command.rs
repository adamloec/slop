//! Command pools, command buffers, and image barriers.
//!
//! # Pools are per-thread, and reset wholesale
//!
//! A Vulkan command pool is **not thread-safe**: two threads recording into
//! buffers from one pool is undefined behaviour. Parallel command recording —
//! which `docs/DESIGN.md` §2.5 and §4.1 both depend on — therefore means one
//! pool per thread, per frame in flight, and that shape is baked in here rather
//! than discovered later.
//!
//! Buffers are recycled by resetting the **pool**, never the individual buffer.
//! Vulkan offers `RESET_COMMAND_BUFFER` for the latter, but setting it forces
//! the driver onto a slower internal allocator for every buffer in the pool, to
//! support a capability the engine does not want. Resetting a whole pool once
//! per frame returns its memory in one operation and is the fast path.

use std::sync::Arc;

use ash::vk;

use crate::{Device, RhiError};

/// A point in an image's lifetime: its layout, and the stage and access that
/// last touched it or will next touch it.
///
/// Bundling the three is deliberate. `docs/DESIGN.md` §2.2 commits to explicit
/// barriers, and the failure mode of explicit barriers is not forgetting them —
/// it is specifying the layout correctly while getting the stage or access mask
/// subtly wrong, which validation may not catch and hardware may tolerate until
/// it does not. Naming the common states once means those three fields cannot
/// drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageState {
    /// How the image is arranged in memory.
    pub layout: vk::ImageLayout,
    /// The pipeline stages that must complete, or must wait.
    pub stage: vk::PipelineStageFlags2,
    /// The memory accesses to make visible, or available.
    pub access: vk::AccessFlags2,
}

impl ImageState {
    /// Contents undefined. The correct source state for an image whose previous
    /// contents are not needed — which is every swapchain image at the start of
    /// a frame, since the whole point is to overwrite it.
    ///
    /// Transitioning *from* `UNDEFINED` permits the driver to discard rather
    /// than preserve, which is faster. Using the real previous layout instead is
    /// a common and needless cost.
    pub const UNDEFINED: Self = Self {
        layout: vk::ImageLayout::UNDEFINED,
        stage: vk::PipelineStageFlags2::TOP_OF_PIPE,
        access: vk::AccessFlags2::empty(),
    };

    /// Being rendered into.
    pub const COLOR_ATTACHMENT: Self = Self {
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        stage: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        access: vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    };

    /// Ready for the presentation engine.
    ///
    /// No access flags: presentation is not a pipeline stage that reads through
    /// the normal memory model, and the semaphore signalled at submission is
    /// what actually orders it. Specifying an access mask here would be
    /// meaningless rather than merely redundant.
    pub const PRESENT: Self = Self {
        layout: vk::ImageLayout::PRESENT_SRC_KHR,
        stage: vk::PipelineStageFlags2::BOTTOM_OF_PIPE,
        access: vk::AccessFlags2::empty(),
    };

    /// Being read by a shader.
    pub const SHADER_READ: Self = Self {
        layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
        access: vk::AccessFlags2::SHADER_SAMPLED_READ,
    };

    /// The destination of a copy or blit.
    pub const TRANSFER_DST: Self = Self {
        layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        stage: vk::PipelineStageFlags2::TRANSFER,
        access: vk::AccessFlags2::TRANSFER_WRITE,
    };

    /// The source of a copy or blit.
    pub const TRANSFER_SRC: Self = Self {
        layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        stage: vk::PipelineStageFlags2::TRANSFER,
        access: vk::AccessFlags2::TRANSFER_READ,
    };
}

/// Allocates command buffers for one thread and one frame in flight.
pub struct CommandPool {
    handle: vk::CommandPool,
    device: Arc<Device>,
    family: u32,
}

impl CommandPool {
    /// Create a pool for `family`.
    ///
    /// The pool is created transient, which tells the driver its buffers are
    /// short-lived and reset often — true of every pool in a frame loop.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation.
    pub fn new(device: &Arc<Device>, family: u32) -> Result<Self, RhiError> {
        let create_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(family)
            // Deliberately NOT `RESET_COMMAND_BUFFER` — see the module docs.
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);

        // SAFETY: `create_info` is fully initialized and `family` is one of the
        // families this device was created with.
        let handle = unsafe { device.raw().create_command_pool(&create_info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
            family,
        })
    }

    /// The queue family these buffers may be submitted to.
    pub fn family(&self) -> u32 {
        self.family
    }

    /// The underlying handle.
    pub fn handle(&self) -> vk::CommandPool {
        self.handle
    }

    /// Allocate `count` primary command buffers.
    ///
    /// # Errors
    ///
    /// Fails if the driver cannot allocate.
    pub fn allocate(&self, count: u32) -> Result<Vec<CommandBuffer>, RhiError> {
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.handle)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count);

        // SAFETY: the pool belongs to this device and is alive.
        let handles = unsafe { self.device.raw().allocate_command_buffers(&info) }?;

        Ok(handles
            .into_iter()
            .map(|handle| CommandBuffer {
                handle,
                device: Arc::clone(&self.device),
            })
            .collect())
    }

    /// Reset every buffer allocated from this pool at once.
    ///
    /// The caller must know that no buffer from this pool is still executing.
    /// That is what the frame's timeline semaphore is for: wait on the value the
    /// frame signalled before reusing its pool.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn reset(&self) -> Result<(), RhiError> {
        // SAFETY: the pool belongs to this device. Vulkan requires no buffer
        // from it be pending; that is the caller's documented obligation and
        // validation reports a breach.
        unsafe {
            self.device
                .raw()
                .reset_command_pool(self.handle, vk::CommandPoolResetFlags::empty())
        }?;

        Ok(())
    }
}

impl Drop for CommandPool {
    fn drop(&mut self) {
        // Destroying a pool frees its buffers, so they need no separate cleanup.
        //
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_command_pool(self.handle, None) };
    }
}

impl std::fmt::Debug for CommandPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandPool")
            .field("family", &self.family)
            .finish_non_exhaustive()
    }
}

/// A primary command buffer.
///
/// Owned by its pool: dropping this does not free anything, because
/// [`CommandPool::reset`] and the pool's own destruction are what reclaim the
/// memory. That is why there is no `Drop` impl.
pub struct CommandBuffer {
    handle: vk::CommandBuffer,
    device: Arc<Device>,
}

impl CommandBuffer {
    /// The underlying handle, for recording commands `ash` exposes directly.
    ///
    /// M0 records through `ash` rather than behind an abstraction, per
    /// `docs/PLAN.md` §4.1-D: the recording API is what M3 extracts once a
    /// render graph exists to say what it should be.
    pub fn handle(&self) -> vk::CommandBuffer {
        self.handle
    }

    /// Begin recording, for a buffer submitted exactly once.
    ///
    /// # Errors
    ///
    /// Fails if the buffer is already recording, or the device was lost.
    pub fn begin(&self) -> Result<(), RhiError> {
        let info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);

        // SAFETY: the buffer belongs to this device and is not currently
        // recording, which validation enforces.
        unsafe { self.device.raw().begin_command_buffer(self.handle, &info) }?;

        Ok(())
    }

    /// Finish recording.
    ///
    /// # Errors
    ///
    /// Fails if recording was never begun, or a command was invalid.
    pub fn end(&self) -> Result<(), RhiError> {
        // SAFETY: the buffer belongs to this device and is recording.
        unsafe { self.device.raw().end_command_buffer(self.handle) }?;

        Ok(())
    }

    /// Record a layout transition for a whole colour image.
    ///
    /// Covers the common case — one mip, one array layer, colour aspect — which
    /// is every swapchain image. Anything else builds its own barrier.
    pub fn transition_image(&self, image: vk::Image, from: ImageState, to: ImageState) {
        let barrier = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(from.stage)
            .src_access_mask(from.access)
            .dst_stage_mask(to.stage)
            .dst_access_mask(to.access)
            .old_layout(from.layout)
            .new_layout(to.layout)
            // No queue family ownership transfer. When graphics and present
            // families differ, an EXCLUSIVE swapchain image needs an explicit
            // transfer — a separate concern from a layout change, and not
            // something to smuggle in here.
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let barriers = [barrier];
        let dependency = vk::DependencyInfo::default().image_memory_barriers(&barriers);

        // SAFETY: the buffer is recording, `dependency` borrows `barriers` which
        // outlives the call, and `synchronization2` was verified present during
        // device selection.
        unsafe {
            self.device
                .raw()
                .cmd_pipeline_barrier2(self.handle, &dependency);
        }
    }
}

impl std::fmt::Debug for CommandBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandBuffer").finish_non_exhaustive()
    }
}
