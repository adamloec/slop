//! M0 task F: the first render.
//!
//! Run `cargo run -p slop-cli -- cook` first, then `cargo run -p example-triangle`.
//!
//! This file owns `main()` and drives the frame loop itself, per
//! `docs/DESIGN.md` §1.2 principle 4. The engine supplies primitives; the render
//! loop's eventual shape is `slop-render`'s job at M3, and inventing it here
//! would be designing against imagined requirements
//! (`docs/PLAN.md` §4.1-D).
//!
//! # The two synchronization subtleties
//!
//! **Acquire returns an index before the image is usable.** The presentation
//! engine may still be reading it, so rendering waits on the acquire semaphore
//! at the colour-attachment stage rather than starting immediately. Skipping
//! this produces flicker that looks like a driver bug.
//!
//! **Render-finished semaphores are per swapchain image, not per frame in
//! flight.** Present waits on one, and there is no way to observe when present
//! is done with it — so a per-frame semaphore could be signalled again while a
//! previous present still waits on it. One per image sidesteps that entirely.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use slop_core::diagnostics::tracing::{error, info};

use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::{Window, WindowId};
use slop_rhi::{
    AcquireOutcome, BinarySemaphore, CommandBuffer, CommandPool, Device, DeviceSelection,
    GraphicsPipeline, GraphicsPipelineConfig, ImageState, Instance, InstanceConfig, PipelineLayout,
    PresentMode, PresentOutcome, ShaderModule, ShaderStage, Surface, Swapchain, SwapchainConfig,
    TimelineSemaphore, vk,
};

/// How many frames the CPU may prepare ahead of the GPU.
///
/// Two is the standard trade: enough to keep both busy, few enough that input
/// latency stays low. `docs/DESIGN.md` §2.9's snapshot is what would eventually
/// let this rise without the simulation and renderer fighting over state.
const FRAMES_IN_FLIGHT: usize = 2;

fn main() {
    slop_app::logging::init();

    let event_loop = EventLoop::new().expect("an event loop must be creatable");
    let mut app = App {
        frame_limit: std::env::var("SLOP_FRAMES")
            .ok()
            .and_then(|value| value.parse().ok()),
        ..Default::default()
    };

    event_loop.run_app(&mut app).expect("the event loop failed");

    if let Some(failure) = &app.failure {
        error!(error = %failure, "the renderer failed");
    }

    // Dropped explicitly so shutdown finishes — and logs that it finished —
    // before the process exits. Letting it fall out of scope after the exit
    // check would work, but "shutdown complete" is only trustworthy if it is
    // printed after the teardown it describes.
    let failed = app.failure.is_some();
    drop(app);

    info!("shutdown complete");

    if failed {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct App {
    renderer: Option<Renderer>,
    failure: Option<String>,
    /// Exit after this many frames, from `SLOP_FRAMES`.
    frame_limit: Option<u64>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        match Renderer::new(event_loop) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => renderer.mark_dirty(),
            WindowEvent::RedrawRequested => {
                if let Err(error) = renderer.render() {
                    self.failure = Some(error);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };

        // `SLOP_FRAMES=n` exits after n frames. Makes shutdown verifiable
        // without a human closing a window, and is the shape the deterministic
        // headless mode in `docs/DESIGN.md` §5 needs — run a fixed number of
        // frames, then stop.
        if let Some(limit) = self.frame_limit
            && renderer.frame_counter >= limit
        {
            println!("rendered {limit} frames; exiting");
            // Cleared so this fires once: `about_to_wait` runs again before the
            // loop actually unwinds.
            self.frame_limit = None;
            event_loop.exit();
            return;
        }

        // Drive continuously rather than only on damage, so the frame loop is
        // exercised the way a game's would be.
        renderer.window.request_redraw();
    }
}

/// Everything one frame in flight needs of its own.
struct Frame {
    pool: CommandPool,
    command: CommandBuffer,
    /// Signalled by the presentation engine when its image is ready to write.
    acquire: BinarySemaphore,
    /// The timeline value this frame's submission will signal. Waiting on it is
    /// what makes reusing the pool safe.
    signalled: u64,
}

struct Renderer {
    // Declared in drop order: everything built from the device, then the
    // device, then the surface, then the window it came from.
    frames: Vec<Frame>,
    /// One per swapchain image — see the module docs.
    render_finished: Vec<BinarySemaphore>,
    timeline: TimelineSemaphore,
    // No separate layout field: `GraphicsPipeline` already holds an `Arc` to
    // it, which is what keeps it alive.
    pipeline: GraphicsPipeline,
    swapchain: Swapchain,
    device: Arc<Device>,
    surface: Surface,
    window: Window,

    frame_index: usize,
    frame_counter: u64,
    /// Set when the swapchain is known to no longer match the window.
    dirty: bool,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let window = window::create(
            event_loop,
            &WindowConfig {
                title: String::from("slop — triangle"),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;

        let extensions =
            window::required_instance_extensions(&window).map_err(|error| error.to_string())?;
        let instance = Arc::new(
            Instance::new(&InstanceConfig {
                application_name: String::from("example-triangle"),
                required_extensions: extensions,
                ..Default::default()
            })
            .map_err(|error| error.to_string())?,
        );

        // SAFETY: `window` is moved into the returned `Renderer` after the
        // surface and is declared last, so it outlives everything built here.
        let surface =
            unsafe { window::create_surface(&instance, &window) }.map_err(|e| e.to_string())?;

        let devices =
            slop_rhi::enumerate(&instance, Some(&surface)).map_err(|error| error.to_string())?;
        let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
            .map_err(|error| error.to_string())?;
        let device = Arc::new(Device::new(&instance, &devices[chosen]).map_err(|e| e.to_string())?);

        let size = window.inner_size();
        let swapchain = Swapchain::new(
            &device,
            &surface,
            &SwapchainConfig {
                present_mode: PresentMode::Mailbox,
                extent: vk::Extent2D {
                    width: size.width,
                    height: size.height,
                },
            },
        )
        .map_err(|error| error.to_string())?;

        let module = load_shader(&device)?;
        let layout = Arc::new(PipelineLayout::empty(&device).map_err(|e| e.to_string())?);
        let pipeline = GraphicsPipeline::new(
            &device,
            &layout,
            &GraphicsPipelineConfig {
                vertex: ShaderStage {
                    module: &module,
                    entry: c"vertexMain",
                },
                fragment: ShaderStage {
                    module: &module,
                    entry: c"fragmentMain",
                },
                color_format: swapchain.format(),
                // No depth: the triangle is a single flat primitive with nothing to
                // occlude it. Depth arrives with the cube.
                depth_format: None,
                // On, deliberately. This is the check that the shader agrees
                // with the engine's counter-clockwise front face: a triangle
                // wound the wrong way vanishes silently, with no validation
                // complaint, so leaving culling off would let the convention rot
                // unnoticed until real geometry made it expensive.
                cull_back_faces: true,
            },
        )
        .map_err(|error| error.to_string())?;

        // The module may be dropped now — Vulkan does not require it to outlive
        // the pipelines built from it.
        drop(module);

        let graphics_family = device.queue_families().graphics;
        let mut frames = Vec::with_capacity(FRAMES_IN_FLIGHT);

        for _ in 0..FRAMES_IN_FLIGHT {
            let pool = CommandPool::new(&device, graphics_family).map_err(|e| e.to_string())?;
            let command = pool
                .allocate(1)
                .map_err(|error| error.to_string())?
                .pop()
                .expect("one buffer was requested");

            frames.push(Frame {
                pool,
                command,
                acquire: BinarySemaphore::new(&device).map_err(|e| e.to_string())?,
                // Zero: the timeline starts there, so the first wait is
                // satisfied immediately rather than deadlocking.
                signalled: 0,
            });
        }

        let render_finished = (0..swapchain.images().len())
            .map(|_| BinarySemaphore::new(&device))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;

        println!(
            "triangle: {}x{}, {} swapchain images, {} frames in flight",
            swapchain.extent().width,
            swapchain.extent().height,
            swapchain.images().len(),
            FRAMES_IN_FLIGHT,
        );

        Ok(Self {
            frames,
            render_finished,
            timeline: TimelineSemaphore::new(&device, 0).map_err(|e| e.to_string())?,
            pipeline,
            swapchain,
            device,
            surface,
            window,
            frame_index: 0,
            frame_counter: 0,
            dirty: false,
        })
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn render(&mut self) -> Result<(), String> {
        if self.dirty {
            self.recreate_swapchain()?;
        }

        let frame_index = self.frame_index;

        // Wait for this frame slot's previous submission before touching its
        // pool. This is the whole reason the timeline exists.
        self.timeline
            .wait_forever(self.frames[frame_index].signalled)
            .map_err(|error| error.to_string())?;

        self.frames[frame_index]
            .pool
            .reset()
            .map_err(|error| error.to_string())?;

        let acquired = self
            .swapchain
            .acquire_next_image(&self.frames[frame_index].acquire, Duration::from_secs(1))
            .map_err(|error| error.to_string())?;

        let image_index = match acquired {
            AcquireOutcome::Acquired { index, suboptimal } => {
                if suboptimal {
                    self.dirty = true;
                }
                index
            }
            AcquireOutcome::OutOfDate => {
                self.recreate_swapchain()?;
                return Ok(());
            }
            AcquireOutcome::TimedOut => return Ok(()),
        };

        self.record(frame_index, image_index)?;

        self.frame_counter += 1;
        let signalled = self.frame_counter;
        self.submit(frame_index, image_index, signalled)?;
        self.frames[frame_index].signalled = signalled;

        let outcome = self
            .swapchain
            .present(
                self.device
                    .queues()
                    .present
                    .unwrap_or(self.device.queues().graphics),
                image_index,
                &self.render_finished[image_index as usize],
            )
            .map_err(|error| error.to_string())?;

        if matches!(
            outcome,
            PresentOutcome::OutOfDate | PresentOutcome::Suboptimal
        ) {
            self.dirty = true;
        }

        self.frame_index = (self.frame_index + 1) % FRAMES_IN_FLIGHT;

        Ok(())
    }

    fn record(&self, frame_index: usize, image_index: u32) -> Result<(), String> {
        let command = &self.frames[frame_index].command;
        let extent = self.swapchain.extent();
        let image = self.swapchain.images()[image_index as usize];
        let view = self.swapchain.views()[image_index as usize];

        command.begin().map_err(|error| error.to_string())?;

        // From UNDEFINED, not from PRESENT_SRC: the previous contents are about
        // to be cleared, so discarding is both correct and faster.
        command.transition_image(
            image,
            vk::ImageAspectFlags::COLOR,
            ImageState::UNDEFINED,
            ImageState::COLOR_ATTACHMENT,
        );

        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.02, 0.02, 0.03, 1.0],
            },
        };
        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(clear)];

        let rendering = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            })
            .layer_count(1)
            .color_attachments(&attachments);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        }];

        let raw = self.device.raw();
        let buffer = command.handle();

        // SAFETY: the buffer is recording, every borrowed structure outlives
        // these calls, and `dynamic_rendering` is in the required feature tier.
        unsafe {
            raw.cmd_begin_rendering(buffer, &rendering);
            raw.cmd_set_viewport(buffer, 0, &viewports);
            raw.cmd_set_scissor(buffer, 0, &scissors);
            raw.cmd_bind_pipeline(
                buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.handle(),
            );
            // Three vertices, one instance. Positions come from SV_VertexID, so
            // there is nothing to bind.
            raw.cmd_draw(buffer, 3, 1, 0, 0);
            raw.cmd_end_rendering(buffer);
        }

        command.transition_image(
            image,
            vk::ImageAspectFlags::COLOR,
            ImageState::COLOR_ATTACHMENT,
            ImageState::PRESENT,
        );
        command.end().map_err(|error| error.to_string())?;

        Ok(())
    }

    fn submit(&self, frame_index: usize, image_index: u32, signalled: u64) -> Result<(), String> {
        let wait = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.frames[frame_index].acquire.handle())
            // Wait at the colour-attachment stage, not the top of the pipe:
            // vertex work may begin before the image is available, since it
            // touches nothing the presentation engine is reading.
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];

        let signal = [
            // Binary, for present — the swapchain accepts nothing else.
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.render_finished[image_index as usize].handle())
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            // Timeline, for frame pacing.
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.timeline.handle())
                .value(signalled)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        ];

        let commands = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(self.frames[frame_index].command.handle())];

        let submits = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait)
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signal)];

        // SAFETY: the buffer is recorded and not pending, every semaphore
        // belongs to this device, and the borrowed arrays outlive the call.
        unsafe {
            self.device.raw().queue_submit2(
                self.device.queues().graphics,
                &submits,
                vk::Fence::null(),
            )
        }
        .map_err(|error| error.to_string())
    }

    fn recreate_swapchain(&mut self) -> Result<(), String> {
        let size = self.window.inner_size();

        // Minimising produces a zero extent, which is not a valid swapchain.
        // Skipping rather than failing is correct: the window will come back.
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }

        self.swapchain
            .recreate(
                &self.surface,
                vk::Extent2D {
                    width: size.width,
                    height: size.height,
                },
            )
            .map_err(|error| error.to_string())?;

        // Image count can change on recreation, so the per-image semaphores are
        // rebuilt rather than assumed still to match.
        self.render_finished = (0..self.swapchain.images().len())
            .map(|_| BinarySemaphore::new(&self.device))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;

        self.dirty = false;

        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        info!(frames = self.frame_counter, "shutting down");

        // Every Vulkan object below is destroyed when this struct's fields drop,
        // which happens *after* this function returns — and the GPU may still be
        // executing the last submitted frame.
        //
        // `Device::drop` also waits, but that is far too late: the device field
        // is declared after the pools and semaphores, so those are already
        // destroyed by the time it runs. Waiting here, before any field drops,
        // is what actually makes teardown safe.
        if let Err(failure) = self.device.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}

/// Load the cooked triangle shader.
///
/// Dev-only path resolution — the asset VFS at M2 replaces this. Hard-coding it
/// is honest about being a placeholder rather than pretending to be a lookup.
fn load_shader(device: &Arc<Device>) -> Result<ShaderModule, String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(".slop/cache/shaders/passes/triangle.spv");

    let bytes = std::fs::read(&path).map_err(|error| {
        format!(
            "{} could not be read ({error}). Run `cargo run -p slop-cli -- cook` first",
            path.display()
        )
    })?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}
