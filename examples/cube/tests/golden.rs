//! The cube, rendered headless and compared against an approved reference.
//!
//! Lives in this crate rather than in `slop-rhi` because the scene does: the
//! windowed demo and this test share one [`Scene`], so the thing a human looks
//! at and the thing CI checks cannot drift apart.
//!
//! # Why a golden image of a *moving* object is possible
//!
//! Because the rotation is a function of the frame counter and nothing else
//! (`docs/DESIGN.md` §2.14). Frame 12 looks identical on every run, on every
//! machine, forever. Driving it from a clock instead would make this scene
//! untestable, and that is not a hypothetical — it is the default way anyone
//! writes a spinning cube.
//!
//! # What a failure here means
//!
//! Far more than the triangle's golden did. A difference implicates the vertex
//! or index upload, the texture upload, the bindless heap, the depth test, the
//! projection, the push constants, or the winding. That breadth is the point of
//! the M0 cube — `docs/PLAN.md` §4 calls it "deliberately unambitious; its job
//! is integration, not looks" — but it does mean the diff image is the first
//! thing to open, not the last.

use std::path::PathBuf;
use std::sync::Arc;

use example_cube::{Scene, Target};
use slop_rhi::{
    Allocator, Buffer, BufferConfig, CommandPool, Device, DeviceSelection, Image, ImageConfig,
    ImageState, Instance, InstanceConfig, MemoryLocation, RhiError, TimelineSemaphore, vk,
};
use slop_verify::{Golden, Mode, Rgba8, Tolerance};

/// Large enough that the cube's faces and the checkerboard on them cover many
/// pixels; small enough to be a trivial file in the repository.
const SIZE: u32 = 256;

/// UNORM, so readback bytes are the bytes the shader wrote and nothing in the
/// comparison depends on a colour space conversion the driver performs.
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The frame captured.
///
/// Chosen so that **three faces are visible** — roughly 46° about Y and 28°
/// about X. That is not cosmetic: a reference showing one face flat on would
/// pass with the other five wound backwards, lit by inverted normals, or
/// textured with swapped axes. Frame 12 was the first attempt and showed
/// exactly one face, which is why this is a named constant with a reason
/// attached rather than a number someone picked.
const CAPTURED_FRAME: u64 = 40;

#[test]
fn the_headless_cube_matches_its_reference() {
    let Some((device, allocator)) = headless() else {
        return;
    };

    let mut renderer = match Headless::new(&device, &allocator) {
        Ok(renderer) => renderer,
        Err(failure) => {
            eprintln!("skipping: {failure}");
            return;
        }
    };

    let image = renderer.render(CAPTURED_FRAME);

    let difference = Golden {
        reference: &reference_path(),
        failures: &failures_path(),
        tolerance: Tolerance::HARDWARE,
        mode: Mode::from_env(),
    }
    .check(&image)
    .unwrap_or_else(|failure| panic!("the cube did not match: {failure}"));

    println!("frame {CAPTURED_FRAME}: {difference}");
}

#[test]
fn the_same_frame_renders_identically_every_time() {
    // The property the reference rests on, asserted independently of it.
    // Exact comparison: the same frame number on the same GPU in the same
    // process has no reason to differ by one bit, and if it does, the reference
    // is measuring noise.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let mut renderer = match Headless::new(&device, &allocator) {
        Ok(renderer) => renderer,
        Err(failure) => {
            eprintln!("skipping: {failure}");
            return;
        }
    };

    let first = renderer.render(CAPTURED_FRAME);

    // Interleaved with other frames, so this also proves the depth buffer and
    // the render target are genuinely cleared between frames rather than
    // carrying state forward.
    renderer.render(CAPTURED_FRAME + 1);
    renderer.render(CAPTURED_FRAME + 7);
    let again = renderer.render(CAPTURED_FRAME);

    assert_eq!(first, again, "frame {CAPTURED_FRAME} rendered differently");
}

#[test]
fn consecutive_frames_actually_differ() {
    // Guards the opposite failure: a rotation wired to a constant would make
    // every test above pass while the cube sat still. A golden image cannot
    // tell the difference, because it only ever sees one frame.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let mut renderer = match Headless::new(&device, &allocator) {
        Ok(renderer) => renderer,
        Err(failure) => {
            eprintln!("skipping: {failure}");
            return;
        }
    };

    assert_ne!(
        renderer.render(CAPTURED_FRAME),
        renderer.render(CAPTURED_FRAME + 4),
        "the cube should have rotated"
    );
}

#[test]
fn the_model_matrix_depends_only_on_the_frame_number() {
    // No GPU needed. This is `DESIGN.md` §2.14 in its smallest form: the same
    // input produces the same transform, so a reference image of frame N stays
    // valid forever. A clock-driven rotation fails this immediately.
    for frame in [0, 1, 12, 1000, u64::from(u32::MAX)] {
        assert_eq!(
            Scene::model_matrix(frame),
            Scene::model_matrix(frame),
            "frame {frame} produced two different transforms"
        );
    }

    assert_ne!(Scene::model_matrix(0), Scene::model_matrix(1));
}

/// A headless device and allocator, or `None` with no Vulkan loader.
fn headless() -> Option<(Arc<Device>, Arc<Allocator>)> {
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    let instance = match Instance::new(&InstanceConfig::default()) {
        Ok(instance) => Arc::new(instance),
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
            return None;
        }
        Err(other) => panic!("instance creation failed: {other}"),
    };

    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
        .expect("one adapter must be usable");
    let device = Arc::new(Device::new(&instance, &devices[chosen]).expect("device creation"));
    let allocator = Allocator::new(&device).expect("allocator creation");

    Some((device, allocator))
}

/// The scene plus an offscreen target and the readback path.
struct Headless {
    pool: CommandPool,
    scene: Scene,
    readback: Buffer,
    target: Image,
    device: Arc<Device>,
}

impl Headless {
    fn new(device: &Arc<Device>, allocator: &Arc<Allocator>) -> Result<Self, String> {
        let extent = vk::Extent2D {
            width: SIZE,
            height: SIZE,
        };

        let scene = Scene::new(device, allocator, extent, FORMAT)?;

        let target = Image::new(
            allocator,
            &ImageConfig {
                name: "cube golden target",
                extent,
                format: FORMAT,
                usage: vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
            },
        )
        .map_err(|error| error.to_string())?;

        let readback = Buffer::new(
            allocator,
            &BufferConfig {
                name: "cube golden readback",
                size: u64::from(SIZE) * u64::from(SIZE) * Rgba8::CHANNELS as u64,
                usage: vk::BufferUsageFlags::TRANSFER_DST,
                location: MemoryLocation::Readback,
            },
        )
        .map_err(|error| error.to_string())?;

        Ok(Self {
            pool: CommandPool::new(device, device.queue_families().graphics)
                .map_err(|error| error.to_string())?,
            scene,
            readback,
            target,
            device: Arc::clone(device),
        })
    }

    /// Render one frame and bring it back to the CPU.
    fn render(&mut self, frame: u64) -> Rgba8 {
        self.pool.reset().expect("the pool must reset");
        let command = self
            .pool
            .allocate(1)
            .expect("allocation")
            .pop()
            .expect("one buffer was requested");

        command.begin().expect("begin");

        self.scene.record(
            &command,
            Target {
                image: self.target.handle(),
                view: self.target.view(),
                extent: self.target.extent(),
                from: ImageState::UNDEFINED,
                to: ImageState::TRANSFER_SRC,
            },
            frame,
        );

        command.copy_image_to_buffer(
            self.target.handle(),
            self.readback.handle(),
            self.target.extent(),
        );
        command.make_visible_to_host(self.readback.handle());
        command.end().expect("end");

        submit_and_wait(&self.device, command.handle());

        let bytes = self
            .readback
            .mapped()
            .expect("readback memory is host-visible")
            .to_vec();

        Rgba8::new(SIZE, SIZE, bytes).expect("the buffer is sized for exactly this image")
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        // Before any field drops. `Scene` waits in its own `Drop` too, but that
        // runs after this and after the pool it is declared before.
        self.device
            .wait_idle()
            .expect("the device must go idle before teardown");
    }
}

/// Submit and block. Tests only — a frame loop waits on a *previous* frame's
/// value so the CPU can run ahead.
fn submit_and_wait(device: &Arc<Device>, command: vk::CommandBuffer) {
    let timeline = TimelineSemaphore::new(device, 0).expect("semaphore");

    let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command)];
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(timeline.handle())
        .value(1)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&commands)
        .signal_semaphore_infos(&signals)];

    // SAFETY: the buffer is recorded and not pending, the timeline belongs to
    // this device, and every borrowed array outlives the call.
    unsafe {
        device
            .raw()
            .queue_submit2(device.queues().graphics, &submits, vk::Fence::null())
    }
    .expect("submission must succeed");

    assert!(
        timeline
            .wait(1, std::time::Duration::from_secs(5))
            .expect("waiting must not fail"),
        "the GPU did not finish within five seconds"
    );
}

/// The approved reference, committed to the repository.
fn reference_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("cube.png")
}

/// Where a failed comparison writes the render and the difference. Under
/// `target/`, because these are build output.
fn failures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("golden-failures")
}
