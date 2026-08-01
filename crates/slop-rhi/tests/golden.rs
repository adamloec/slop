//! M0 task G: headless rendering, and the first golden image.
//!
//! Renders the triangle with no window, no surface, and no swapchain, copies the
//! result back to the CPU, and compares it against an approved reference.
//!
//! # Why this is the headless mode
//!
//! `docs/DESIGN.md` §5 asks for a headless mode that renders N frames without a
//! window. A separate `examples/headless` binary that drew a triangle to a PNG
//! and this test would be the same program written twice, so there is one, and
//! it is the one that asserts something. `SLOP_UPDATE_GOLDEN=1` writes the image
//! out to be looked at, which covers the case a demo binary would have served.
//!
//! # Why it renders more than one frame
//!
//! Frame 1 exercises the draw. Frames 2 and 3 exercise pool reset, timeline
//! waiting, and reuse of the render target — the parts of the loop where a
//! missing barrier shows up as a frame that differs from the one before it.
//! Comparing every frame against the same reference is what makes that visible;
//! rendering once would not.
//!
//! # Colour space
//!
//! The target is `R8G8B8A8_UNORM`, not the sRGB format a swapchain picks. No
//! automatic encode happens on write, so the bytes read back are the bytes the
//! shader produced, and nothing in the comparison depends on a colour space
//! conversion the driver performs. The image therefore looks darker than the
//! window does. That is correct and deliberate.
//!
//! # Which tier this is
//!
//! The committed reference was produced on real hardware, so the tolerance is
//! [`Tolerance::HARDWARE`]. The lavapipe tier, which compares by exact match,
//! lands with CI — see `docs/PLAN.md` §4.1-G. Both use this same test; only the
//! reference and the tolerance differ.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use slop_rhi::{
    Allocator, Buffer, BufferConfig, CommandPool, Device, GraphicsPipeline, GraphicsPipelineConfig,
    Image, ImageConfig, ImageState, MemoryLocation, PipelineLayout, ShaderModule, ShaderStage, vk,
};
use slop_verify::{Golden, Mode, Rgba8, Tolerance};

/// Small enough to be a trivial file in the repository, large enough that the
/// triangle's edges cover many pixels — an 8x8 image would pass with the
/// geometry substantially wrong.
const SIZE: u32 = 256;

/// No automatic sRGB encode on write, so readback bytes are shader output. See
/// the module docs.
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Enough to exercise reset and reuse; more would only cost time.
const FRAMES: u32 = 3;

#[test]
fn the_headless_triangle_matches_its_reference() {
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };
    let Some(module) = cooked_triangle(&device) else {
        return;
    };

    let mut renderer = Headless::new(&device, &allocator, &module);

    for frame in 1..=FRAMES {
        let image = renderer.render();

        let golden = Golden {
            reference: &reference_path(),
            failures: &failures_path(),
            tolerance: Tolerance::HARDWARE,
            mode: Mode::from_env(),
        };

        match golden.check(&image) {
            Ok(difference) => {
                // Printed on success too: a test sitting at 0.9% of a 1% budget
                // is worth seeing before it crosses, not after.
                println!("frame {frame}: {difference}");
            }
            Err(failure) => panic!("frame {frame} did not match: {failure}"),
        }
    }
}

#[test]
fn every_frame_is_identical_to_the_first() {
    // Independent of the reference, and it catches a different bug: a target
    // that is not fully cleared, or a barrier that is missing between frames,
    // produces a second frame differing from the first while both could still
    // sit inside the tolerance against a committed reference.
    //
    // Exact comparison, deliberately. Two frames from the same GPU in the same
    // process have no reason to differ by even one bit.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };
    let Some(module) = cooked_triangle(&device) else {
        return;
    };

    let mut renderer = Headless::new(&device, &allocator, &module);
    let first = renderer.render();

    for frame in 2..=FRAMES {
        assert_eq!(
            renderer.render(),
            first,
            "frame {frame} differs from frame 1"
        );
    }
}

#[test]
fn the_allocator_reports_nothing_live_once_resources_drop() {
    // A leak check needing no external tooling. Vulkan's own validation reports
    // objects destroyed in the wrong order, not memory the allocator still
    // believes is handed out.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    assert_eq!(allocator.stats().live, 0, "nothing should be live yet");

    {
        let _target = colour_target(&allocator);
        let _readback = readback_buffer(&allocator);

        assert_eq!(allocator.stats().live, 2);
    }

    assert_eq!(
        allocator.stats().live,
        0,
        "both allocations should have returned"
    );

    device.wait_idle().expect("the device must go idle");
}

/// Everything needed to render the triangle into memory we own.
struct Headless {
    // Declared in drop order: the pool and pipeline first, then the resources
    // built from the allocator, then the device.
    //
    // No `allocator` field: `Buffer` and `Image` each hold their own `Arc` to
    // it, which is what keeps it alive for exactly as long as anything it
    // allocated. A field here would be a second copy of a guarantee already
    // made, and dead weight the compiler correctly complains about.
    pool: CommandPool,
    pipeline: GraphicsPipeline,
    readback: Buffer,
    target: Image,
    device: Arc<Device>,
}

impl Headless {
    fn new(device: &Arc<Device>, allocator: &Arc<Allocator>, module: &ShaderModule) -> Self {
        let layout = Arc::new(PipelineLayout::empty(device).expect("an empty layout"));
        let pipeline = GraphicsPipeline::new(
            device,
            &layout,
            &GraphicsPipelineConfig {
                vertex: ShaderStage {
                    module,
                    entry: c"vertexMain",
                },
                fragment: ShaderStage {
                    module,
                    entry: c"fragmentMain",
                },
                color_format: FORMAT,
                // On, matching the windowed path. A triangle wound the wrong way
                // vanishes silently with no validation complaint, so this is the
                // only thing that catches it — and here it would produce a
                // uniformly cleared reference that still compares equal to
                // itself every run.
                cull_back_faces: true,
            },
        )
        .expect("pipeline creation must succeed");

        Self {
            pool: CommandPool::new(device, device.queue_families().graphics).expect("a pool"),
            pipeline,
            readback: readback_buffer(allocator),
            target: colour_target(allocator),
            device: Arc::clone(device),
        }
    }

    /// Render one frame and bring it back to the CPU.
    fn render(&mut self) -> Rgba8 {
        self.pool.reset().expect("the pool must reset");
        let command = self
            .pool
            .allocate(1)
            .expect("allocation must succeed")
            .pop()
            .expect("one buffer was requested");

        command.begin().expect("recording must begin");

        // From UNDEFINED every frame: the previous contents are about to be
        // cleared, so discarding is both correct and faster than preserving
        // them. This is also what makes each frame independent of the last.
        command.transition_image(
            self.target.handle(),
            ImageState::UNDEFINED,
            ImageState::COLOR_ATTACHMENT,
        );

        self.draw(command.handle());

        command.transition_image(
            self.target.handle(),
            ImageState::COLOR_ATTACHMENT,
            ImageState::TRANSFER_SRC,
        );
        command.copy_image_to_buffer(
            self.target.handle(),
            self.readback.handle(),
            self.target.extent(),
        );
        // Coherent memory is not ordered memory: without this the host may read
        // the buffer before the copy filling it has finished.
        command.make_visible_to_host(self.readback.handle());

        command.end().expect("recording must end");

        support::submit_and_wait(&self.device, &command);

        let bytes = self
            .readback
            .mapped()
            .expect("readback memory must be host-visible")
            .to_vec();

        Rgba8::new(SIZE, SIZE, bytes).expect("the buffer is sized for exactly this image")
    }

    /// Record the draw. Mirrors `examples/triangle`, minus the swapchain.
    fn draw(&self, buffer: vk::CommandBuffer) {
        let extent = self.target.extent();

        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(self.target.view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    // The same clear the windowed example uses, so the two
                    // renders are comparable by eye.
                    float32: [0.02, 0.02, 0.03, 1.0],
                },
            })];

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
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        // The same invariant every owner of Vulkan objects carries: wait here,
        // before any field drops, because `Device::drop` runs far too late for
        // fields declared before it. `examples/triangle` learned this from a
        // shutdown crash.
        self.device
            .wait_idle()
            .expect("the device must go idle before teardown");
    }
}

/// The image rendered into.
fn colour_target(allocator: &Arc<Allocator>) -> Image {
    Image::new(
        allocator,
        &ImageConfig {
            name: "golden colour target",
            extent: vk::Extent2D {
                width: SIZE,
                height: SIZE,
            },
            format: FORMAT,
            // TRANSFER_SRC is what makes it readable; without it the copy is a
            // validation error rather than a wrong result.
            usage: vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        },
    )
    .expect("the colour target must be creatable")
}

/// The host-visible buffer pixels are copied into.
fn readback_buffer(allocator: &Arc<Allocator>) -> Buffer {
    Buffer::new(
        allocator,
        &BufferConfig {
            name: "golden readback",
            size: u64::from(SIZE) * u64::from(SIZE) * Rgba8::CHANNELS as u64,
            usage: vk::BufferUsageFlags::TRANSFER_DST,
            location: MemoryLocation::Readback,
        },
    )
    .expect("the readback buffer must be creatable")
}

/// The cooked triangle module, or `None` with an explanation if absent.
fn cooked_triangle(device: &Arc<Device>) -> Option<ShaderModule> {
    let path = workspace_root()
        .join(".slop")
        .join("cache")
        .join("shaders")
        .join("passes")
        .join("triangle.spv");

    match std::fs::read(&path) {
        Ok(bytes) => {
            Some(ShaderModule::from_bytes(device, &bytes).expect("cooked SPIR-V must load"))
        }
        Err(_) => {
            eprintln!(
                "skipping: {} not found — run `cargo run -p slop-cli -- cook`",
                path.display()
            );
            None
        }
    }
}

/// Dev-only path resolution — the asset VFS at M2 replaces this.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// The approved reference, committed to the repository.
fn reference_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("triangle.png")
}

/// Where a failed comparison writes the render and the difference.
///
/// Under `target/`, because these are build output rather than source and must
/// never be committed by accident.
fn failures_path() -> PathBuf {
    workspace_root().join("target").join("golden-failures")
}
