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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use example_cube::Scene;
use slop_app::gpu::{Gpu, GpuConfig};
use slop_app::timing::FrameTimes;
use slop_app::window::WindowConfig;
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};
use slop_editor::{DebugUi, Declared};
use slop_render::{FrameRenderer, FrameRendererConfig};

/// The repository root, which is where `.slop/cache` lives.
///
/// `CARGO_MANIFEST_DIR` is example-grade and does not survive being installed —
/// `docs/PLAN.md` §6.1 has the row for it.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

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

        // The overlay sees every event first and reports whether it wants
        // exclusive use of it — a click on a panel is the interface's, a click
        // on the scene behind it is the game's. Without this the overlay renders
        // and cannot be touched, which is what it did when it first landed.
        //
        // `consumed` is deliberately ignored below rather than unused: the cube
        // has no camera or picking to suppress yet, and pretending otherwise
        // would be writing the branch before there is anything on the other side
        // of it.
        let _consumed = renderer.ui.on_window_event(renderer.gpu.window(), &event);

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

        renderer.gpu.window().request_redraw();
    }
}

struct Renderer {
    // Declared in drop order: the scene and the frame renderer first, then the
    // `Gpu` holding the allocator, device, surface and window their memory and
    // handles came from.
    scene: Scene,
    renderer: FrameRenderer,
    gpu: Gpu,
    /// When assets were last checked for changes. See `poll_for_reloaded_assets`.
    last_asset_poll: Instant,
    /// How long recent frames took. See `FrameTimes`.
    frame_times: FrameTimes,
    /// The debug UI — egui's state, the winit glue, and the overlay renderer.
    ///
    /// In `slop-app` rather than here or in `slop-render`: the renderer stays
    /// windowing-agnostic and only ever sees tessellated triangles, and the
    /// wiring between the two is identical in every application.
    ui: DebugUi,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let gpu = Gpu::new(
            event_loop,
            &GpuConfig {
                window: WindowConfig {
                    title: String::from("slop — cube"),
                    ..Default::default()
                },
                application_name: String::from("example-cube"),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;

        let renderer = FrameRenderer::new(
            gpu.device(),
            gpu.surface(),
            gpu.extent(),
            &FrameRendererConfig::default(),
        )
        .map_err(|error| error.to_string())?;

        let scene = Scene::new(
            gpu.device(),
            gpu.allocator(),
            renderer.extent(),
            renderer.format(),
        )?;

        info!(
            width = renderer.extent().width,
            height = renderer.extent().height,
            "cube ready"
        );

        // Into the scene's heap, so the font atlas sits in the same table as the
        // cube's texture — which is what a bindless model is for.
        let mut scene = scene;
        let ui = DebugUi::new(
            gpu.window(),
            gpu.device(),
            scene.heap_mut(),
            &Vfs::for_project(&project_root()),
            renderer.format(),
        )
        .map_err(|error| error.to_string())?;

        Ok(Self {
            scene,
            renderer,
            frame_times: FrameTimes::default(),
            ui,
            gpu,
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
        self.frame_times.tick();

        // Before the frame, not after: the depth buffer has to agree with the
        // colour target while the frame is being recorded, so a resize noticed
        // afterwards would draw one wrong frame first.
        if let Some(extent) = self
            .renderer
            .prepare(self.gpu.surface(), self.gpu.extent())
            .map_err(|error| error.to_string())?
        {
            self.scene.resize(self.gpu.allocator(), extent)?;
        }

        self.poll_for_reloaded_assets()?;

        // The UI is declared, tessellated and its textures uploaded *before* the
        // frame, because uploading waits for the GPU and nothing inside a
        // recorded frame may block on it.
        let declared = self.run_ui()?;

        // Borrowed out of `self` field by field, so the closure does not capture
        // it whole — `render` needs `&mut self.renderer` at the same time.
        let scene = &self.scene;
        let ui = &mut self.ui;
        let allocator = self.gpu.allocator();

        self.renderer
            .render(|frame| {
                scene.record(frame);

                // In a pass of its own, over the scene. Errors are logged rather
                // than propagated: a debug overlay that fails to allocate must
                // not take the frame with it.
                if let Err(failure) = ui.draw(scene.heap(), allocator, frame, &declared) {
                    error!(error = %failure, "the debug overlay did not draw");
                }

                // After everything that draws — see `Frame::finish`.
                frame.finish();
            })
            .map_err(|error| error.to_string())?;

        Ok(())
    }

    /// Declare this frame's debug UI and turn it into triangles.
    ///
    /// The whole interface is re-declared every frame from current state, which
    /// is what immediate mode means (`docs/DESIGN.md` §10.2): there is no widget
    /// tree to keep synchronised, so it cannot fall out of sync with the engine
    /// it is reporting on.
    fn run_ui(&mut self) -> Result<Declared, String> {
        let frames = self.renderer.frame_number();
        let extent = self.renderer.extent();
        let timing = self.frame_times.summary();

        let declared = self.ui.run(self.gpu.window(), |context| {
            slop_editor::egui::Window::new("slop").show(context, |ui| {
                // Milliseconds, not frames per second. See `slop_app::timing`.
                ui.label(format!("{:.2} ms  ({:.0} fps)", timing.last, timing.fps()));
                ui.label(format!(
                    "{:.2} ms  worst of last {}",
                    timing.worst, timing.window
                ));
                ui.separator();
                ui.label(format!("{}x{}", extent.width, extent.height));
                ui.label(format!("frame {frames}"));
                ui.separator();
                ui.label("cook --watch is live; edit assets/checker.png");
            });
        });

        self.ui
            .upload(self.scene.heap_mut(), self.gpu.allocator(), &declared)
            .map_err(|error| error.to_string())?;

        Ok(declared)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        info!(frames = self.renderer.frame_number(), "shutting down");

        // Before any field drops. `FrameRenderer::drop` waits too, but it drops
        // after the scene, and destroying the scene's images while a frame that
        // samples them is still executing is undefined.
        if let Err(failure) = self.gpu.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}
