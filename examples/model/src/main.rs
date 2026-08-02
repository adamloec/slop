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

use std::path::PathBuf;
use std::sync::Arc;

use slop_app::gpu::{Gpu, GpuConfig};
use slop_app::window::WindowConfig;
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};
use slop_math::{Mat4, Quat, Vec3};
use slop_render::{FrameRenderer, FrameRendererConfig, MeshRenderer};
use slop_rhi::{BindlessHeap, BindlessHeapConfig, Device, ShaderModule};

/// Which model to draw, when nothing says otherwise.
const DEFAULT_MODEL: &str = "models/cube.model";

/// Vertical field of view, in degrees.
const FIELD_OF_VIEW: f32 = 55.0;

/// Radians of orbit per frame.
///
/// Per *frame* rather than per second, matching the cube and for the same
/// reason: `docs/DESIGN.md` §2.14 makes a frame number the only clock a
/// reproducible render may read.
const RADIANS_PER_FRAME: f32 = 0.006;

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
    /// How far the camera sits from the model's centre, from its bounds.
    distance: f32,
    centre: Vec3,
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

        let project = project_root();
        let vfs = Vfs::for_project(&project);
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
            // Far enough that the whole model fits the vertical field of view,
            // with a margin so nothing is clipped by the near plane.
            distance: radius / slop_math::scalar::tan(FIELD_OF_VIEW.to_radians() * 0.5) * 1.4,
            centre,
            gpu,
        })
    }

    fn render(&mut self) -> Result<(), String> {
        if let Some(extent) = self
            .frames
            .prepare(self.gpu.surface(), self.gpu.extent())
            .map_err(|error| error.to_string())?
        {
            self.meshes
                .resize(self.gpu.allocator(), extent)
                .map_err(|error| error.to_string())?;
        }

        let meshes = &self.meshes;
        let heap = &self.heap;
        let centre = self.centre;
        let distance = self.distance;

        self.frames
            .render(|frame| {
                meshes.record(heap, frame, camera(frame, centre, distance));
            })
            .map_err(|error| error.to_string())?;

        Ok(())
    }
}

/// The view-projection for one frame.
///
/// Orbits the model rather than accepting input, so a run is reproducible from
/// its frame number alone and a screenshot at frame *n* is comparable across
/// machines (`docs/DESIGN.md` §2.14). Camera control arrives with the editor.
fn camera(frame: &slop_render::Frame<'_>, centre: Vec3, distance: f32) -> Mat4 {
    let angle = frame.number as f32 * RADIANS_PER_FRAME;
    let eye = centre + Quat::from_rotation_y(angle) * Vec3::new(0.0, distance * 0.35, distance);

    let view = slop_math::look_at(eye, centre, slop_math::UP);
    let aspect = frame.target.extent.width as f32 / frame.target.extent.height.max(1) as f32;

    // The engine's own projection, not glam's: it is reverse-Z and infinite,
    // matching the depth comparison `slop-rhi` configures, and it flips Y for
    // Vulkan's clip space. Reaching for `Mat4::perspective_rh` here would draw
    // the model inverted and fail every depth test.
    //
    // The near plane scales with the model so that a metre-wide cube and a
    // hundred-metre building both get sensible precision.
    let projection = slop_math::perspective(FIELD_OF_VIEW.to_radians(), aspect, distance * 0.005);

    projection * view
}

/// The model's centre and radius, from the meshes it names.
///
/// Read from the cooked artifacts rather than guessed, so pointing this at
/// Sponza frames Sponza and pointing it at a cube frames a cube. A renderer that
/// needed a hand-tuned camera per asset would be a demo rather than a viewer.
fn bounds(vfs: &Vfs, logical: &str) -> (Vec3, f32) {
    let Ok(bytes) = vfs.read(logical) else {
        return (Vec3::ZERO, 1.0);
    };
    let Ok(model) = slop_asset::Model::read(&bytes) else {
        return (Vec3::ZERO, 1.0);
    };

    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);

    for instance in &model.instances {
        let Ok(bytes) = vfs.read(&instance.mesh) else {
            continue;
        };
        let Ok(mesh) = slop_asset::Mesh::read(&bytes) else {
            continue;
        };

        let transform = Mat4::from_cols_array(&instance.transform);

        for vertex in &mesh.vertices {
            let placed = transform.transform_point3(Vec3::from_array(vertex.position));

            min = min.min(placed);
            max = max.max(placed);
        }
    }

    if min.x > max.x {
        return (Vec3::ZERO, 1.0);
    }

    let centre = (min + max) * 0.5;

    ((centre), (max - min).length() * 0.5)
}

/// Where this example's assets were cooked into.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
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
