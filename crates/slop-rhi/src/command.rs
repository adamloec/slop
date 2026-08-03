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

use crate::{
    BufferHandle, Device, Extent2D, ImageAspect, ImageHandle, RhiError, SemaphoreHandle,
    TimelineSemaphore,
};

/// A pipeline stage a submission may wait at.
///
/// Not the full Vulkan set: waiting is a choice with two sensible answers here,
/// and naming them is what keeps a caller from reaching for `vk::` to express
/// one. The stage matters — waiting at the top of the pipe stalls vertex work
/// that has no reason to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStage {
    /// Wait only when the pipeline reaches colour output. What a swapchain
    /// acquire wants: nothing before that stage touches the image.
    ColorAttachmentOutput,
    /// Wait before anything runs. The conservative answer, and the right one
    /// when the dependency is not specifically a colour write.
    AllCommands,
}

impl WaitStage {
    pub(crate) fn to_vk(self) -> vk::PipelineStageFlags2 {
        match self {
            Self::ColorAttachmentOutput => vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            Self::AllCommands => vk::PipelineStageFlags2::ALL_COMMANDS,
        }
    }
}

/// Which shader stage touches a resource.
///
/// # Why a state takes this rather than assuming it
///
/// A barrier needs the *stage* that reads or writes, not just what the access
/// is. Every named state here used to bake one in — [`ImageState::SHADER_READ`]
/// meant "read by a **fragment** shader" — which was correct while graphics was
/// the only consumer and stopped being correct the moment compute arrived.
///
/// The alternative was a constant per stage per access: `SHADER_READ`,
/// `SHADER_READ_COMPUTE`, `DEPTH_READ`, `DEPTH_READ_COMPUTE`, and so on for
/// every stage added later. That grows multiplicatively and each new name is a
/// place for the access mask to be typed differently.
///
/// This is a step toward, not a replacement for, what `docs/PLAN.md` §9.5 E3
/// does: the render graph knows what stage each pass runs at, so it will supply
/// this rather than the caller naming it. Keeping the *shape* — access in the
/// state, stage supplied — means that change is the graph filling in an argument
/// that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// A fragment shader. What samples a material's textures.
    Fragment,

    /// A compute shader. `docs/PLAN.md` §9.4's cluster build, and the post
    /// stack at E7.
    Compute,

    /// Any stage at all.
    ///
    /// Correct but pessimistic: it orders against work that never touched the
    /// resource. Right when a buffer is reached through a device address and the
    /// stage genuinely is not known — which is why
    /// [`BufferState::SHADER_READ`] uses it — and wrong as a default, because a
    /// barrier that over-synchronises is invisible in every way except speed.
    Any,
}

impl Stage {
    /// The Vulkan stage mask this maps to.
    #[must_use]
    pub(crate) const fn to_vk(self) -> vk::PipelineStageFlags2 {
        match self {
            Self::Fragment => vk::PipelineStageFlags2::FRAGMENT_SHADER,
            Self::Compute => vk::PipelineStageFlags2::COMPUTE_SHADER,
            Self::Any => vk::PipelineStageFlags2::ALL_COMMANDS,
        }
    }
}

/// Every access flag in this file that writes memory.
///
/// Listed rather than derived, because Vulkan offers no "is this a write"
/// predicate and the flags are a flat bitset. That makes this a list which can
/// fall behind the states above — a write flag missing from here is a barrier
/// silently not emitted — so `every_writing_state_says_it_writes` below walks
/// the named states and checks each one against it.
const WRITE_ACCESSES: vk::AccessFlags2 = vk::AccessFlags2::from_raw(
    vk::AccessFlags2::SHADER_WRITE.as_raw()
        | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw()
        | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE.as_raw()
        | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE.as_raw()
        | vk::AccessFlags2::TRANSFER_WRITE.as_raw()
        | vk::AccessFlags2::HOST_WRITE.as_raw()
        | vk::AccessFlags2::MEMORY_WRITE.as_raw(),
);

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

    /// Being read by a **fragment** shader.
    ///
    /// The overwhelmingly common case — a material sampling a texture — kept as
    /// a constant so the thirteen call sites that mean exactly this do not have
    /// to say so. [`shader_read`](Self::shader_read) is the same thing with the
    /// stage chosen, and this is defined in terms of it so the two cannot drift.
    pub const SHADER_READ: Self = Self::shader_read(Stage::Fragment);

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

    /// Depth being read by a **fragment** shader, as in a shadow map sampled by
    /// the pass that lights the scene.
    ///
    /// [`depth_read`](Self::depth_read) is the same with the stage chosen —
    /// `docs/PLAN.md` §9.4's cluster build reads the depth prepass from compute.
    pub const DEPTH_READ: Self = Self::depth_read(Stage::Fragment);

    /// A swapchain image just handed over by `acquire`, whose previous contents
    /// are not needed.
    ///
    /// [`UNDEFINED`](Self::UNDEFINED)'s layout, but staged at colour-attachment
    /// output rather than top-of-pipe — **and that difference is a real bug
    /// fix, not a refinement.**
    ///
    /// A frame waits on the acquire semaphore at colour-attachment output, so
    /// that vertex work need not wait for an image it never touches. A barrier
    /// transitioning the image at *top of pipe* is then ordered **before** that
    /// wait, and may run while the presentation engine still owns the image.
    /// Nothing observable goes wrong on desktop hardware, which is why this
    /// survived from M0 until synchronization validation was switched on and
    /// reported it ten times per frame in every example.
    ///
    /// The rule to keep: **the first barrier on an acquired image must be staged
    /// no earlier than the stage its semaphore is waited at.** Both halves are
    /// in `slop_render::FrameRenderer` — the state here is chosen in `render`
    /// and the wait stage in `submit`, forty lines apart — and
    /// `the_acquired_state_matches_the_wait_stage` below asserts they agree, so
    /// changing one alone is a test failure rather than a silent race.
    pub const ACQUIRED: Self = Self {
        layout: vk::ImageLayout::UNDEFINED,
        stage: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        access: vk::AccessFlags2::empty(),
    };

    /// Whether these accesses write memory.
    ///
    /// What tells a render graph that a barrier is needed **even when the state
    /// does not change**. A depth prepass and the forward pass testing against
    /// it are both [`DEPTH_ATTACHMENT`](Self::DEPTH_ATTACHMENT): identical
    /// layout, identical stages, identical access. Nothing about the state
    /// differs, and Vulkan still orders neither rendering scope against the
    /// other — the prepass's late-fragment-test write and the forward pass's
    /// early-fragment-test read need a dependency between them or the second
    /// pass may test against depth the first has not finished writing.
    ///
    /// Barriering only on a *change* of state misses exactly that case, which is
    /// how `docs/PLAN.md` §9.4's prepass found it. Synchronization validation
    /// did not report it.
    #[must_use]
    pub const fn writes(self) -> bool {
        self.access.as_raw() & WRITE_ACCESSES.as_raw() != 0
    }

    /// An image read by a shader, at the stage that reads it.
    ///
    /// See [`Stage`] for why this takes one rather than assuming.
    #[must_use]
    pub const fn shader_read(stage: Stage) -> Self {
        Self {
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            stage: stage.to_vk(),
            access: vk::AccessFlags2::SHADER_SAMPLED_READ,
        }
    }

    /// A depth image read by a shader, at the stage that reads it.
    ///
    /// `DEPTH_READ_ONLY_OPTIMAL` rather than the general shader-read layout: a
    /// depth image being sampled is still a depth image, and the read-only depth
    /// layout is what lets it stay bound as a depth attachment for testing while
    /// another pass samples it.
    #[must_use]
    pub const fn depth_read(stage: Stage) -> Self {
        Self {
            layout: vk::ImageLayout::DEPTH_READ_ONLY_OPTIMAL,
            stage: stage.to_vk(),
            access: vk::AccessFlags2::SHADER_SAMPLED_READ,
        }
    }

    /// Being written by a compute shader through the heap's storage-image
    /// binding.
    ///
    /// `GENERAL` is not a choice: it is the only layout a storage image may be
    /// written through, which is why this state has no optimal-layout cousin.
    ///
    /// **Read and write, both.** A compute pass that only writes still declares
    /// the read: shaders that sample their own output between dispatches are
    /// ordinary — a bloom chain does exactly that — and a write-only state would
    /// order those wrongly while looking correct.
    ///
    /// # This state names its stage, and that is a problem this constant cannot fix
    ///
    /// Every state here pairs an access with a *pipeline stage*, and the stage is
    /// baked in: [`SHADER_READ`](Self::SHADER_READ) means "read by a **fragment**
    /// shader" and cannot express the same read from compute. That was correct
    /// while graphics was the only consumer and stops being correct here.
    ///
    /// The right answer is for a state to carry access intent while the caller
    /// supplies the stage — and the caller that should supply it is the render
    /// graph, which knows what stage each pass runs at. Designing that now,
    /// against one compute pass that does not exist yet, is what `docs/PLAN.md`
    /// §9.4 was written to avoid. So the constants double for the moment;
    /// `docs/PLAN.md` §6.1 carries the row, and E3 collapses it.
    pub const STORAGE_WRITE: Self = Self {
        layout: vk::ImageLayout::GENERAL,
        stage: vk::PipelineStageFlags2::COMPUTE_SHADER,
        access: vk::AccessFlags2::from_raw(
            vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
                | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw(),
        ),
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
    /// Whether these accesses write memory.
    ///
    /// See [`ImageState::writes`] for why a render graph needs this and what it
    /// misses without it.
    #[must_use]
    pub const fn writes(self) -> bool {
        self.access.as_raw() & WRITE_ACCESSES.as_raw() != 0
    }

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

    /// Read by a shader at an unknown stage — including through a device
    /// address, which is how `docs/DESIGN.md` §2.2's GPU-driven passes reach
    /// most buffers.
    ///
    /// Defined in terms of [`shader_read`](Self::shader_read) so the two cannot
    /// drift. Prefer naming the stage where it is known: this one orders against
    /// work that never touched the buffer, and over-synchronising is invisible
    /// in every way except speed.
    pub const SHADER_READ: Self = Self::shader_read(Stage::Any);

    /// A buffer read by a shader, at the stage that reads it.
    ///
    /// §9.4's forward pass reads the cluster light list from a *fragment*
    /// shader, and the cluster build wrote it from compute. Naming both ends is
    /// what makes that barrier the narrow one it should be.
    #[must_use]
    pub const fn shader_read(stage: Stage) -> Self {
        Self {
            stage: stage.to_vk(),
            access: vk::AccessFlags2::SHADER_READ,
        }
    }

    /// Supplying draw or dispatch parameters to an indirect command.
    pub const INDIRECT: Self = Self {
        stage: vk::PipelineStageFlags2::DRAW_INDIRECT,
        access: vk::AccessFlags2::INDIRECT_COMMAND_READ,
    };

    /// Being written by a shader through the heap's storage-buffer binding.
    ///
    /// `docs/PLAN.md` §9.4's cluster build is the first of these: compute writes
    /// a light-index buffer that the forward pass then reads, and the read side
    /// is [`SHADER_READ`](Self::SHADER_READ) above.
    ///
    /// **Read and write, both**, for the reason
    /// [`ImageState::STORAGE_WRITE`] gives: a pass that accumulates into a
    /// buffer reads what it wrote, and a write-only state orders that wrongly
    /// while looking correct.
    ///
    /// Unlike the image side there is no layout to get wrong here, which is why
    /// this is a plain function of the stage and `ImageState`'s equivalent is a
    /// constant pinned to `GENERAL`.
    #[must_use]
    pub const fn storage_write(stage: Stage) -> Self {
        Self {
            stage: stage.to_vk(),
            access: vk::AccessFlags2::from_raw(
                vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw(),
            ),
        }
    }
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
    /// The device this was allocated from.
    pub(crate) fn device(&self) -> &Arc<Device> {
        &self.device
    }

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
        image: ImageHandle,
        aspect: ImageAspect,
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
            .image(image.0)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect.to_vk(),
                base_mip_level: 0,
                // Every level, not just level zero. A barrier names a
                // *subresource range*, and layout is tracked per level — so a
                // transition covering one level leaves the rest of a mip chain
                // in UNDEFINED, and sampling those is undefined behaviour that
                // validation reports as a layout mismatch far from here.
                level_count: vk::REMAINING_MIP_LEVELS,
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
    pub fn barrier_buffer(&self, buffer: BufferHandle, from: BufferState, to: BufferState) {
        let barriers = [vk::BufferMemoryBarrier2::default()
            .src_stage_mask(from.stage)
            .src_access_mask(from.access)
            .dst_stage_mask(to.stage)
            .dst_access_mask(to.access)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(buffer.0)
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
    pub fn copy_buffer(&self, source: BufferHandle, destination: BufferHandle, size: u64) {
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
                .cmd_copy_buffer(self.handle, source.0, destination.0, &regions);
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
        buffer: BufferHandle,
        image: ImageHandle,
        aspect: ImageAspect,
        extent: Extent2D,
    ) {
        self.copy_buffer_to_image_level(buffer, 0, image, aspect, extent, 0);
    }

    /// Copy one mip level out of a buffer holding a whole chain.
    ///
    /// `buffer_offset` is where this level's bytes start, and `extent` is *this
    /// level's* size rather than level zero's — Vulkan validates the copy
    /// against the level's real dimensions, so passing the base extent for level
    /// three is rejected rather than silently scaled.
    ///
    /// The whole chain is one buffer and one copy per level, rather than a
    /// staging buffer each. Levels are tiny after the first two: a full chain is
    /// only a third larger than level zero alone.
    pub fn copy_buffer_to_image_level(
        &self,
        buffer: BufferHandle,
        buffer_offset: u64,
        image: ImageHandle,
        aspect: ImageAspect,
        extent: Extent2D,
        level: u32,
    ) {
        let regions = [vk::BufferImageCopy::default()
            .buffer_offset(buffer_offset)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: aspect.to_vk(),
                mip_level: level,
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
                buffer.0,
                image.0,
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
    pub fn make_visible_to_host(&self, buffer: BufferHandle) {
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
    pub fn copy_image_to_buffer(&self, image: ImageHandle, buffer: BufferHandle, extent: Extent2D) {
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
                image.0,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer.0,
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

    submit_recorded_and_wait(device, &command)
}

/// Submit an already-recorded command buffer and wait for it to finish.
///
/// The same blocking one-off as [`submit_and_wait`], for callers that own their
/// command pool rather than wanting one allocated per submission — a test that
/// resets and reuses one pool across captured frames, most typically.
///
/// Prefer [`submit_and_wait`] unless the pool is genuinely being reused: it
/// closes over recording, so the buffer cannot be submitted un-ended.
///
/// # Errors
///
/// [`RhiError`] if the timeline cannot be created, the submission is rejected,
/// or the work does not complete within ten seconds.
pub fn submit_recorded_and_wait(
    device: &Arc<Device>,
    command: &CommandBuffer,
) -> Result<(), RhiError> {
    let timeline = TimelineSemaphore::new(device, 0)?;

    device.submit_graphics(&Submission {
        wait: &[],
        signal: &[],
        signal_timeline: &[(timeline.handle(), 1)],
        command,
    })?;

    if !timeline.wait(1, ONE_SHOT_TIMEOUT)? {
        return Err(RhiError::Timeout {
            what: "a one-off submission",
        });
    }

    Ok(())
}

/// One frame's submission: what to wait for, what to run, what to signal.
///
/// Every field is optional in the sense that an empty slice is legal, which is
/// what makes this cover both a frame (wait on acquire, signal present and the
/// timeline) and a one-off upload (wait on nothing).
#[derive(Debug, Clone, Copy)]
pub struct Submission<'a> {
    /// Semaphores the GPU waits on before the commands run, with the stage each
    /// wait applies to.
    ///
    /// The stage matters: waiting at the top of the pipe stalls vertex work that
    /// has no reason to wait, and the difference is visible in a frame's
    /// occupancy rather than in correctness.
    pub wait: &'a [(SemaphoreHandle, WaitStage)],
    /// Binary semaphores signalled when the commands finish.
    pub signal: &'a [SemaphoreHandle],
    /// Timeline semaphores and the values to signal them to.
    pub signal_timeline: &'a [(SemaphoreHandle, u64)],
    /// The command buffer to run.
    pub command: &'a CommandBuffer,
}

impl Device {
    /// Submit to the graphics queue.
    ///
    /// Exists so that recording a frame needs no `unsafe` in the crate doing the
    /// recording — `docs/CONVENTIONS.md` §7 keeps it in this one.
    ///
    /// # Errors
    ///
    /// [`RhiError`] if the driver rejects the submission, which for a
    /// well-formed one means the device was lost.
    pub fn submit_graphics(&self, submission: &Submission<'_>) -> Result<(), RhiError> {
        let wait: Vec<vk::SemaphoreSubmitInfo<'_>> = submission
            .wait
            .iter()
            .map(|(semaphore, stage)| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore.0)
                    .stage_mask(stage.to_vk())
            })
            .collect();

        let signal: Vec<vk::SemaphoreSubmitInfo<'_>> = submission
            .signal
            .iter()
            .map(|semaphore| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore.0)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            })
            .chain(submission.signal_timeline.iter().map(|(semaphore, value)| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore.0)
                    .value(*value)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            }))
            .collect();

        let commands =
            [vk::CommandBufferSubmitInfo::default().command_buffer(submission.command.handle())];

        let submits = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait)
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signal)];

        // SAFETY: the caller guarantees the command buffer is recorded and not
        // pending; every semaphore belongs to this device, and each borrowed
        // array outlives the call.
        unsafe {
            self.raw()
                .queue_submit2(self.queues().graphics.0, &submits, vk::Fence::null())
        }?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of the acquire race fix must agree.
    ///
    /// A frame waits on the acquire semaphore at one stage and transitions the
    /// image at another, and the transition must not be *earlier* — otherwise it
    /// is ordered before the wait and may run while the presentation engine
    /// still owns the image. That was a real race, present from M0 until
    /// synchronization validation reported it ten times a frame.
    ///
    /// Asserted rather than left as a comment because both halves are ordinary
    /// values in one file, so a well-meaning change to either — narrowing the
    /// wait to save a stall, widening the transition — silently reintroduces it.
    #[test]
    fn the_acquired_state_matches_the_wait_stage() {
        assert_eq!(
            ImageState::ACQUIRED.stage,
            WaitStage::ColorAttachmentOutput.to_vk(),
            "the first barrier on an acquired image is staged earlier than the \
             semaphore it must follow"
        );
    }

    /// `WRITE_ACCESSES` must keep up with the states named above.
    ///
    /// It is a hand-written list, because Vulkan's access flags are a flat
    /// bitset with no way to ask whether one writes. A state whose write flag is
    /// missing from that list reports `writes() == false`, and a render graph
    /// then skips a barrier it needed — which is not a validation error, it is a
    /// race. So the states are enumerated here and each is checked against the
    /// answer, rather than the list being trusted.
    #[test]
    fn every_writing_state_says_it_writes() {
        for (state, expected) in [
            (ImageState::UNDEFINED, false),
            (ImageState::ACQUIRED, false),
            (ImageState::PRESENT, false),
            (ImageState::COLOR_ATTACHMENT, true),
            (ImageState::DEPTH_ATTACHMENT, true),
            (ImageState::SHADER_READ, false),
            (ImageState::shader_read(Stage::Compute), false),
            (ImageState::DEPTH_READ, false),
            (ImageState::TRANSFER_SRC, false),
            (ImageState::TRANSFER_DST, true),
            (ImageState::STORAGE_WRITE, true),
        ] {
            assert_eq!(
                state.writes(),
                expected,
                "{state:?} disagrees about whether it writes"
            );
        }

        for (state, expected) in [
            (BufferState::HOST_READ, false),
            (BufferState::HOST_WRITE, true),
            (BufferState::TRANSFER_SRC, false),
            (BufferState::TRANSFER_DST, true),
            (BufferState::VERTEX_INPUT, false),
            (BufferState::INDEX_INPUT, false),
            (BufferState::SHADER_READ, false),
            (BufferState::INDIRECT, false),
            (BufferState::storage_write(Stage::Compute), true),
        ] {
            assert_eq!(
                state.writes(),
                expected,
                "{state:?} disagrees about whether it writes"
            );
        }
    }

    /// The case that made `writes` necessary at all.
    ///
    /// A depth prepass and the forward pass after it are the *same state*, so a
    /// graph barriering on state change alone emits nothing between them. This
    /// is the property that stops it.
    #[test]
    fn two_consecutive_depth_passes_are_indistinguishable_by_state_alone() {
        assert_eq!(ImageState::DEPTH_ATTACHMENT, ImageState::DEPTH_ATTACHMENT);
        assert!(ImageState::DEPTH_ATTACHMENT.writes());
    }

    /// `ACQUIRED` discards, exactly as `UNDEFINED` does.
    ///
    /// The stage is the only difference. Giving it a real layout would preserve
    /// contents the frame is about to clear, which costs bandwidth for nothing.
    #[test]
    fn the_acquired_state_still_discards() {
        assert_eq!(ImageState::ACQUIRED.layout, ImageState::UNDEFINED.layout);
        assert!(ImageState::ACQUIRED.access.is_empty());
    }

    /// Naming a stage must actually change the barrier.
    ///
    /// Guards `Stage::to_vk` collapsing two variants onto one mask, which would
    /// make a compute read order against fragment work and look correct.
    #[test]
    fn each_stage_is_a_distinct_mask() {
        let all = [Stage::Fragment, Stage::Compute, Stage::Any];

        for (index, one) in all.iter().enumerate() {
            for other in &all[index + 1..] {
                assert_ne!(one.to_vk(), other.to_vk(), "{one:?} and {other:?} collide");
            }
        }
    }

    /// The constants are the stage-chosen functions, not a second spelling.
    #[test]
    fn the_fragment_constants_are_the_selector_applied() {
        assert_eq!(
            ImageState::SHADER_READ,
            ImageState::shader_read(Stage::Fragment)
        );
        assert_eq!(
            ImageState::DEPTH_READ,
            ImageState::depth_read(Stage::Fragment)
        );
    }

    /// A storage write declares the read too, so a pass that accumulates into a
    /// resource is ordered against itself.
    #[test]
    fn a_storage_write_covers_reading_back_what_it_wrote() {
        let state = BufferState::storage_write(Stage::Compute);

        assert!(
            state
                .access
                .contains(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        );
        assert!(state.access.contains(vk::AccessFlags2::SHADER_STORAGE_READ));
    }
}
