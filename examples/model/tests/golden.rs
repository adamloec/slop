//! `MeshRenderer` rendered headless and compared against approved references.
//!
//! What `examples/cube`'s goldens do not cover. Those exercise `Scene` — one
//! hand-written pipeline, one hardcoded texture — and never touch
//! `MeshRenderer`, the material buffer, the mip chain or the tangent frame.
//! Everything M2 added after the cube was verified by validation staying quiet
//! and by a human looking at the window, which says nothing about whether the
//! next change alters a pixel.
//!
//! This exists **before** M3's render graph on purpose. That rewrite changes how
//! every pass is recorded and derives barriers instead of taking them
//! hand-written, and the cube's reference is what made the last two rewrites
//! checkable — `FrameRenderer` and `Pass` were both "the golden is unchanged,
//! therefore the rewrite draws the same picture". `MeshRenderer` had no such
//! oracle until now.
//!
//! # Why a golden image of a moving camera is possible
//!
//! The camera is a pure function of an angle, and the angle is a multiple of the
//! frame number (`docs/DESIGN.md` §2.14). Frame 40 looks identical on every run,
//! forever. Driving it from a clock instead would make this untestable, which is
//! the default way anyone writes an orbiting viewer.
//!
//! # Two models, two references, one skippable
//!
//! `models/cube.model` is cooked from `assets/cube.gltf`, which is committed —
//! so that reference always runs, and covers the material buffer, a BC7 texture
//! with a full mip chain, and derived tangents.
//!
//! Sponza covers what the cube cannot: 103 primitives, 25 materials, alpha
//! masking, and **normal maps**, which nothing else in the repository samples.
//! It is fetched rather than committed (`slop-cli fetch sponza`), so that test
//! skips when it is absent — and that skip is checked **by name**, because a
//! blanket skip is precisely what once let this suite report green while the
//! demo refused to start.

use std::path::PathBuf;
use std::sync::Arc;

use example_model::{OrbitCamera, bounds, camera};
use slop_math::Vec3;
use slop_render::{MeshRenderer, Target};
use slop_rhi::{
    Allocator, BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, CommandPool, Device,
    DeviceSelection, Image, ImageConfig, ImageState, Instance, InstanceConfig, MemoryLocation,
    RhiError, ShaderModule, vk,
};
use slop_verify::{Golden, Mode, Rgba8, Tolerance};

/// Large enough that a material boundary covers many pixels; small enough that
/// the reference is a trivial file in the repository.
const SIZE: u32 = 256;

/// UNORM, so readback bytes are the bytes the shader wrote and nothing in the
/// comparison depends on a colour space conversion the driver performs.
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The frame captured.
///
/// Far enough into the orbit that the camera is off-axis — a view straight down
/// an axis would hide a transposed matrix, and one face flat on would pass with
/// the other five wound backwards.
const CAPTURED_FRAME: u64 = 40;

#[test]
fn the_cube_model_matches_its_reference() {
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model") else {
        return;
    };

    let image = renderer.render(CAPTURED_FRAME);

    let difference = Golden {
        reference: &reference_path("cube-model.png"),
        failures: &failures_path(),
        tolerance: Tolerance::HARDWARE,
        mode: Mode::from_env(),
    }
    .check(&image)
    .unwrap_or_else(|failure| panic!("the cube model did not match: {failure}"));

    println!("cube model, frame {CAPTURED_FRAME}: {difference}");
}

#[test]
fn sponza_matches_its_reference() {
    // The only thing in the repository that samples a normal map, and the only
    // thing with more than one material.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, SPONZA) else {
        return;
    };

    let image = renderer.render(CAPTURED_FRAME);

    let difference = Golden {
        reference: &reference_path("sponza.png"),
        failures: &failures_path(),
        tolerance: Tolerance::HARDWARE,
        mode: Mode::from_env(),
    }
    .check(&image)
    .unwrap_or_else(|failure| panic!("sponza did not match: {failure}"));

    println!("sponza, frame {CAPTURED_FRAME}: {difference}");
}

#[test]
fn the_same_frame_renders_identically_every_time() {
    // The property the references rest on, asserted independently of them.
    // Exact comparison: the same frame on the same GPU in the same process has
    // no reason to differ by one bit, and if it does, the reference is measuring
    // noise rather than correctness.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model") else {
        return;
    };

    let first = renderer.render(CAPTURED_FRAME);

    // Interleaved with other frames, so this also proves the depth buffer and
    // the colour target are genuinely cleared between frames rather than
    // carrying state forward.
    renderer.render(CAPTURED_FRAME + 1);
    renderer.render(CAPTURED_FRAME + 9);
    let again = renderer.render(CAPTURED_FRAME);

    assert_eq!(first, again, "frame {CAPTURED_FRAME} rendered differently");
}

#[test]
fn consecutive_frames_actually_differ() {
    // Guards the opposite failure: a camera wired to a constant angle would make
    // every test above pass while nothing moved. A golden image cannot tell,
    // because it only ever sees one frame.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model") else {
        return;
    };

    assert_ne!(
        renderer.render(CAPTURED_FRAME),
        renderer.render(CAPTURED_FRAME + 30),
        "the camera does not appear to be orbiting"
    );
}

/// Where Sponza's cooked model lives, when it has been fetched.
const SPONZA: &str = "models/vendor/sponza/Sponza.model";

/// How close Sponza's camera sits, as a fraction of what framing it would use.
///
/// **Inside the building, not outside it**, and that is the whole value of this
/// reference. `OrbitCamera::framing` fits the bounding sphere, which for Sponza
/// means looking down at the roof — a grey box that would pass with every
/// material wrong, every normal map unsampled and every alpha-masked surface
/// opaque. The first generated reference was exactly that, and approving it
/// would have recorded a test that checks almost nothing.
///
/// A fifth of the framing distance puts the camera in the atrium among the
/// columns and arches, which is where the materials this is meant to guard
/// actually are.
const SPONZA_CLOSENESS: f32 = 0.1;

/// How high Sponza's camera sits, as a fraction of its distance.
///
/// Near floor level. Looking down from above would see the floor and little
/// else; this looks along the atrium at the columns.
const SPONZA_HEIGHT: f32 = 0.05;

/// A device with no surface, or `None` if this machine has no Vulkan at all.
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

/// Build the harness, or skip if the model it needs is not cooked.
///
/// **A renderer that fails to build is a failure, not a skip.** The cube's
/// suite once printed "skipping" for *any* setup error, which meant a broken
/// shader reported the suite green while the demo refused to start. So the two
/// legitimate skips are checked by name, before anything is constructed:
///
/// - nothing cooked at all, which a fresh clone is genuinely in;
/// - Sponza not fetched, which is the normal state of a clone that has not run
///   `slop-cli fetch sponza`.
///
/// Anything else panics.
fn harness(device: &Arc<Device>, allocator: &Arc<Allocator>, model: &str) -> Option<Headless> {
    let vfs = example_model::assets().ok().or_else(|| {
        eprintln!("skipping: nothing is cooked — run `cargo run -p slop-cli -- cook`");
        None
    })?;

    for logical in ["shaders/passes/model.spv", "shaders/passes/model.refl"] {
        if !vfs.exists(logical) {
            eprintln!("skipping: '{logical}' is not cooked — run `cargo run -p slop-cli -- cook`");
            return None;
        }
    }

    if !vfs.exists(model) {
        // Named rather than blanket: only the *vendored* model may be missing on
        // a working checkout. A missing `cube.model` means the cook is broken,
        // which must fail rather than quietly pass.
        assert_eq!(
            model, SPONZA,
            "'{model}' is not cooked, and only the fetched Sponza is allowed to be absent"
        );

        eprintln!("skipping: sponza is not fetched — run `cargo run -p slop-cli -- fetch sponza`");
        return None;
    }

    match Headless::new(device, allocator, &vfs, model) {
        Ok(headless) => Some(headless),
        Err(failure) => panic!("the renderer must build once its assets are cooked: {failure}"),
    }
}

/// A `MeshRenderer` with an offscreen target and the readback path.
struct Headless {
    pool: CommandPool,
    meshes: MeshRenderer,
    heap: BindlessHeap,
    readback: Buffer,
    target: Image,
    centre: Vec3,
    settings: OrbitCamera,
    device: Arc<Device>,
}

impl Headless {
    fn new(
        device: &Arc<Device>,
        allocator: &Arc<Allocator>,
        vfs: &slop_asset::Vfs,
        model: &str,
    ) -> Result<Self, String> {
        let extent = vk::Extent2D {
            width: SIZE,
            height: SIZE,
        };

        let mut heap = BindlessHeap::new(device, &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;

        let module = ShaderModule::from_bytes(
            device,
            &vfs.read("shaders/passes/model.spv")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let reflection = slop_asset::Reflection::read(
            &vfs.read("shaders/passes/model.refl")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let mut meshes = MeshRenderer::new(
            device,
            &mut heap,
            &module,
            &reflection,
            FORMAT,
            slop_rhi::preferred_depth_format(device),
        )
        .map_err(|error| error.to_string())?;

        meshes
            .load(allocator, &mut heap, vfs, model)
            .map_err(|error| error.to_string())?;
        meshes
            .resize(allocator, extent)
            .map_err(|error| error.to_string())?;

        let target = Image::new(
            allocator,
            &ImageConfig {
                name: "model golden target",
                extent,
                format: FORMAT,
                usage: vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                mip_levels: 1,
            },
        )
        .map_err(|error| error.to_string())?;

        let readback = Buffer::new(
            allocator,
            &BufferConfig {
                name: "model golden readback",
                size: u64::from(SIZE) * u64::from(SIZE) * Rgba8::CHANNELS as u64,
                usage: vk::BufferUsageFlags::TRANSFER_DST,
                location: MemoryLocation::Readback,
            },
        )
        .map_err(|error| error.to_string())?;

        let (centre, radius) = bounds(vfs, model);

        // The camera *maths* is what is under test and is shared with the
        // windowed viewer. The camera *settings* are data, and Sponza's are
        // chosen to look at something worth comparing — see `SPONZA_CLOSENESS`.
        let mut settings = OrbitCamera::framing(radius);
        if model == SPONZA {
            settings.distance *= SPONZA_CLOSENESS;
            settings.height = SPONZA_HEIGHT;
        }

        Ok(Self {
            pool: CommandPool::new(device, device.queue_families().graphics)
                .map_err(|error| error.to_string())?,
            meshes,
            heap,
            readback,
            target,
            centre,
            settings,
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

        // A `Frame` built by hand rather than by `FrameRenderer`: no swapchain
        // and no window, which is the whole point of being headless. One slot,
        // because nothing here is in flight.
        let frame = slop_render::Frame {
            command: &command,
            target: Target {
                image: self.target.handle(),
                view: self.target.view(),
                extent: self.target.extent(),
                from: ImageState::UNDEFINED,
                to: ImageState::TRANSFER_SRC,
            },
            number: frame,
            slot: 0,
            slots: 1,
        };

        // The angle from the frame number, which is what makes this comparable
        // across runs. The windowed viewer accumulates instead, so that editing
        // the speed takes effect from the current position rather than jumping.
        let angle = frame.number as f32 * self.settings.radians_per_frame;
        let aspect = self.target.extent().width as f32 / self.target.extent().height as f32;

        self.meshes.record(
            &self.heap,
            &frame,
            camera(aspect, self.centre, angle, self.settings),
        );

        // Only the last thing to draw transitions the target — here to
        // TRANSFER_SRC, so the copy below can read it.
        frame.finish();

        command.copy_image_to_buffer(
            self.target.handle(),
            self.readback.handle(),
            self.target.extent(),
        );
        command.make_visible_to_host(self.readback.handle());
        command.end().expect("end");

        submit(&self.device, &command);

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
        // Before any field drops: the GPU may still be executing a frame that
        // references the meshes and textures declared above.
        if let Err(failure) = self.device.wait_idle() {
            eprintln!("device did not go idle: {failure}");
        }
    }
}

/// Submit a recorded buffer and wait for it.
fn submit(device: &Arc<Device>, command: &slop_rhi::CommandBuffer) {
    device
        .submit_graphics(&slop_rhi::Submission {
            command,
            wait: &[],
            signal: &[],
            signal_timeline: &[],
        })
        .expect("submission must succeed");

    device.wait_idle().expect("the GPU must finish");
}

/// An approved reference, committed to the repository.
fn reference_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
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
