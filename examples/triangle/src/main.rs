//! M0 task F: the first render.
//!
//! Run `cargo run -p slop-cli -- cook` first, then `cargo run -p example-triangle`.
//!
//! This file owns `main()` and the event loop, per `docs/DESIGN.md` §1.2
//! principle 4 — the engine supplies pieces rather than a shape to sit inside.
//! What it no longer owns is the *frame* loop: acquire, submit, present and
//! frames in flight are `slop_render::FrameRenderer`'s, which is why this file
//! is now about a third of its previous length.
//!
//! **The synchronisation subtleties this file used to explain have moved with
//! the code that handles them**, into `slop-render`. Two are worth knowing about
//! even from here, because they are what makes the loop non-obvious:
//!
//! - Acquire returns an index *before* the image is usable, so rendering waits
//!   on a semaphore at the colour-attachment stage. Skipping that produces
//!   flicker indistinguishable from a driver bug.
//! - Render-finished semaphores are per swapchain image rather than per frame in
//!   flight, because present waits on one and there is no way to observe when it
//!   is finished with it.
//!
//! What remains here is what an application genuinely owns: creating a window
//! and a device, building one pipeline, and recording a draw into whatever
//! target the frame renderer hands over.

use std::sync::Arc;

use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};

use slop_app::gpu::{Gpu, GpuConfig};
use slop_app::timing::FrameTimes;
use slop_app::window::WindowConfig;
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_editor::DebugUi;
use slop_render::{Frame, FrameRenderer, FrameRendererConfig, Target};
use slop_rhi::{
    Attachments, BindlessHeap, BindlessHeapConfig, Blend, ClearValue, ColorAttachment, Device,
    GraphicsPipeline, GraphicsPipelineConfig, ImageState, Load, PipelineLayout, ShaderModule,
    ShaderStage, vk,
};

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
            && renderer.frame_number() >= limit
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
        renderer.gpu.window().request_redraw();
    }
}
struct Renderer {
    // Declared in drop order: everything built from the device, then the `Gpu`
    // that owns the device, surface and window — whose *internal* drop order is
    // its own problem rather than this file's.
    //
    // No separate layout field: `GraphicsPipeline` already holds an `Arc` to it,
    // which is what keeps it alive.
    pipeline: GraphicsPipeline,
    /// The debug overlay, and the heap holding its font atlas.
    ///
    /// The triangle's own pipeline binds nothing, so this heap exists purely for
    /// the overlay. That is not waste: a heap is descriptors, and the point of a
    /// bindless model is that one heap serves everything.
    ui: DebugUi,
    heap: BindlessHeap,
    frame_times: FrameTimes,
    renderer: FrameRenderer,
    gpu: Gpu,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let gpu = Gpu::new(
            event_loop,
            &GpuConfig {
                window: WindowConfig {
                    title: String::from("slop — triangle"),
                    ..Default::default()
                },
                application_name: String::from("example-triangle"),
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

        let module = load_shader(gpu.device())?;
        let layout = Arc::new(PipelineLayout::empty(gpu.device()).map_err(|e| e.to_string())?);
        let pipeline = GraphicsPipeline::new(
            gpu.device(),
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
                color_format: renderer.format(),
                // No depth: the triangle is a single flat primitive with nothing
                // to occlude it. Depth arrives with the cube.
                depth_format: None,
                // Positions come from SV_VertexID, so there is nothing to bind.
                vertex_layout: None,
                // On, deliberately. This is the check that the shader agrees
                // with the engine's counter-clockwise front face: a triangle
                // wound the wrong way vanishes silently, with no validation
                // complaint, so leaving culling off would let the convention rot
                // unnoticed until real geometry made it expensive.
                cull_back_faces: true,
                blend: Blend::Opaque,
            },
        )
        .map_err(|error| error.to_string())?;

        // The module may be dropped now — Vulkan does not require it to outlive
        // the pipelines built from it.
        drop(module);

        let mut heap = BindlessHeap::new(gpu.device(), &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;
        let ui = DebugUi::new(
            gpu.window(),
            gpu.device(),
            &mut heap,
            &assets(),
            renderer.format(),
        )
        .map_err(|error| error.to_string())?;

        info!(
            width = renderer.extent().width,
            height = renderer.extent().height,
            "triangle ready"
        );

        Ok(Self {
            pipeline,
            ui,
            heap,
            frame_times: FrameTimes::default(),
            renderer,
            gpu,
        })
    }

    fn frame_number(&self) -> u64 {
        self.renderer.frame_number()
    }

    fn mark_dirty(&mut self) {
        self.renderer.invalidate();
    }

    fn render(&mut self) -> Result<(), String> {
        // Nothing here is sized to the target — no depth buffer, no offscreen
        // attachment — so the new extent is discarded. The cube, which has one,
        // is where this return value earns its keep.
        self.renderer
            .prepare(self.gpu.surface(), self.gpu.extent())
            .map_err(|error| error.to_string())?;

        // Borrowed out of `self` so the closure does not capture it whole:
        // `render` needs `&mut self.renderer` at the same time.
        self.frame_times.tick();

        // Before the frame opens: uploading the font atlas waits on the GPU.
        let timing = self.frame_times.summary();
        let extent = self.renderer.extent();
        let frames = self.renderer.frame_number();

        let declared = self.ui.run(self.gpu.window(), |context| {
            slop_editor::egui::Window::new("slop").show(context, |ui| {
                ui.label(format!("{:.2} ms  ({:.0} fps)", timing.last, timing.fps()));
                ui.label(format!(
                    "{:.2} ms  worst of last {}",
                    timing.worst, timing.window
                ));
                ui.separator();
                ui.label(format!("{}x{}", extent.width, extent.height));
                ui.label(format!("frame {frames}"));
            });
        });

        self.ui
            .upload(&mut self.heap, self.gpu.allocator(), &declared)
            .map_err(|error| error.to_string())?;

        let pipeline = &self.pipeline;
        let ui = &mut self.ui;
        let heap = &self.heap;
        let allocator = self.gpu.allocator();

        self.renderer
            .render(|frame| {
                record(pipeline, frame);

                if let Err(failure) = ui.draw(heap, allocator, frame, &declared) {
                    error!(error = %failure, "the debug overlay did not draw");
                }

                // After everything that draws — see `Frame::finish`.
                frame.finish();
            })
            .map_err(|error| error.to_string())?;

        Ok(())
    }
}

/// Draw the triangle into this frame's target.
///
/// A free function rather than a method, because everything it needs arrives in
/// the [`Frame`] or is borrowed explicitly — which is also what lets it be
/// called while the frame renderer is mutably borrowed.
fn record(pipeline: &GraphicsPipeline, frame: &Frame<'_>) {
    let Target {
        image,
        view,
        extent,
        from,
        ..
    } = frame.target;

    // From UNDEFINED rather than from PRESENT: the previous contents are about
    // to be cleared, so discarding them is both correct and faster. Which state
    // that is comes from the target rather than being assumed here.
    frame.command.transition_image(
        image,
        vk::ImageAspectFlags::COLOR,
        from,
        ImageState::COLOR_ATTACHMENT,
    );

    let mut pass = frame.command.begin_rendering(&Attachments {
        color: ColorAttachment {
            view,
            load: Load::Clear(ClearValue::Color([0.02, 0.02, 0.03, 1.0])),
        },
        // No depth: the triangle is one flat primitive with nothing to occlude
        // it, and the pipeline was built the same way.
        depth: None,
        extent,
    });

    pass.bind_pipeline(pipeline);
    // Three vertices, one instance. Positions come from SV_VertexID, so there is
    // nothing to bind.
    pass.draw(3, 1);

    // Ends the pass, so the overlay can open its own.
    //
    // The image is left in `COLOR_ATTACHMENT` rather than the frame's final
    // state: the overlay draws after this, and only the last writer transitions.
    // The caller calls `Frame::finish` once everything has drawn.
    drop(pass);
}

impl Drop for Renderer {
    fn drop(&mut self) {
        info!(frames = self.renderer.frame_number(), "shutting down");

        // Before any field drops. `FrameRenderer::drop` waits too, but it drops
        // after the pipeline, and destroying a pipeline a pending submission
        // still references is undefined.
        if let Err(failure) = self.gpu.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}

/// Load the cooked triangle shader.
///
/// Through the asset VFS, so this names the shader rather than a path into the
/// cache. Where cooked bytes live is `slop-asset`'s business.
/// Cooked assets, found by walking up from wherever this was run.
///
/// An application's decision, which is why the starting directory is chosen here
/// rather than inside `slop-asset` — `docs/CONVENTIONS.md` §5.1 keeps a library
/// from reading the environment on its caller's behalf.
fn assets() -> Vfs {
    let here = std::env::current_dir().expect("the current directory must be readable");

    Vfs::discover(&here).unwrap_or_else(|failure| panic!("{failure}"))
}

fn load_shader(device: &Arc<Device>) -> Result<ShaderModule, String> {
    let bytes = assets()
        .read("shaders/passes/triangle.spv")
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}
