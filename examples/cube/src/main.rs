//! The windowed cube — `docs/PLAN.md` §4.2's exit criterion, on screen.
//!
//! Run `cargo run -p slop-cli -- cook` first, then `cargo run -p example-cube`.
//! Add `cook --watch` in a second terminal and editing `assets/checker.png` or
//! `assets/cube.gltf` changes the window without a restart.
//! `SLOP_FRAMES=n` exits after n frames, which is how shutdown gets verified
//! without a human closing a window.
//!
//! The scene itself lives in this crate's library, shared with the headless
//! golden test. This file owns `main()` and the frame loop, per
//! `docs/DESIGN.md` §1.2 principle 4 — the engine supplies pieces, it does not
//! supply a framework to sit inside.
//!
//! **This loop is duplicated in `examples/triangle/src/main.rs`**, and that is a
//! known and deliberate cost — `docs/PLAN.md` §6.1 records why it is being left
//! until M3 rather than lifted into `slop-app` now, and what would change that
//! decision. Both copies are deleted when the frame renderer lands. A **third**
//! copy is the signal to extract it early; do not add one silently.

use std::sync::Arc;
use std::time::{Duration, Instant};

use example_cube::{Scene, Target};
use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::{Window, WindowId};
use slop_core::diagnostics::tracing::{error, info};
use slop_rhi::{
    AcquireOutcome, Allocator, BinarySemaphore, CommandBuffer, CommandPool, Device,
    DeviceSelection, ImageState, Instance, InstanceConfig, PresentMode, PresentOutcome, Surface,
    Swapchain, SwapchainConfig, TimelineSemaphore, vk,
};

/// How many frames the CPU may prepare ahead of the GPU.
const FRAMES_IN_FLIGHT: usize = 2;

/// How often to check whether a cooked asset has been rewritten.
///
/// See [`Renderer::poll_for_reloaded_assets`] for why this is throttled at all.
const ASSET_POLL_INTERVAL: Duration = Duration::from_millis(80);

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
            WindowEvent::Resized(_) => renderer.dirty = true,
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

        if let Some(limit) = self.frame_limit
            && renderer.frame_counter >= limit
        {
            println!("rendered {limit} frames; exiting");
            self.frame_limit = None;
            event_loop.exit();
            return;
        }

        renderer.window.request_redraw();
    }
}

/// Everything one frame in flight needs of its own.
struct Frame {
    pool: CommandPool,
    command: CommandBuffer,
    acquire: BinarySemaphore,
    signalled: u64,
}

struct Renderer {
    // Declared in drop order: the scene and per-frame state first, then the
    // allocator that owns their memory, then the device, surface and window.
    frames: Vec<Frame>,
    /// One per swapchain image, not per frame in flight — present waits on one
    /// and there is no way to observe when it is done with it.
    render_finished: Vec<BinarySemaphore>,
    timeline: TimelineSemaphore,
    scene: Scene,
    swapchain: Swapchain,
    allocator: Arc<Allocator>,
    device: Arc<Device>,
    surface: Surface,
    window: Window,

    frame_index: usize,
    frame_counter: u64,
    dirty: bool,
    /// When assets were last checked for changes. See `poll_for_reloaded_assets`.
    last_asset_poll: Instant,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let window = window::create(
            event_loop,
            &WindowConfig {
                title: String::from("slop — cube"),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;

        let extensions =
            window::required_instance_extensions(&window).map_err(|error| error.to_string())?;
        let instance = Arc::new(
            Instance::new(&InstanceConfig {
                application_name: String::from("example-cube"),
                required_extensions: extensions,
                ..Default::default()
            })
            .map_err(|error| error.to_string())?,
        );

        // SAFETY: `window` is moved into the returned `Renderer` and declared
        // last, so it outlives everything built from it.
        let surface =
            unsafe { window::create_surface(&instance, &window) }.map_err(|e| e.to_string())?;

        let devices =
            slop_rhi::enumerate(&instance, Some(&surface)).map_err(|error| error.to_string())?;
        let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
            .map_err(|error| error.to_string())?;
        let device = Arc::new(Device::new(&instance, &devices[chosen]).map_err(|e| e.to_string())?);
        let allocator = Allocator::new(&device).map_err(|error| error.to_string())?;

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

        let scene = Scene::new(&device, &allocator, swapchain.extent(), swapchain.format())?;

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
                signalled: 0,
            });
        }

        let render_finished = (0..swapchain.images().len())
            .map(|_| BinarySemaphore::new(&device))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;

        println!(
            "cube: {}x{}, {} swapchain images, {} frames in flight",
            swapchain.extent().width,
            swapchain.extent().height,
            swapchain.images().len(),
            FRAMES_IN_FLIGHT,
        );

        Ok(Self {
            frames,
            render_finished,
            timeline: TimelineSemaphore::new(&device, 0).map_err(|e| e.to_string())?,
            scene,
            swapchain,
            allocator,
            device,
            surface,
            window,
            frame_index: 0,
            frame_counter: 0,
            dirty: false,
            last_asset_poll: Instant::now(),
        })
    }

    /// Pick up any asset that has been recooked since the last check.
    ///
    /// Run `cargo run -p slop-cli -- cook --watch` beside this and editing
    /// `assets/checker.png` or `assets/cube.gltf` changes the window without a
    /// restart. Two processes on purpose: `docs/DESIGN.md` §2.8 keeps source
    /// parsing out of anything that ships, so this binary never links a shader
    /// compiler or a glTF parser — it only notices that cooked bytes changed.
    ///
    /// Only the windowed demo does this. The golden test renders by frame number
    /// and has to stay a pure function of it (§2.14), so it must not be able to
    /// race a file on disk.
    ///
    /// Throttled because the check is a `stat` per loaded asset, and at several
    /// thousand frames a second that is thousands of syscalls a second to
    /// discover nothing. A twelfth of a second is far below what a human notices
    /// between saving a file and alt-tabbing.
    fn poll_for_reloaded_assets(&mut self) -> Result<(), String> {
        if self.last_asset_poll.elapsed() < ASSET_POLL_INTERVAL {
            return Ok(());
        }

        self.last_asset_poll = Instant::now();
        self.scene.reload_changed()?;

        Ok(())
    }

    fn render(&mut self) -> Result<(), String> {
        if self.dirty {
            self.recreate_swapchain()?;
        }

        self.poll_for_reloaded_assets()?;

        let frame_index = self.frame_index;

        // Wait for this slot's previous submission before touching its pool.
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

        let command = &self.frames[frame_index].command;
        command.begin().map_err(|error| error.to_string())?;

        self.scene.record(
            command,
            Target {
                image: self.swapchain.images()[image_index as usize],
                view: self.swapchain.views()[image_index as usize],
                extent: self.swapchain.extent(),
                // The frame clears, so the previous contents are worth nothing
                // and discarding is faster than preserving them.
                from: ImageState::UNDEFINED,
                to: ImageState::PRESENT,
            },
            self.frame_counter,
        );

        command.end().map_err(|error| error.to_string())?;

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

    fn submit(&self, frame_index: usize, image_index: u32, signalled: u64) -> Result<(), String> {
        let wait = [vk::SemaphoreSubmitInfo::default()
            .semaphore(self.frames[frame_index].acquire.handle())
            // At the colour-attachment stage, not the top of the pipe: vertex
            // work may begin before the image is available.
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];

        let signal = [
            vk::SemaphoreSubmitInfo::default()
                .semaphore(self.render_finished[image_index as usize].handle())
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
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

        // The depth buffer must match the colour target's size, so it is
        // rebuilt too. Forgetting this is a validation error on the first frame
        // after a resize.
        self.scene
            .resize(&self.allocator, self.swapchain.extent())?;

        // Image count can change on recreation.
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

        // Before any field drops — `Device::drop` waits too, but by then the
        // pools and semaphores declared above it are already destroyed.
        if let Err(failure) = self.device.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}
