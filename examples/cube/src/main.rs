//! The windowed cube — `docs/PLAN.md` §4.2's exit criterion, on screen.
//!
//! Run `cargo run -p slop-cli -- cook` first, then `cargo run -p example-cube`.
//! Add `cook --watch` in a second terminal and editing `assets/checker.png` or
//! `assets/cube.gltf` changes the window without a restart.
//! `SLOP_FRAMES=n` exits after n frames, which is how shutdown gets verified
//! without a human closing a window.
//!
//! The scene itself lives in this crate's library, shared with the headless
//! golden test. This file owns `main()` and the event loop, per
//! `docs/DESIGN.md` §1.2 principle 4 — the engine supplies pieces, it does not
//! supply a framework to sit inside.
//!
//! The *frame* loop is no longer here. Acquire, submit, present and frames in
//! flight belong to `slop_render::FrameRenderer`, which is what this file and
//! `examples/triangle` used to hold a copy of each. What is left is genuinely an
//! application's: a window, a device, a scene, and the decision of when to poll
//! for reloaded assets.
//!
//! Two calls rather than one, and the order matters:
//! [`FrameRenderer::prepare`] reports a resize so the depth buffer can be
//! rebuilt, and [`FrameRenderer::render`] then records against a target that
//! agrees with it. Doing them the other way round draws one wrong frame after
//! every resize.

use std::sync::Arc;
use std::time::{Duration, Instant};

use example_cube::Scene;
use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::{Window, WindowId};
use slop_core::diagnostics::tracing::{error, info};
use slop_render::{FrameRenderer, FrameRendererConfig};
use slop_rhi::{Allocator, Device, DeviceSelection, Instance, InstanceConfig, Surface, vk};

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
            WindowEvent::Resized(_) => renderer.renderer.invalidate(),
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
            && renderer.frame_number() >= limit
        {
            println!("rendered {limit} frames; exiting");
            self.frame_limit = None;
            event_loop.exit();
            return;
        }

        renderer.window.request_redraw();
    }
}

struct Renderer {
    // Declared in drop order: the scene and the frame renderer first, then the
    // allocator that owns their memory, then the device, surface and window.
    scene: Scene,
    renderer: FrameRenderer,
    allocator: Arc<Allocator>,
    device: Arc<Device>,
    surface: Surface,
    window: Window,
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

        let renderer = FrameRenderer::new(
            &device,
            &surface,
            window_extent(&window),
            &FrameRendererConfig::default(),
        )
        .map_err(|error| error.to_string())?;

        let scene = Scene::new(&device, &allocator, renderer.extent(), renderer.format())?;

        info!(
            width = renderer.extent().width,
            height = renderer.extent().height,
            "cube ready"
        );

        Ok(Self {
            scene,
            renderer,
            allocator,
            device,
            surface,
            window,
            last_asset_poll: Instant::now(),
        })
    }

    fn frame_number(&self) -> u64 {
        self.renderer.frame_number()
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
        // Before the frame, not after: the depth buffer has to agree with the
        // colour target while the frame is being recorded, so a resize noticed
        // afterwards would draw one wrong frame first.
        if let Some(extent) = self
            .renderer
            .prepare(&self.surface, window_extent(&self.window))
            .map_err(|error| error.to_string())?
        {
            self.scene.resize(&self.allocator, extent)?;
        }

        self.poll_for_reloaded_assets()?;

        // Borrowed out of `self` so the closure does not capture it whole —
        // `render` needs `&mut self.renderer` at the same time.
        let scene = &self.scene;

        self.renderer
            .render(|frame| scene.record(frame.command, frame.target, frame.number))
            .map_err(|error| error.to_string())?;

        Ok(())
    }
}

/// The window's size, in the form Vulkan wants.
fn window_extent(window: &Window) -> vk::Extent2D {
    let size = window.inner_size();

    vk::Extent2D {
        width: size.width,
        height: size.height,
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        info!(frames = self.renderer.frame_number(), "shutting down");

        // Before any field drops. `FrameRenderer::drop` waits too, but it drops
        // after the scene, and destroying the scene's images while a frame that
        // samples them is still executing is undefined.
        if let Err(failure) = self.device.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}
