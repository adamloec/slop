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

use crate::{Device, RhiError, TimelineSemaphore};

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

    /// Being depth-tested and written.
    ///
    /// Both stages, and both accesses. The early fragment test reads and writes
    /// depth before the fragment shader runs, and the late test does so after —
    /// naming only one is the classic depth barrier bug, because it works right
    /// up until a shader discards or writes its own depth and the driver moves
    /// the test to the other stage.
    pub const DEPTH_ATTACHMENT: Self = Self {
        layout: vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
        stage: vk::PipelineStageFlags2::from_raw(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS.as_raw()
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS.as_raw(),
        ),
        access: vk::AccessFlags2::from_raw(
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ.as_raw()
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE.as_raw(),
        ),
    };

    /// Depth being read by a shader, as in a shadow map or a depth prepass
    /// consumed later in the frame.
    pub const DEPTH_READ: Self = Self {
        layout: vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL,
        stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
        access: vk::AccessFlags2::SHADER_SAMPLED_READ,
    };
}

/// A point in a buffer's lifetime.
///
/// The same idea as [`ImageState`] minus the layout, which buffers do not have.
/// Naming the pairs once is what keeps a stage and its access mask from drifting
/// apart — a barrier with the right stage and the wrong access is not a
/// validation error, it is a race that reproduces on one vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferState {
    /// The pipeline stages that must complete, or must wait.
    pub stage: vk::PipelineStageFlags2,
    /// The memory accesses to make visible, or available.
    pub access: vk::AccessFlags2,
}

impl BufferState {
    /// Just written by the CPU through a mapped pointer.
    pub const HOST_WRITE: Self = Self {
        stage: vk::PipelineStageFlags2::HOST,
        access: vk::AccessFlags2::HOST_WRITE,
    };

    /// About to be read by the CPU.
    pub const HOST_READ: Self = Self {
        stage: vk::PipelineStageFlags2::HOST,
        access: vk::AccessFlags2::HOST_READ,
    };

    /// The source of a copy.
    pub const TRANSFER_SRC: Self = Self {
        stage: vk::PipelineStageFlags2::TRANSFER,
        access: vk::AccessFlags2::TRANSFER_READ,
    };

    /// The destination of a copy.
    pub const TRANSFER_DST: Self = Self {
        stage: vk::PipelineStageFlags2::TRANSFER,
        access: vk::AccessFlags2::TRANSFER_WRITE,
    };

    /// Being fetched as vertex attributes.
    pub const VERTEX_INPUT: Self = Self {
        stage: vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT,
        access: vk::AccessFlags2::VERTEX_ATTRIBUTE_READ,
    };

    /// Being fetched as indices.
    pub const INDEX_INPUT: Self = Self {
        stage: vk::PipelineStageFlags2::INDEX_INPUT,
        access: vk::AccessFlags2::INDEX_READ,
    };

    /// Read by a shader — including through a device address, which is how
    /// `docs/DESIGN.md` §2.2's GPU-driven passes reach most buffers.
    pub const SHADER_READ: Self = Self {
        stage: vk::PipelineStageFlags2::ALL_COMMANDS,
        access: vk::AccessFlags2::SHADER_READ,
    };

    /// Supplying draw or dispatch parameters to an indirect command.
    pub const INDIRECT: Self = Self {
        stage: vk::PipelineStageFlags2::DRAW_INDIRECT,
        access: vk::AccessFlags2::INDIRECT_COMMAND_READ,
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

    /// Record a layout transition for a whole image.
    ///
    /// Covers one mip and one array layer, which is every image the engine has
    /// so far. `aspect` comes from the image's format —
    /// [`Image::aspect`](crate::Image::aspect) supplies it, and
    /// [`aspect_of`](crate::aspect_of) derives it for a raw handle such as a
    /// swapchain image.
    ///
    /// It is a parameter rather than a constant because a depth image needs
    /// `DEPTH`, a depth-stencil image needs both, and a barrier naming the
    /// wrong aspect transitions nothing while reporting nothing.
    pub fn transition_image(
        &self,
        image: vk::Image,
        aspect: vk::ImageAspectFlags,
        from: ImageState,
        to: ImageState,
    ) {
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
                aspect_mask: aspect,
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

    /// Record a barrier moving `buffer` from one state to another.
    ///
    /// Covers the whole buffer. Sub-range barriers exist and are almost never
    /// what is wanted — a buffer written in one pass and read in the next is
    /// read whole.
    pub fn barrier_buffer(&self, buffer: vk::Buffer, from: BufferState, to: BufferState) {
        let barriers = [vk::BufferMemoryBarrier2::default()
            .src_stage_mask(from.stage)
            .src_access_mask(from.access)
            .dst_stage_mask(to.stage)
            .dst_access_mask(to.access)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE)];

        let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&barriers);

        // SAFETY: the buffer is recording, `dependency` borrows `barriers`
        // which outlives the call, and `synchronization2` is in the required
        // feature tier.
        unsafe {
            self.device
                .raw()
                .cmd_pipeline_barrier2(self.handle, &dependency);
        }
    }

    /// Record a copy between two buffers.
    ///
    /// The upload path: write into a host-visible staging buffer, copy here,
    /// and the data lands in device-local memory the GPU reads at full speed.
    /// Mapping a device-local buffer directly is not possible on most discrete
    /// hardware, and where it is, writes go over PCIe on every read.
    ///
    /// `size` bytes from the start of each. Both buffers need the matching
    /// `TRANSFER_SRC` and `TRANSFER_DST` usage flags.
    pub fn copy_buffer(&self, source: vk::Buffer, destination: vk::Buffer, size: u64) {
        let regions = [vk::BufferCopy::default()
            .src_offset(0)
            .dst_offset(0)
            .size(size)];

        // SAFETY: the buffer is recording, `regions` outlives the call, and
        // both buffers carrying the right usage flags is the caller's
        // obligation — one validation reports if broken.
        unsafe {
            self.device
                .raw()
                .cmd_copy_buffer(self.handle, source, destination, &regions);
        }
    }

    /// Record a copy from a buffer into a whole image, tightly packed.
    ///
    /// The inverse of [`copy_image_to_buffer`](Self::copy_image_to_buffer), and
    /// how a texture gets to the GPU. The image must already be in
    /// [`ImageState::TRANSFER_DST`], and the source rows must be tightly packed
    /// — zero for both `bufferRowLength` and `bufferImageHeight` means "the same
    /// as the copy extent", so the stride is `width * bytes_per_pixel` with no
    /// padding.
    pub fn copy_buffer_to_image(
        &self,
        buffer: vk::Buffer,
        image: vk::Image,
        aspect: vk::ImageAspectFlags,
        extent: vk::Extent2D,
    ) {
        let regions = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: aspect,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })];

        // SAFETY: the buffer is recording, `regions` outlives the call, and the
        // image being in TRANSFER_DST_OPTIMAL is the caller's documented
        // obligation.
        unsafe {
            self.device.raw().cmd_copy_buffer_to_image(
                self.handle,
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }
    }

    /// Record a barrier making prior transfer writes to `buffer` readable by the
    /// CPU.
    ///
    /// Mapped memory being host-coherent means no cache maintenance is needed,
    /// but coherence is not ordering: without this barrier the host may observe
    /// the buffer before the copy that filled it has completed, and the read
    /// silently returns whatever was there. Waiting on a semaphore is not a
    /// substitute — it orders execution, and this orders memory.
    ///
    /// The `HOST` pipeline stage exists precisely for this, and is the only
    /// place in the engine it should appear: everything else in a frame is
    /// ordered GPU-side.
    pub fn make_visible_to_host(&self, buffer: vk::Buffer) {
        self.barrier_buffer(buffer, BufferState::TRANSFER_DST, BufferState::HOST_READ);
    }

    /// Record a copy of a whole colour image into a buffer, tightly packed.
    ///
    /// This is how pixels reach the CPU. An optimally tiled image has a
    /// driver-private memory layout, so mapping one directly is not possible —
    /// the copy is what converts it to rows the host can read.
    ///
    /// The image must already be in [`ImageState::TRANSFER_SRC`], and the
    /// buffer must be at least `width * height * bytes_per_pixel` bytes and
    /// carry [`vk::BufferUsageFlags::TRANSFER_DST`].
    ///
    /// Rows are tightly packed: zero for both `bufferRowLength` and
    /// `bufferImageHeight` means "the same as the copy extent", so the
    /// destination has no padding between rows and `width * bytes_per_pixel`
    /// is the stride. Anything else would have to be communicated back to the
    /// caller, and there is no reason to want it here.
    pub fn copy_image_to_buffer(&self, image: vk::Image, buffer: vk::Buffer, extent: vk::Extent2D) {
        let regions = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })];

        // SAFETY: the buffer is recording, `regions` outlives the call, and the
        // image being in TRANSFER_SRC_OPTIMAL is the caller's documented
        // obligation — one validation reports if broken.
        unsafe {
            self.device.raw().cmd_copy_image_to_buffer(
                self.handle,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &regions,
            );
        }
    }
}

impl std::fmt::Debug for CommandBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandBuffer").finish_non_exhaustive()
    }
}

/// How long a one-off submission may take before it is treated as hung.
///
/// Generous: an upload of a large texture on a busy queue is slow, not broken.
/// A timeout here means the GPU is not coming back.
const ONE_SHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Record a command buffer, submit it, and wait for it to finish.
///
/// The blunt instrument for work that happens outside a frame: uploading a mesh
/// at load time, building a font atlas, taking a screenshot. It allocates a
/// pool, submits one buffer, and blocks — none of which belongs in a frame loop,
/// and all of which is exactly right for startup.
///
/// `docs/PLAN.md` §6.1 records the replacement: an async transfer queue with a
/// staging ring, so streaming does not stall the caller. This stays correct for
/// the cases that genuinely are one-off.
///
/// # Errors
///
/// [`RhiError`] if the pool or buffer cannot be created, the submission is
/// rejected, or the work does not complete within ten seconds.
pub fn submit_and_wait(
    device: &Arc<Device>,
    record: impl FnOnce(&CommandBuffer),
) -> Result<(), RhiError> {
    let pool = CommandPool::new(device, device.queue_families().graphics)?;
    let command = pool
        .allocate(1)?
        .pop()
        .expect("one command buffer was requested");

    command.begin()?;
    record(&command);
    command.end()?;

    let timeline = TimelineSemaphore::new(device, 0)?;

    let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command.handle())];
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(timeline.handle())
        .value(1)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&commands)
        .signal_semaphore_infos(&signals)];

    // SAFETY: the buffer is recorded and not pending, the timeline belongs to
    // this device, and every borrowed array outlives the call.
    unsafe {
        device
            .raw()
            .queue_submit2(device.queues().graphics, &submits, vk::Fence::null())
    }?;

    if !timeline.wait(1, ONE_SHOT_TIMEOUT)? {
        return Err(RhiError::Timeout {
            what: "a one-off submission",
        });
    }

    Ok(())
}
