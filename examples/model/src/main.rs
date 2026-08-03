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
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::ActiveEventLoop;
use slop_app::winit::window::Window;
use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};
use slop_ecs::{Entity, World};
use slop_editor::{DebugUi, InspectorState};
use slop_math::Vec3;
use slop_render::{
    FrameRenderer, FrameRendererConfig, Graph, HdrTarget, Imported, Lights, MeshRenderer,
    PointLight, RenderPass, Tonemap, View,
};

/// What the scene pass clears the HDR target to.
///
/// Dark rather than black, so that geometry missing entirely is visibly
/// different from geometry that is merely unlit.
const CLEAR: [f32; 4] = [0.02, 0.02, 0.03, 1.0];

/// How many lights the buffer is built to hold.
///
/// Fixed for its lifetime — see `Lights::new` — so it is sized for what §9.4 is
/// heading towards rather than for the four this example places. A thousand
/// rows is thirty-two kilobytes, which is not worth being careful about.
const LIGHT_CAPACITY: u32 = 1024;
use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, ClearValue, Device, ImageAspect, ImageState, Load,
    ShaderModule, Stage,
};

// Shared with `tests/golden.rs`, so the window and the reference image are
// framed by the same camera — see the library's docs.
use example_model::{DEFAULT_MODEL, OrbitCamera, assets, bounds, camera};

fn main() -> ! {
    slop_app::run::<Renderer>()
}

impl slop_app::Application for Renderer {
    type Error = String;

    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        Self::create(event_loop)
    }

    fn window(&self) -> &Window {
        self.gpu.window()
    }

    fn render(&mut self) -> Result<(), String> {
        self.draw()
    }

    fn frame_number(&self) -> u64 {
        self.frames.frame_number()
    }

    fn resized(&mut self) {
        self.frames.invalidate();
    }

    fn on_window_event(&mut self, event: &WindowEvent) -> bool {
        // The UI sees every event first and reports whether it consumed one.
        // Nothing here reads input yet, so the answer is discarded — but a
        // camera control added later must respect it, or a drag on a UI window
        // would also swing the camera behind it.
        self.ui.on_window_event(self.gpu.window(), event);
        false
    }
}

struct Renderer {
    // Declared in drop order: what draws, then the heap it indexes, then the
    // frame renderer, then the `Gpu` everything was built from.
    meshes: MeshRenderer,
    /// Where the scene is drawn, in floating point, before being resolved.
    hdr: HdrTarget,
    /// What resolves it onto the swapchain.
    tonemap: Tonemap,
    /// This frame's lights, one buffer per frame in flight.
    lights: Lights,
    /// Where they sit — a function of the model's bounds, so it frames whatever
    /// is loaded. Kept rather than rebuilt, since nothing moves them yet.
    placed_lights: Vec<PointLight>,
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
    /// Build the window, device and scene. Named apart from
    /// `Application::new` so neither shadows the other.
    fn create(event_loop: &ActiveEventLoop) -> Result<Self, String> {
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
        let module = load_shader(gpu.device(), &vfs, "model")?;
        let reflection = load_reflection(&vfs, "model")?;

        let mut meshes = MeshRenderer::new(
            gpu.device(),
            &mut heap,
            &module,
            &reflection,
            // The HDR target's format, not the swapchain's. The scene is drawn
            // in floating point and resolved by `Tonemap` — see
            // `slop_render::HdrTarget`.
            slop_render::HDR_FORMAT,
            slop_rhi::preferred_depth_format(gpu.device()),
        )
        .map_err(|error| error.to_string())?;

        let hdr = HdrTarget::new(gpu.allocator(), &mut heap, frames.extent())
            .map_err(|error| error.to_string())?;

        let tonemap_module = load_shader(gpu.device(), &vfs, "tonemap")?;
        let tonemap = Tonemap::new(
            gpu.device(),
            &mut heap,
            &tonemap_module,
            &load_reflection(&vfs, "tonemap")?,
            // The swapchain's, because this is the pass that writes it.
            frames.format(),
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

        // Sized for far more than four, because the capacity is fixed for the
        // lifetime of the buffer — see `Lights::new` — and E4's whole point is
        // that many lights become affordable.
        let lights = Lights::new(
            gpu.allocator(),
            &mut heap,
            frames.frames_in_flight(),
            LIGHT_CAPACITY,
        )
        .map_err(|error| error.to_string())?;
        let placed_lights = example_model::lights(centre, radius);

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
            .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

        info!(
            model = logical,
            meshes = meshes.mesh_count(),
            draws = meshes.draw_count(),
            "model ready"
        );

        Ok(Self {
            meshes,
            hdr,
            tonemap,
            lights,
            placed_lights,
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

    /// Draw one frame. Named apart from `Application::render` so neither
    /// shadows the other.
    fn draw(&mut self) -> Result<(), String> {
        self.frame_times.tick();

        if let Some(extent) = self
            .frames
            .prepare(self.gpu.surface(), self.gpu.extent())
            .map_err(|error| error.to_string())?
        {
            self.meshes
                .resize(self.gpu.allocator(), extent)
                .map_err(|error| error.to_string())?;
            self.hdr
                .resize(
                    self.gpu.device(),
                    self.gpu.allocator(),
                    &mut self.heap,
                    extent,
                )
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
        let hdr = &self.hdr;
        let tonemap = &self.tonemap;
        let heap = &self.heap;
        let lights = &mut self.lights;
        let placed_lights = &self.placed_lights;
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

                // **The frame as a declaration.** Nothing below names a barrier;
                // the graph derives every one of them from what each pass says
                // it touches. `docs/PLAN.md` §9.5 E3.
                let mut graph = Graph::new();

                let scene = graph.import(&Imported {
                    name: "hdr",
                    image: hdr.image(),
                    view: hdr.view(),
                    aspect: hdr.aspect(),
                    extent: hdr.extent(),
                    // Cleared by the pass that writes it, so its previous
                    // contents are worth nothing.
                    state: ImageState::UNDEFINED,
                    final_state: None,
                });

                let screen = graph.import(&Imported {
                    name: "swapchain",
                    image: frame.target.image,
                    view: frame.target.view,
                    aspect: ImageAspect::Color,
                    extent: frame.target.extent,
                    state: frame.target.from,
                    // Left in COLOR_ATTACHMENT: the overlay still draws over it
                    // outside the graph, and `Frame::finish` ends the frame.
                    final_state: None,
                });

                // Written here rather than before the frame opened, and that is
                // the safe place rather than a convenient one: `render` waits
                // for *this* slot's previous submission before handing the
                // frame over, so the GPU has finished reading what was here.
                //
                // A failure is logged rather than propagated. The only way this
                // fails is more lights than the buffer holds, and a scene that
                // renders with the lighting it had is better than one that does
                // not render.
                if let Err(failure) = lights.write(frame.slot, placed_lights) {
                    error!(error = %failure, "this frame's lights were not written");
                }

                let view = View::new(camera(aspect, centre, angle, settings), lights, frame.slot);

                if let Some((image, depth_view, depth_aspect)) = meshes.depth() {
                    let depth = graph.import(&Imported {
                        name: "depth",
                        image,
                        view: depth_view,
                        aspect: depth_aspect,
                        extent: hdr.extent(),
                        state: ImageState::UNDEFINED,
                        final_state: None,
                    });

                    // Depth first, and nothing else. A pass with no colour
                    // attachment at all — `docs/PLAN.md` §9.4.
                    graph.add(
                        &RenderPass {
                            name: "depth prepass",
                            color: None,
                            depth: Some((
                                depth,
                                Load::Clear(ClearValue::Depth(slop_rhi::DEPTH_CLEAR)),
                                // Stored, unlike every depth attachment before
                                // this one: the pass below is what reads it.
                                true,
                            )),
                            ..RenderPass::default()
                        },
                        // Unlit: the prepass shades nothing, so the lights in
                        // `view` are along for the ride rather than read.
                        |pass| meshes.draw_depth(pass, heap, &view),
                    );

                    graph.add(
                        &RenderPass {
                            name: "scene",
                            color: Some((scene, Load::Clear(ClearValue::Color(CLEAR)))),
                            depth: Some((
                                depth,
                                // What the prepass wrote. Clearing here would
                                // throw the prepass away and leave it pure cost.
                                Load::Preserve,
                                // Scratch from here on, so storing it would
                                // cost bandwidth nothing reads.
                                false,
                            )),
                            ..RenderPass::default()
                        },
                        |pass| meshes.draw(pass, heap, &view),
                    );
                }

                graph.add(
                    &RenderPass {
                        name: "tonemap",
                        color: Some((screen, Load::Discard)),
                        // The declaration that produces the barrier: this reads
                        // what the pass above wrote.
                        samples: &[(scene, Stage::Fragment)],
                        ..RenderPass::default()
                    },
                    |pass| tonemap.draw(pass, heap, hdr.slot()),
                );

                graph.execute(frame.command);

                // Still outside the graph, and the last thing that is: the
                // overlay opens its own pass, composites over the tonemapped
                // image, and is what keeps `Frame::finish` alive. Errors are
                // logged rather than propagated — a debug overlay that fails is
                // not a reason to take the frame down.
                if let Err(failure) = ui.draw(heap, allocator, frame, &declared) {
                    error!(error = %failure, "the debug overlay did not draw");
                }

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

fn load_shader(device: &Arc<Device>, vfs: &Vfs, name: &str) -> Result<ShaderModule, String> {
    let bytes = vfs
        .read(&format!("shaders/passes/{name}.spv"))
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}

fn load_reflection(vfs: &Vfs, name: &str) -> Result<slop_asset::Reflection, String> {
    let bytes = vfs
        .read(&format!("shaders/passes/{name}.refl"))
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
