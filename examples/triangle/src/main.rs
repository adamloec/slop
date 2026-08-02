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

use std::path::PathBuf;
use std::sync::Arc;

use slop_asset::Vfs;
use slop_core::diagnostics::tracing::{error, info};

use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::{Window, WindowId};
use slop_render::{Frame, FrameRenderer, FrameRendererConfig, Target};
use slop_rhi::{
    Blend, Device, DeviceSelection, GraphicsPipeline, GraphicsPipelineConfig, ImageState, Instance,
    InstanceConfig, PipelineLayout, ShaderModule, ShaderStage, Surface, vk,
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
        renderer.window.request_redraw();
    }
}
struct Renderer {
    // Declared in drop order: everything built from the device, then the
    // device, then the surface, then the window it came from.
    //
    // No separate layout field: `GraphicsPipeline` already holds an `Arc` to it,
    // which is what keeps it alive.
    pipeline: GraphicsPipeline,
    renderer: FrameRenderer,
    device: Arc<Device>,
    surface: Surface,
    window: Window,
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

        let renderer = FrameRenderer::new(
            &device,
            &surface,
            window_extent(&window),
            &FrameRendererConfig::default(),
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

        info!(
            width = renderer.extent().width,
            height = renderer.extent().height,
            "triangle ready"
        );

        Ok(Self {
            pipeline,
            renderer,
            device,
            surface,
            window,
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
            .prepare(&self.surface, window_extent(&self.window))
            .map_err(|error| error.to_string())?;

        // Borrowed out of `self` so the closure does not capture it whole:
        // `render` needs `&mut self.renderer` at the same time.
        let device = &self.device;
        let pipeline = &self.pipeline;

        self.renderer
            .render(|frame| record(device, pipeline, frame))
            .map_err(|error| error.to_string())?;

        Ok(())
    }
}

/// Draw the triangle into this frame's target.
///
/// A free function rather than a method, because everything it needs arrives in
/// the [`Frame`] or is borrowed explicitly — which is also what lets it be
/// called while the frame renderer is mutably borrowed.
fn record(device: &Arc<Device>, pipeline: &GraphicsPipeline, frame: &Frame<'_>) {
    let Target {
        image,
        view,
        extent,
        from,
        to,
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

    let raw = device.raw();
    let buffer = frame.command.handle();

    // SAFETY: the buffer is recording, every borrowed structure outlives these
    // calls, and `dynamic_rendering` is in the required feature tier.
    unsafe {
        raw.cmd_begin_rendering(buffer, &rendering);
        raw.cmd_set_viewport(buffer, 0, &viewports);
        raw.cmd_set_scissor(buffer, 0, &scissors);
        raw.cmd_bind_pipeline(buffer, vk::PipelineBindPoint::GRAPHICS, pipeline.handle());
        // Three vertices, one instance. Positions come from SV_VertexID, so
        // there is nothing to bind.
        raw.cmd_draw(buffer, 3, 1, 0, 0);
        raw.cmd_end_rendering(buffer);
    }

    frame.command.transition_image(
        image,
        vk::ImageAspectFlags::COLOR,
        ImageState::COLOR_ATTACHMENT,
        to,
    );
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
        // after the pipeline, and destroying a pipeline a pending submission
        // still references is undefined.
        if let Err(failure) = self.device.wait_idle() {
            error!(error = %failure, "device did not go idle; teardown may be unsafe");
        }
    }
}

/// Load the cooked triangle shader.
///
/// Through the asset VFS, so this names the shader rather than a path into the
/// cache. Where cooked bytes live is `slop-asset`'s business.
fn load_shader(device: &Arc<Device>) -> Result<ShaderModule, String> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    let bytes = Vfs::for_project(&project)
        .read("shaders/passes/triangle.spv")
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}
