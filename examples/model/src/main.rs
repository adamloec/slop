//! Rendering a cooked model — any glTF, not one this file knows about.
//!
//! ```text
//! cargo run -p slop-cli -- cook
//! cargo run -p example-model                          # models/cube.model
//! SLOP_MODEL=models/sponza.model cargo run -p example-model
//! ```
//!
//! # How this differs from `examples/cube`
//!
//! The cube is the M0 integration test: one mesh, one hardcoded texture, and a
//! shader written around it. Its golden image is guarded by a reference that
//! predates the content pipeline, which is what makes it an oracle rather than a
//! record of whatever the code currently does — so it is deliberately *not*
//! migrated onto `MeshRenderer`.
//!
//! This one knows nothing about what it draws. It loads whatever the model names
//! — however many meshes, however many materials — and hands the lot to
//! `slop_render::MeshRenderer`. That is the difference between an example that
//! proves the stack works and one that proves the *renderer* works.
//!
//! `SLOP_FRAMES=n` exits after n frames, as the other examples do.

use std::sync::Arc;

use slop_app::gpu::{Gpu, GpuConfig};
use slop_app::timing::FrameTimes;
use slop_app::window::WindowConfig;
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};
use slop_ecs::{Entity, World};
use slop_editor::{DebugUi, InspectorState};
use slop_math::Vec3;
use slop_render::{FrameRenderer, FrameRendererConfig, MeshRenderer};
use slop_rhi::{BindlessHeap, BindlessHeapConfig, Device, ShaderModule};

// Shared with `tests/golden.rs`, so the window and the reference image are
// framed by the same camera — see the library's docs.
use example_model::{DEFAULT_MODEL, OrbitCamera, assets, bounds, camera};

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

        // The UI sees every event first and reports whether it consumed one.
        // Nothing here reads input yet, so the answer is discarded — but a
        // camera control added later must respect it, or a drag on a UI window
        // would also swing the camera behind it.
        let _consumed = renderer.ui.on_window_event(renderer.gpu.window(), &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => renderer.frames.invalidate(),
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
            && renderer.frames.frame_number() >= limit
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
    // Declared in drop order: what draws, then the heap it indexes, then the
    // frame renderer, then the `Gpu` everything was built from.
    meshes: MeshRenderer,
    heap: BindlessHeap,
    frames: FrameRenderer,
    /// Where the model sits, for the camera to look at.
    centre: Vec3,
    /// The world holding the camera, and what the inspector inspects.
    world: World,
    camera: Entity,
    /// Which entity the inspector is showing, across frames.
    inspector: InspectorState,
    /// The orbit angle, accumulated rather than derived from the frame number.
    ///
    /// Derived would be simpler and would ignore the speed being edited: at
    /// frame 5000, halving `radians_per_frame` would jump the camera to the far
    /// side of the model rather than slowing it. Accumulating means a change
    /// takes effect from *now*.
    angle: f32,
    /// The debug overlay, and the timing it reports.
    ui: DebugUi,
    frame_times: FrameTimes,
    /// What was loaded, for the overlay to name.
    model: String,
    gpu: Gpu,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let gpu = Gpu::new(
            event_loop,
            &GpuConfig {
                window: WindowConfig {
                    title: String::from("slop — model"),
                    ..Default::default()
                },
                application_name: String::from("example-model"),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;

        let frames = FrameRenderer::new(
            gpu.device(),
            gpu.surface(),
            gpu.extent(),
            &FrameRendererConfig::default(),
        )
        .map_err(|error| error.to_string())?;

        let mut heap = BindlessHeap::new(gpu.device(), &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;

        let vfs = assets()?;
        let module = load_shader(gpu.device(), &vfs)?;
        let reflection = load_reflection(&vfs)?;

        let mut meshes = MeshRenderer::new(
            gpu.device(),
            &mut heap,
            &module,
            &reflection,
            frames.format(),
            slop_rhi::preferred_depth_format(gpu.device()),
        )
        .map_err(|error| error.to_string())?;

        let logical = std::env::var("SLOP_MODEL").unwrap_or_else(|_| String::from(DEFAULT_MODEL));

        meshes
            .load(gpu.allocator(), &mut heap, &vfs, &logical)
            .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;
        meshes
            .resize(gpu.allocator(), frames.extent())
            .map_err(|error| error.to_string())?;

        let (centre, radius) = bounds(&vfs, &logical);

        // One entity, one component. `with_builtins` registers the primitives
        // that `OrbitCamera`'s fields resolve to; the component's own type is
        // registered from its derived `Reflect` impl.
        let mut world = World::with_builtins();
        world
            .registry_mut()
            .register_native::<OrbitCamera>()
            .map_err(|error| error.to_string())?;

        let camera = world.spawn();
        world
            .insert(camera, OrbitCamera::framing(radius))
            .map_err(|error| error.to_string())?;

        // Built after the meshes so its font atlas lands in the same heap, and
        // before the first frame because the atlas arrives in the *first*
        // texture delta — a UI wired up mid-frame draws nothing at all.
        let ui = DebugUi::new(gpu.window(), gpu.device(), &mut heap, &vfs, frames.format())
            .map_err(|error| error.to_string())?;

        info!(
            model = logical,
            meshes = meshes.mesh_count(),
            draws = meshes.draw_count(),
            "model ready"
        );

        Ok(Self {
            meshes,
            heap,
            frames,
            centre,
            world,
            camera,
            inspector: InspectorState::default(),
            angle: 0.0,
            ui,
            frame_times: FrameTimes::default(),
            model: logical,
            gpu,
        })
    }

    fn render(&mut self) -> Result<(), String> {
        self.frame_times.tick();

        if let Some(extent) = self
            .frames
            .prepare(self.gpu.surface(), self.gpu.extent())
            .map_err(|error| error.to_string())?
        {
            self.meshes
                .resize(self.gpu.allocator(), extent)
                .map_err(|error| error.to_string())?;
        }

        // Declared and uploaded before the frame opens: uploading a texture
        // waits on the GPU, and nothing inside a recorded frame may block on it.
        let declared = self.declare_ui();
        self.ui
            .upload(&mut self.heap, self.gpu.allocator(), &declared)
            .map_err(|error| error.to_string())?;

        // Read from the world every frame, so an edit in the inspector takes
        // effect on the next one. Copied out because the closure below cannot
        // borrow the world while `render` holds `&mut self.frames`.
        let settings = *self
            .world
            .get::<OrbitCamera>(self.camera)
            .expect("the camera entity is spawned in `new` and never despawned");

        if settings.orbiting {
            self.angle += settings.radians_per_frame;
        }

        let meshes = &self.meshes;
        let heap = &self.heap;
        let ui = &mut self.ui;
        let allocator = self.gpu.allocator();
        let centre = self.centre;
        let angle = self.angle;

        self.frames
            .render(|frame| {
                // Aspect from the target rather than the window: they agree
                // except on the frame a resize is noticed, and the target is
                // what is actually being drawn into.
                let aspect =
                    frame.target.extent.width as f32 / frame.target.extent.height.max(1) as f32;

                meshes.record(heap, frame, camera(aspect, centre, angle, settings));

                // Last, in a pass of its own: the overlay loads the colour
                // attachment rather than clearing it, so it composites over the
                // model. Errors are logged rather than propagated — a debug
                // overlay that fails is not a reason to take the frame down.
                if let Err(failure) = ui.draw(heap, allocator, frame, &declared) {
                    error!(error = %failure, "the debug overlay did not draw");
                }

                // After everything that draws. Only the last writer transitions
                // the target to its final state — see `Frame::finish`.
                frame.finish();
            })
            .map_err(|error| error.to_string())?;

        Ok(())
    }

    /// Declare this frame's debug UI.
    ///
    /// Re-declared every frame from current state, which is what immediate mode
    /// means (`docs/DESIGN.md` §10.2): there is no widget tree to keep in sync,
    /// so it cannot disagree with the engine it is reporting on.
    fn declare_ui(&mut self) -> slop_editor::Declared {
        let timing = self.frame_times.summary();
        let extent = self.frames.extent();
        let frames = self.frames.frame_number();
        let meshes = self.meshes.mesh_count();
        let draws = self.meshes.draw_count();
        let model = self.model.clone();

        // Borrowed out of `self` so the closure does not capture it whole:
        // `run` needs `&mut self.ui` at the same time.
        let world = &mut self.world;
        let inspector = &mut self.inspector;

        self.ui.run(self.gpu.window(), |context| {
            slop_editor::egui::Window::new("slop").show(context, |ui| {
                // Milliseconds first. See `slop_app::timing`.
                ui.label(format!("{:.2} ms  ({:.0} fps)", timing.last, timing.fps()));
                ui.label(format!(
                    "{:.2} ms  worst of last {}",
                    timing.worst, timing.window
                ));
                ui.separator();
                ui.label(format!("{}x{}", extent.width, extent.height));
                ui.label(format!("frame {frames}"));
                ui.separator();
                ui.label(&model);
                ui.label(format!("{meshes} meshes, {draws} draws"));
            });

            // A second window rather than a section of the first, because it is
            // the one thing here that is worth resizing and scrolling.
            slop_editor::egui::Window::new("inspector")
                .default_open(false)
                .show(context, |ui| {
                    // Nothing reacts to the return value yet. The camera is read
                    // fresh every frame, so an edit is picked up without needing
                    // to be told about — a system that had to be re-run would
                    // use this.
                    let _changed = slop_editor::inspector(ui, world, inspector);
                });
        })
    }
}

fn load_shader(device: &Arc<Device>, vfs: &Vfs) -> Result<ShaderModule, String> {
    let bytes = vfs
        .read("shaders/passes/model.spv")
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}

fn load_reflection(vfs: &Vfs) -> Result<slop_asset::Reflection, String> {
    let bytes = vfs
        .read("shaders/passes/model.refl")
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    slop_asset::Reflection::read(&bytes).map_err(|error| error.to_string())
}

impl Drop for Renderer {
    fn drop(&mut self) {
        info!(frames = self.frames.frame_number(), "shutting down");

        // Before any field drops: the GPU may still be executing a frame that
        // references the meshes and textures declared above the device.
        if let Err(failure) = self.gpu.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}
