//! Acquire, record, submit, present — the loop every windowed application runs.
//!
//! This existed twice before this crate did, copied between `examples/cube` and
//! `examples/triangle`. Those copies are what the design below is derived from:
//! they say what the loop must handle, having been debugged into working against
//! real validation output. What they are *not* is the implementation — see
//! `docs/PLAN.md` §9.1.

use std::sync::Arc;
use std::time::Duration;

use slop_core::diagnostics::tracing::error;
use slop_rhi::{
    AcquireOutcome, BinarySemaphore, CommandBuffer, CommandPool, Device, ImageState, PresentMode,
    PresentOutcome, Surface, Swapchain, SwapchainConfig, TimelineSemaphore, vk,
};

use crate::RenderError;

/// How long to wait for the presentation engine to hand over an image.
///
/// A timeout here means it is wedged rather than busy, and the frame is skipped
/// rather than the application blocking forever on a compositor that is not
/// coming back. Long enough that a stalled-but-alive compositor still completes.
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

/// How to set up a [`FrameRenderer`].
#[derive(Debug, Clone)]
pub struct FrameRendererConfig {
    /// How many frames the CPU may prepare ahead of the GPU.
    ///
    /// Two is the usual answer: one being recorded while one is in flight. One
    /// serialises CPU and GPU and is useful for debugging; three adds latency
    /// for throughput that a frame this simple does not need. It was a `const`
    /// in both examples, which is exactly the kind of thing a library must not
    /// decide for its caller.
    pub frames_in_flight: usize,

    /// Presentation pacing.
    pub present_mode: PresentMode,

    /// How long to wait for an image before skipping the frame.
    pub acquire_timeout: Duration,
}

impl Default for FrameRendererConfig {
    fn default() -> Self {
        Self {
            frames_in_flight: 2,
            present_mode: PresentMode::Mailbox,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        }
    }
}

/// Where a frame is drawn, and what state the image must be left in.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    /// The colour image being rendered into.
    pub image: vk::Image,
    /// A view of it.
    pub view: vk::ImageView,
    /// Its size in pixels.
    pub extent: vk::Extent2D,
    /// The state the image is in on entry.
    pub from: ImageState,
    /// The state the image must be left in.
    pub to: ImageState,
}

/// One frame's worth of what a caller needs to record into it.
#[derive(Debug)]
pub struct Frame<'a> {
    /// The command buffer, already begun. [`FrameRenderer::render`] ends it.
    pub command: &'a CommandBuffer,
    /// The swapchain image this frame draws into.
    pub target: Target,
    /// How many frames have been presented before this one.
    ///
    /// The determinism handle (`docs/DESIGN.md` §2.14): animation driven from
    /// this rather than from a clock is what makes a golden image of a moving
    /// scene possible. Skipped frames do not advance it, so it counts frames
    /// that were actually drawn.
    pub number: u64,
    /// Which in-flight slot this frame is using.
    ///
    /// Anything writing GPU memory *per frame* — a UI's vertex buffer, a
    /// per-frame uniform block — needs one copy per slot and needs to know which
    /// one to write. A single shared copy is corrupted by the previous frame
    /// still reading it: [`FrameRenderer::render`] waits for *this* slot before
    /// recording, which says nothing about the others still in flight.
    pub slot: usize,
    /// How many slots exist, so a caller can size its own ring to match.
    pub slots: usize,
}

impl Frame<'_> {
    /// Put the target into the state the frame renderer needs it in.
    ///
    /// Call once, after everything that draws into this frame. Renderers leave
    /// the colour attachment in [`ImageState::COLOR_ATTACHMENT`] so that another
    /// pass can follow — the overlay composites over the scene — and **only the
    /// last writer may perform the final transition.**
    ///
    /// Forgetting this leaves the image in the wrong layout for presentation.
    /// Doing it twice, or doing it before something else draws, is the bug this
    /// exists to prevent: an overlay pass beginning on an image already handed
    /// to the presentation engine, which validation reports once per frame.
    ///
    /// This is a convention, and conventions rot. The render graph
    /// (`docs/PLAN.md` §9.2 item E) is what will derive these transitions from
    /// declared reads and writes instead.
    pub fn finish(&self) {
        self.command.transition_image(
            self.target.image,
            vk::ImageAspectFlags::COLOR,
            ImageState::COLOR_ATTACHMENT,
            self.target.to,
        );
    }
}

/// What happened when a frame was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// It was recorded, submitted and queued for display.
    Presented,

    /// No image was available, so nothing was recorded.
    ///
    /// Normal rather than exceptional: it happens while a window is being
    /// resized and while the swapchain is out of date. The next
    /// [`FrameRenderer::prepare`] fixes it.
    Skipped,
}

/// Everything one in-flight frame needs of its own.
struct Slot {
    pool: CommandPool,
    command: CommandBuffer,
    /// Signalled when the presentation engine may hand this slot an image.
    acquire: BinarySemaphore,
    /// The timeline value this slot's last submission signals.
    signalled: u64,
}

/// Drives the swapchain: acquire, record, submit, present.
///
/// Owns the swapchain and the per-frame synchronisation, and nothing else. It
/// does not own an event loop, a window, or a scene — `docs/DESIGN.md` §1.2
/// principle 4 makes that the application's business, and a renderer that ran
/// the loop would be a framework to sit inside rather than a piece to use.
pub struct FrameRenderer {
    // Declared in drop order: per-frame state, then the swapchain, then the
    // device everything was built from.
    slots: Vec<Slot>,
    /// One per swapchain **image**, not per in-flight frame.
    ///
    /// Present waits on this semaphore and there is no way to observe when it is
    /// done with it, so it cannot be reused on a schedule the application picks.
    /// Tying it to the image is what makes reuse safe: an image is only handed
    /// back by `acquire` once the presentation engine has finished with it, and
    /// that is the same event that releases the semaphore.
    render_finished: Vec<BinarySemaphore>,
    timeline: TimelineSemaphore,
    swapchain: Swapchain,
    present_queue: vk::Queue,
    /// Resolved once at construction rather than per submit — see
    /// `RenderError::NoPresentQueue` on why the fallback that used to be here
    /// was wrong. Now unused directly: `Device::submit_graphics` picks it.
    #[expect(dead_code, reason = "kept so the resolution stays explicit")]
    graphics_queue: vk::Queue,
    acquire_timeout: Duration,
    slot_index: usize,
    frame_number: u64,
    /// Set when the swapchain is known to be stale; cleared by `prepare`.
    stale: bool,
    device: Arc<Device>,
}

impl FrameRenderer {
    /// Build a renderer for `surface` at `size`.
    ///
    /// # Errors
    ///
    /// [`RenderError::NoPresentQueue`] if the device was created without a
    /// surface, [`RenderError::NoFramesInFlight`] for a zero frame count, and
    /// [`RenderError::Rhi`] if any GPU object cannot be created.
    pub fn new(
        device: &Arc<Device>,
        surface: &Surface,
        size: vk::Extent2D,
        config: &FrameRendererConfig,
    ) -> Result<Self, RenderError> {
        if config.frames_in_flight == 0 {
            return Err(RenderError::NoFramesInFlight);
        }

        // Resolved once, here, rather than at every present. The examples wrote
        // `present.unwrap_or(graphics)`, which silently does the wrong thing on
        // a device whose families differ.
        let present_queue = device.queues().present.ok_or(RenderError::NoPresentQueue)?;

        let swapchain = Swapchain::new(
            device,
            surface,
            &SwapchainConfig {
                present_mode: config.present_mode,
                extent: size,
            },
        )?;

        let graphics_family = device.queue_families().graphics;
        let mut slots = Vec::with_capacity(config.frames_in_flight);

        for _ in 0..config.frames_in_flight {
            let pool = CommandPool::new(device, graphics_family)?;
            let command = pool
                .allocate(1)?
                .pop()
                .expect("one command buffer was requested");

            slots.push(Slot {
                pool,
                command,
                acquire: BinarySemaphore::new(device)?,
                signalled: 0,
            });
        }

        let render_finished = Self::semaphores_for(device, &swapchain)?;

        Ok(Self {
            slots,
            render_finished,
            timeline: TimelineSemaphore::new(device, 0)?,
            swapchain,
            present_queue,
            graphics_queue: device.queues().graphics,
            acquire_timeout: config.acquire_timeout,
            slot_index: 0,
            frame_number: 0,
            stale: false,
            device: Arc::clone(device),
        })
    }

    /// The size frames are currently being drawn at.
    pub fn extent(&self) -> vk::Extent2D {
        self.swapchain.extent()
    }

    /// The colour format a pipeline must be built against.
    pub fn format(&self) -> vk::Format {
        self.swapchain.format()
    }

    /// How many frames have been presented.
    pub fn frame_number(&self) -> u64 {
        self.frame_number
    }

    /// Mark the swapchain as needing recreation, after a resize.
    ///
    /// Separate from [`FrameRenderer::prepare`] because the window event and the
    /// frame are different moments: a resize arrives whenever the compositor
    /// says so, and rebuilding a swapchain in the middle of that is wasted work
    /// when three more resize events are queued behind it.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Recreate the swapchain if it is stale, reporting the new size.
    ///
    /// Returns `Some(extent)` when something was rebuilt, so a caller can resize
    /// the resources that have to match — a depth buffer above all, where a
    /// mismatch is a validation error on the first frame after a resize.
    ///
    /// **Call this before [`FrameRenderer::render`], not after.** Attachments
    /// have to agree with the target *while it is being recorded*, so a caller
    /// told about a resize afterwards would draw one wrong frame first.
    ///
    /// A zero-sized window — minimised, on Windows — leaves the swapchain alone
    /// and stays stale, because zero is not a valid extent. The next call with a
    /// real size rebuilds.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the swapchain or its semaphores cannot be built.
    pub fn prepare(
        &mut self,
        surface: &Surface,
        size: vk::Extent2D,
    ) -> Result<Option<vk::Extent2D>, RenderError> {
        if !self.stale {
            return Ok(None);
        }

        if size.width == 0 || size.height == 0 {
            return Ok(None);
        }

        self.swapchain.recreate(surface, size)?;

        // The image count can change on recreation, so these are rebuilt rather
        // than reused. Reusing them across a recreation that grew the swapchain
        // would leave the new images sharing a semaphore with an old one.
        self.render_finished = Self::semaphores_for(&self.device, &self.swapchain)?;
        self.stale = false;

        Ok(Some(self.swapchain.extent()))
    }

    /// Record and present one frame.
    ///
    /// `record` is handed a command buffer that has already been begun and is
    /// ended afterwards, so it only issues draws. It cannot fail: recording is
    /// `vkCmd*` calls into an already-allocated buffer, with nothing fallible
    /// left to do by the time it runs. Anything that *can* fail — creating a
    /// pipeline, uploading a mesh — belongs before the frame, not inside it.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the GPU rejects the submission or the device is
    /// lost. A swapchain that merely needs recreating is
    /// [`FrameOutcome::Skipped`], not an error.
    pub fn render(&mut self, record: impl FnOnce(&Frame<'_>)) -> Result<FrameOutcome, RenderError> {
        let slot_index = self.slot_index;

        // Before the pool is touched: this slot's previous submission may still
        // be executing, and resetting a pool whose buffers are pending is
        // undefined. This is the whole reason the timeline exists.
        self.timeline
            .wait_forever(self.slots[slot_index].signalled)?;
        self.slots[slot_index].pool.reset()?;

        let acquired = self
            .swapchain
            .acquire_next_image(&self.slots[slot_index].acquire, self.acquire_timeout)?;

        let image_index = match acquired {
            AcquireOutcome::Acquired { index, suboptimal } => {
                if suboptimal {
                    self.stale = true;
                }

                index
            }
            AcquireOutcome::OutOfDate => {
                self.stale = true;
                return Ok(FrameOutcome::Skipped);
            }
            AcquireOutcome::TimedOut => return Ok(FrameOutcome::Skipped),
        };

        let image = image_index as usize;
        let command = &self.slots[slot_index].command;

        command.begin()?;
        record(&Frame {
            command,
            target: Target {
                image: self.swapchain.images()[image],
                view: self.swapchain.views()[image],
                extent: self.swapchain.extent(),
                // The frame clears, so the previous contents are worth nothing
                // and discarding them is faster than preserving them.
                from: ImageState::UNDEFINED,
                to: ImageState::PRESENT,
            },
            number: self.frame_number,
            slot: slot_index,
            slots: self.slots.len(),
        });
        command.end()?;

        let signalled = self.frame_number + 1;
        self.submit(slot_index, image, signalled)?;

        self.slots[slot_index].signalled = signalled;
        self.frame_number = signalled;

        let outcome = self.swapchain.present(
            self.present_queue,
            image_index,
            &self.render_finished[image],
        )?;

        if matches!(
            outcome,
            PresentOutcome::OutOfDate | PresentOutcome::Suboptimal
        ) {
            self.stale = true;
        }

        self.slot_index = (self.slot_index + 1) % self.slots.len();

        Ok(FrameOutcome::Presented)
    }

    /// Submit the recorded buffer, signalling both the per-image semaphore that
    /// present waits on and the timeline value this slot is reused after.
    fn submit(&self, slot: usize, image: usize, signalled: u64) -> Result<(), RenderError> {
        self.device.submit_graphics(&slop_rhi::Submission {
            wait: &[(
                self.slots[slot].acquire.handle(),
                // At the colour-attachment stage rather than the top of the
                // pipe: vertex work has no reason to wait for an image it never
                // touches.
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            )],
            signal: &[self.render_finished[image].handle()],
            signal_timeline: &[(self.timeline.handle(), signalled)],
            command: &self.slots[slot].command,
        })?;

        Ok(())
    }

    /// One binary semaphore per swapchain image.
    fn semaphores_for(
        device: &Arc<Device>,
        swapchain: &Swapchain,
    ) -> Result<Vec<BinarySemaphore>, RenderError> {
        (0..swapchain.images().len())
            .map(|_| BinarySemaphore::new(device).map_err(RenderError::from))
            .collect()
    }
}

impl Drop for FrameRenderer {
    fn drop(&mut self) {
        // Before any field drops. `Device::drop` waits too, but by then the
        // pools and semaphores declared above it are already destroyed, and
        // destroying a semaphore a pending submission still references is
        // undefined.
        if let Err(failure) = self.device.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}

impl std::fmt::Debug for FrameRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameRenderer")
            .field("extent", &self.swapchain.extent())
            .field("frames_in_flight", &self.slots.len())
            .field("images", &self.render_finished.len())
            .field("frame_number", &self.frame_number)
            .field("stale", &self.stale)
            .finish()
    }
}
