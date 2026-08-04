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
use std::time::Duration;

use example_model::{OrbitCamera, bounds, camera};
use slop_math::Vec3;
use slop_render::{HdrTarget, MeshRenderer, Target, Tonemap};
use slop_rhi::{
    Allocator, BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferUsage, CommandPool,
    Device, DeviceSelection, Extent2D, Format, Image, ImageConfig, ImageState, ImageUsage,
    Instance, InstanceConfig, MemoryLocation, RhiError, ShaderModule, TimelineSemaphore,
};
use slop_verify::{Golden, Mode, Rgba8, Tolerance};

/// Large enough that a material boundary covers many pixels; small enough that
/// the reference is a trivial file in the repository.
const SIZE: u32 = 256;

/// UNORM, so readback bytes are the bytes the shader wrote and nothing in the
/// comparison depends on a colour space conversion the driver performs.
const FORMAT: Format = Format::Rgba8Unorm;

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

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model", Sky::Uniform) else {
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

    let Some(mut renderer) = harness(&device, &allocator, SPONZA, Sky::Uniform) else {
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
fn sponza_under_a_cooked_environment_matches_its_reference() {
    // The only thing that renders nine spherical-harmonic coefficients rather
    // than the degenerate one-colour case. Every other reference here binds
    // `default_irradiance`, which is a *uniform* sky — so if the shader's band
    // weights or basis constants were wrong beyond the constant term, all of
    // them would still pass. This is what covers the rest of the formula.
    //
    // Sponza rather than the cube because the effect is a directional one: an
    // arcade with sky above and stone below is where a sky that varies with
    // direction looks different from a sky that does not.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, SPONZA, Sky::Cooked(HELIPAD)) else {
        return;
    };

    let image = renderer.render(CAPTURED_FRAME);

    let difference = Golden {
        reference: &reference_path("sponza-helipad.png"),
        failures: &failures_path(),
        tolerance: Tolerance::HARDWARE,
        mode: Mode::from_env(),
    }
    .check(&image)
    .unwrap_or_else(|failure| panic!("sponza under helipad did not match: {failure}"));

    println!("sponza + helipad, frame {CAPTURED_FRAME}: {difference}");
}

#[test]
fn a_cooked_environment_actually_changes_the_image() {
    // The trap `docs/PLAN.md` §6.1 records for the cascades, avoided here: a
    // reference proves a frame did not *change*, never that a feature does
    // anything. If `irradiance` silently fell back to the uniform sky — a
    // mistyped logical path would do it — the reference above would still pass,
    // having been approved against whatever it produced.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut uniform) = harness(&device, &allocator, SPONZA, Sky::Uniform) else {
        return;
    };

    let Some(mut cooked) = harness(&device, &allocator, SPONZA, Sky::Cooked(HELIPAD)) else {
        return;
    };

    assert_ne!(
        uniform.render(CAPTURED_FRAME),
        cooked.render(CAPTURED_FRAME),
        "a cooked environment renders identically to a flat one, so it is not \
         reaching the shader"
    );
}

#[test]
fn the_shader_reads_more_than_the_constant_band() {
    // What neither reference above can isolate. Every other test binds a sky
    // that is the same in every direction, so a shader that dropped bands one
    // and two — kept only the constant term, or weighted the rest by zero —
    // would render all of them identically and pass. Even the cooked
    // environment would still *differ* from the uniform one, because its
    // constant term differs, so `a_cooked_environment_actually_changes_the_
    // image` does not catch it either.
    //
    // So: a sky lit from one side, against a uniform sky carrying the **same**
    // constant band. The two differ only in bands one and two, and a cube has
    // faces pointing six ways to show it. Synthetic coefficients rather than a
    // cooked file, so this needs no fetch and runs on every checkout.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let mut directional = slop_math::Sh9::ZERO;
    directional.accumulate(Vec3::Y, Vec3::splat(0.6), 1.0);

    // The same band zero, so band zero cannot be what distinguishes them.
    let flat = slop_math::Sh9 {
        coefficients: {
            let mut coefficients = [Vec3::ZERO; slop_math::COEFFICIENTS];
            coefficients[0] = directional.coefficients[0];
            coefficients
        },
    };

    let Some(mut lit_from_above) = harness(
        &device,
        &allocator,
        "models/cube.model",
        Sky::Given(directional),
    ) else {
        return;
    };

    let Some(mut lit_evenly) = harness(&device, &allocator, "models/cube.model", Sky::Given(flat))
    else {
        return;
    };

    assert_ne!(
        lit_from_above.render(CAPTURED_FRAME),
        lit_evenly.render(CAPTURED_FRAME),
        "a sky lit from +Y renders the same as a uniform one with the same constant \
         band, so the shader is reading only band zero"
    );
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

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model", Sky::Uniform) else {
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

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model", Sky::Uniform) else {
        return;
    };

    assert_ne!(
        renderer.render(CAPTURED_FRAME),
        renderer.render(CAPTURED_FRAME + 30),
        "the camera does not appear to be orbiting"
    );
}

#[test]
fn resizing_between_frames_replaces_the_depth_buffer_safely() {
    // `MeshRenderer::resize` destroys the depth image it is replacing, and a
    // frame submitted just before may still be reading it. In the windowed path
    // that is survivable by accident: `FrameRenderer::prepare` recreates the
    // swapchain first, and `Swapchain::recreate` waits for the device. Here
    // there is no swapchain and no such wait, which is why this test lives in
    // the headless harness rather than a windowed one.
    //
    // The signal is the validation layer, which reports
    // VUID-vkDestroyImage-image-01000 when an image a pending command buffer
    // references is destroyed. `Instance::validation_errors` is what makes that
    // assertable: without it the layer's report only reaches `tracing` and this
    // test passes just as happily against the unfixed code.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model", Sky::Uniform) else {
        return;
    };

    let before = renderer.render(CAPTURED_FRAME);

    // The frame is deliberately left in flight: `submit_frame` returns without
    // waiting, so the GPU may still be reading the depth buffer that `resize` is
    // about to destroy. Going through `render` here would prove nothing, because
    // it waits before returning and nothing is ever in flight.
    let in_flight = renderer.submit_frame(CAPTURED_FRAME);

    // The same extent on purpose: the depth attachment has to keep matching the
    // colour target, which this harness does not resize. The destroy-and-replace
    // is what is under test, not the new size.
    renderer
        .resize(&allocator)
        .expect("resizing to the same extent must succeed");

    Headless::wait(&in_flight);

    assert_eq!(
        device.instance().validation_errors(),
        0,
        "the validation layer reported an error while resizing over a frame in flight"
    );

    assert_eq!(
        before,
        renderer.render(CAPTURED_FRAME),
        "the same frame rendered differently after the depth buffer was replaced"
    );
}

#[test]
fn loading_a_second_time_replaces_rather_than_accumulates() {
    // `load` used to be unsafe to call twice and said nothing about it. The
    // material rows it builds are local and restart at zero, while `self.meshes`
    // accumulated across calls holding the *previous* call's row indices — so a
    // second model silently re-pointed the first model's meshes at the wrong
    // material rows, or past the end of the buffer. The superseded heap slot was
    // never removed either, so it leaked.
    //
    // Counting meshes is the direct assertion: the same model loaded twice must
    // leave exactly what one load leaves.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(vfs) = example_model::assets().ok() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator, "models/cube.model", Sky::Uniform) else {
        return;
    };

    let before = renderer.render(CAPTURED_FRAME);
    let meshes = renderer.meshes.mesh_count();
    let draws = renderer.meshes.draw_count();

    assert!(meshes > 0, "the harness should have loaded something");

    renderer
        .load(&allocator, &vfs, "models/cube.model")
        .expect("loading a second time must succeed");

    assert_eq!(
        renderer.meshes.mesh_count(),
        meshes,
        "meshes accumulated across loads instead of being replaced"
    );
    assert_eq!(
        renderer.meshes.draw_count(),
        draws,
        "placements accumulated across loads instead of being replaced"
    );

    // The freed resources were ones the frame above may still have been reading,
    // so this also covers the wait `unload` performs before dropping them.
    assert_eq!(
        device.instance().validation_errors(),
        0,
        "the validation layer reported an error while reloading"
    );

    assert_eq!(
        before,
        renderer.render(CAPTURED_FRAME),
        "the same model reloaded rendered differently"
    );
}

/// Where Sponza's cooked model lives, when it has been fetched.
const SPONZA: &str = "models/vendor/sponza/Sponza.model";

/// Where the cooked environment lives, when it has been fetched.
///
/// The second vendored asset, and the second legitimate skip. Named here rather
/// than reached for through `example_model::DEFAULT_ENVIRONMENT` so that the
/// skip check and the thing being skipped are the same string.
const HELIPAD: &str = example_model::DEFAULT_ENVIRONMENT;

/// What lights a scene, chosen by the test rather than discovered.
///
/// Discovery is the thing to avoid: an environment picked up because it happened
/// to be cooked would make one test render two different images depending on
/// whether someone had run `fetch`, and one reference cannot be right for both.
#[derive(Debug, Clone, Copy)]
enum Sky {
    /// The uniform fallback — what every reference from before E6b was approved
    /// against, and what they must still produce.
    Uniform,
    /// A cooked environment, skipped by name when it has not been fetched.
    Cooked(&'static str),
    /// Coefficients the test builds itself, for asserting on the shape of the
    /// reconstruction rather than on any particular sky.
    Given(slop_math::Sh9),
}

/// What each cascade's pass is called, matching the windowed viewer.
const CASCADE_NAMES: [&str; slop_render::CASCADES] = [
    "shadow cascade 0",
    "shadow cascade 1",
    "shadow cascade 2",
    "shadow cascade 3",
];

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
fn harness(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    model: &str,
    environment: Sky,
) -> Option<Headless> {
    let vfs = example_model::assets().ok().or_else(|| {
        eprintln!("skipping: nothing is cooked — run `cargo run -p slop-cli -- cook`");
        None
    })?;

    for logical in [
        "shaders/passes/scene/model.spv",
        "shaders/passes/scene/model.refl",
    ] {
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

    // A uniform sky unless a test asks for something else. **Not "whichever is
    // cooked"**: that would render two different images from one test depending
    // on whether someone had run `fetch`, and one reference cannot be right for
    // both. The default is the same nine coefficients describing a sky that
    // happens to be one colour, so there is no second code path either way.
    let irradiance = match environment {
        Sky::Uniform => slop_render::default_irradiance(),
        Sky::Given(sh) => sh,
        Sky::Cooked(logical) => {
            if !vfs.exists(logical) {
                // The same named skip the model above gets, for the same reason.
                assert_eq!(
                    logical, HELIPAD,
                    "'{logical}' is not cooked, and only the fetched environment is \
                     allowed to be absent"
                );

                eprintln!(
                    "skipping: helipad is not fetched — run \
                     `cargo run -p slop-cli -- fetch helipad`"
                );
                return None;
            }

            example_model::irradiance(&vfs, logical)
        }
    };

    match Headless::new(device, allocator, &vfs, model, irradiance) {
        Ok(headless) => Some(headless),
        Err(failure) => panic!("the renderer must build once its assets are cooked: {failure}"),
    }
}

/// A `MeshRenderer` with an offscreen target and the readback path.
struct Headless {
    pool: CommandPool,
    meshes: MeshRenderer,
    /// Where the scene is drawn before being resolved, as in the window.
    hdr: HdrTarget,
    tonemap: Tonemap,
    /// One slot: the harness submits and waits per frame, so nothing is ever in
    /// flight beside it.
    lights: slop_render::Lights,
    /// The same clustered path the window uses, not a simplified stand-in —
    /// otherwise the references stop covering the pass that decides which lights
    /// reach a fragment at all.
    clusters: slop_render::Clusters,
    /// The sun and the sky, with the values the shader used to hold as
    /// constants — see `DirectionalLight::default`.
    environment: slop_render::Environment,
    /// The sky's nine coefficients.
    ///
    /// **Passed in rather than discovered**, which is the whole point: if this
    /// read a cooked environment when one happened to be present, the same test
    /// would render two different images depending on whether someone had run
    /// `fetch`, and one reference cannot be right for both.
    irradiance: slop_math::Sh9,
    /// The same four cascades the window renders, so the references cover the
    /// shadow path rather than a simplified stand-in.
    shadows: slop_render::Shadows,
    placed_lights: Vec<slop_render::PointLight>,
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
        irradiance: slop_math::Sh9,
    ) -> Result<Self, String> {
        let extent = Extent2D {
            width: SIZE,
            height: SIZE,
        };

        let mut heap = BindlessHeap::new(device, &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;

        let module = ShaderModule::from_bytes(
            device,
            &vfs.read("shaders/passes/scene/model.spv")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let reflection = slop_asset::Reflection::read(
            &vfs.read("shaders/passes/scene/model.refl")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let mut meshes = MeshRenderer::new(
            device,
            &mut heap,
            &module,
            &reflection,
            // The scene is drawn in floating point and resolved by `Tonemap`,
            // exactly as the windowed viewer does it. Rendering straight into
            // `FORMAT` here would test a path the window does not take.
            slop_render::HDR_FORMAT,
            slop_rhi::preferred_depth_format(device),
        )
        .map_err(|error| error.to_string())?;

        let hdr =
            HdrTarget::new(allocator, &mut heap, extent).map_err(|error| error.to_string())?;

        let tonemap_module = ShaderModule::from_bytes(
            device,
            &vfs.read("shaders/passes/post/tonemap.spv")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let tonemap = Tonemap::new(
            device,
            &mut heap,
            &tonemap_module,
            &slop_asset::Reflection::read(
                &vfs.read("shaders/passes/post/tonemap.refl")
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?,
            FORMAT,
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
                usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::TRANSFER_SRC,
                mip_levels: 1,
                array_layers: 1,
            },
        )
        .map_err(|error| error.to_string())?;

        let readback = Buffer::new(
            allocator,
            &BufferConfig {
                name: "model golden readback",
                size: u64::from(SIZE) * u64::from(SIZE) * Rgba8::CHANNELS as u64,
                usage: BufferUsage::TRANSFER_DST,
                location: MemoryLocation::Readback,
            },
        )
        .map_err(|error| error.to_string())?;

        let (centre, radius) = bounds(vfs, model);

        // The same rig the window places, from the same function, for the same
        // reason the camera is shared: a golden image lit differently from the
        // demo would stop catching a broken light rig and start drifting from
        // it silently.
        let lights = slop_render::Lights::new(allocator, &mut heap, 1, 1024)
            .map_err(|error| error.to_string())?;
        let placed_lights = example_model::lights(centre, radius);

        let environment = slop_render::Environment::new(allocator, &mut heap, 1)
            .map_err(|error| error.to_string())?;

        let shadows = slop_render::Shadows::new(
            device,
            allocator,
            &mut heap,
            slop_render::ShadowConfig {
                near: radius * 0.02,
                far: radius * 4.0,
                ..slop_render::ShadowConfig::default()
            },
            1,
        )
        .map_err(|error| error.to_string())?;

        let cluster_module = ShaderModule::from_bytes(
            device,
            &vfs.read("shaders/passes/scene/cluster_build.spv")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let cluster_reflection = slop_asset::Reflection::read(
            &vfs.read("shaders/passes/scene/cluster_build.refl")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        // The same grid the window builds, from the same numbers.
        let clusters = slop_render::Clusters::new(
            device,
            allocator,
            &mut heap,
            &cluster_module,
            &cluster_reflection,
            slop_render::ClusterGrid {
                near: radius * 0.01,
                far: radius * 8.0,
                ..slop_render::ClusterGrid::default()
            },
            1,
        )
        .map_err(|error| error.to_string())?;

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
            hdr,
            tonemap,
            lights,
            clusters,
            environment,
            irradiance,
            shadows,
            placed_lights,
            heap,
            readback,
            target,
            centre,
            settings,
            device: Arc::clone(device),
        })
    }

    /// Rebuild the renderer's depth buffer at the target's current size.
    fn resize(&mut self, allocator: &Arc<Allocator>) -> Result<(), slop_render::RenderError> {
        self.meshes.resize(allocator, self.target.extent())
    }

    /// Load a model into the renderer that already has one.
    fn load(
        &mut self,
        allocator: &Arc<Allocator>,
        vfs: &slop_asset::Vfs,
        model: &str,
    ) -> Result<(), slop_render::RenderError> {
        self.meshes.load(allocator, &mut self.heap, vfs, model)
    }

    /// Render one frame and bring it back to the CPU.
    fn render(&mut self, frame: u64) -> Rgba8 {
        let timeline = self.submit_frame(frame);
        Self::wait(&timeline);
        self.read_back()
    }

    /// Record and submit one frame, returning without waiting for it.
    ///
    /// Split out of [`Self::render`] so a test can hold a frame in flight and do
    /// something to the resources it references. Every other caller wants
    /// `render`, which waits.
    fn submit_frame(&mut self, frame: u64) -> TimelineSemaphore {
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

        // Every per-frame write, before the graph exists — the same order the
        // windowed viewer uses, and for the same reason.
        self.lights
            .write(0, &self.placed_lights)
            .expect("the light rig fits the buffer it was built for");

        let cluster_camera = example_model::cluster_camera(
            aspect,
            self.centre,
            angle,
            self.settings,
            (SIZE as f32, SIZE as f32),
        );

        self.clusters
            .write(0, &cluster_camera, &self.lights)
            .expect("the cluster grid must be writable");

        self.environment
            .write(
                0,
                &slop_render::DirectionalLight::default(),
                &self.irradiance,
            )
            .expect("the environment must be writable");

        self.shadows
            .write(
                0,
                &slop_render::DirectionalLight::default(),
                example_model::view_of(self.centre, angle, self.settings).inverse(),
                cluster_camera.tan_half_fov_y,
                aspect,
            )
            .expect("the cascades must be writable");

        // Before the graph, which borrows them.
        let cascade_map = self.shadows.map(0);
        let cascade_views: [slop_rhi::ImageViewHandle; slop_render::CASCADES] =
            std::array::from_fn(|index| cascade_map.layer_view(index as u32));
        let cascade_cameras: [slop_render::View; slop_render::CASCADES] =
            std::array::from_fn(|index| self.shadows.cascade_view(index, &self.environment, 0));

        let view = slop_render::View::new(
            camera(aspect, self.centre, angle, self.settings),
            &self.environment,
            &self.clusters,
            Some(&self.shadows),
            0,
        );

        // The same declaration the windowed viewer makes, minus the overlay.
        // Nothing here names a barrier.
        let mut graph = slop_render::Graph::new();

        let scene = graph.import(&slop_render::Imported {
            name: "hdr",
            image: self.hdr.image(),
            view: self.hdr.view(),
            layer_views: &[],
            aspect: self.hdr.aspect(),
            extent: self.hdr.extent(),
            state: ImageState::UNDEFINED,
            final_state: None,
        });

        let screen = graph.import(&slop_render::Imported {
            name: "readback target",
            image: self.target.handle(),
            view: self.target.view(),
            layer_views: &[],
            aspect: self.target.aspect(),
            extent: self.target.extent(),
            state: frame.target.from,
            // Where the window would ask for PRESENT, this asks for
            // TRANSFER_SRC — and the graph emits it because it knows which pass
            // touched the image last. That is the arbitration `frame.finish`
            // used to do by convention, and it is why this test no longer calls
            // it.
            final_state: Some(ImageState::TRANSFER_SRC),
        });

        // The cascades, before anything that reads them — four passes into four
        // layers of one image.
        let cascades = graph.import(&slop_render::Imported {
            name: "shadow cascades",
            image: cascade_map.handle(),
            view: cascade_map.view(),
            layer_views: &cascade_views,
            aspect: cascade_map.aspect(),
            extent: cascade_map.extent(),
            state: ImageState::UNDEFINED,
            final_state: None,
        });

        for index in 0..slop_render::CASCADES {
            let meshes = &self.meshes;
            let heap = &self.heap;

            graph.add(
                &slop_render::RenderPass {
                    name: CASCADE_NAMES[index],
                    color: None,
                    depth: Some(slop_render::DepthTarget {
                        image: cascades,
                        load: slop_rhi::Load::Clear(slop_rhi::ClearValue::Depth(
                            slop_rhi::DEPTH_CLEAR,
                        )),
                        store: true,
                        layer: index as u32,
                    }),
                    samples: &[],
                    reads: &[],
                },
                move |pass| meshes.draw_depth(pass, heap, &cascade_cameras[index]),
            );
        }

        let [cluster_ranges, cluster_indices] = self.clusters.buffers(0);

        let cluster_ranges = graph.import_buffer(&slop_render::ImportedBuffer {
            name: "cluster ranges",
            buffer: cluster_ranges,
            state: slop_rhi::BufferState::storage_write(slop_rhi::Stage::Compute),
            final_state: None,
        });

        let cluster_indices = graph.import_buffer(&slop_render::ImportedBuffer {
            name: "cluster light indices",
            buffer: cluster_indices,
            state: slop_rhi::BufferState::storage_write(slop_rhi::Stage::Compute),
            final_state: None,
        });

        let clusters = &self.clusters;
        let cluster_heap = &self.heap;

        graph.add_compute(
            &slop_render::ComputePass {
                name: "cluster build",
                writes: &[cluster_ranges, cluster_indices],
                ..slop_render::ComputePass::default()
            },
            move |command| clusters.build(command, cluster_heap, 0),
        );

        let meshes = &self.meshes;
        let tonemap = &self.tonemap;
        let heap = &self.heap;
        let source = self.hdr.slot();

        if let Some((image, depth_view, depth_aspect)) = self.meshes.depth() {
            let depth = graph.import(&slop_render::Imported {
                name: "depth",
                image,
                view: depth_view,
                layer_views: &[],
                aspect: depth_aspect,
                extent: self.hdr.extent(),
                state: ImageState::UNDEFINED,
                final_state: None,
            });

            // The same two passes the windowed path declares, in the same
            // order. That is what makes this a golden test of the frame rather
            // than of a simplified stand-in.
            //
            // **What the references do and do not prove about the prepass.** A
            // prepass that drew nothing at all would leave depth at
            // `DEPTH_CLEAR`, every fragment would pass, and the image would be
            // identical — so "the goldens are unchanged" is not evidence the
            // prepass runs. What is: clearing this to 1.0 instead, so the
            // prepass leaves the near plane everywhere, changes 65533 of 65536
            // pixels. The scene pass really does test against what this wrote.
            // Measured, and the reason the reference is trusted here.
            graph.add(
                &slop_render::RenderPass {
                    name: "depth prepass",
                    color: None,
                    depth: Some(slop_render::DepthTarget {
                        image: depth,
                        load: slop_rhi::Load::Clear(slop_rhi::ClearValue::Depth(
                            slop_rhi::DEPTH_CLEAR,
                        )),
                        store: true,
                        layer: 0,
                    }),
                    ..slop_render::RenderPass::default()
                },
                |pass| meshes.draw_depth(pass, heap, &view),
            );

            graph.add(
                &slop_render::RenderPass {
                    name: "scene",
                    color: Some((
                        scene,
                        slop_rhi::Load::Clear(slop_rhi::ClearValue::Color([0.02, 0.02, 0.03, 1.0])),
                    )),
                    depth: Some(slop_render::DepthTarget {
                        image: depth,
                        load: slop_rhi::Load::Preserve,
                        store: false,
                        layer: 0,
                    }),
                    reads: &[
                        (cluster_ranges, slop_rhi::Stage::Fragment),
                        (cluster_indices, slop_rhi::Stage::Fragment),
                    ],
                    samples: &[(cascades, slop_rhi::Stage::Fragment)],
                },
                |pass| meshes.draw(pass, heap, &view),
            );
        }

        graph.add(
            &slop_render::RenderPass {
                name: "tonemap",
                color: Some((screen, slop_rhi::Load::Discard)),
                samples: &[(scene, slop_rhi::Stage::Fragment)],
                ..slop_render::RenderPass::default()
            },
            |pass| tonemap.draw(pass, heap, source),
        );

        graph.execute(&command);

        command.copy_image_to_buffer(
            self.target.handle(),
            self.readback.handle(),
            self.target.extent(),
        );
        command.make_visible_to_host(self.readback.handle());
        command.end().expect("end");

        let timeline =
            TimelineSemaphore::new(&self.device, 0).expect("semaphore creation must succeed");

        self.device
            .submit_graphics(&slop_rhi::Submission {
                command: &command,
                wait: &[],
                signal: &[],
                signal_timeline: &[(timeline.handle(), 1)],
            })
            .expect("submission must succeed");

        timeline
    }

    /// Block until a submitted frame has finished.
    fn wait(timeline: &TimelineSemaphore) {
        assert!(
            timeline
                .wait(1, Duration::from_secs(10))
                .expect("waiting must not fail"),
            "the GPU did not finish within ten seconds"
        );
    }

    /// Copy the target out of the readback buffer. Only valid once the frame
    /// that wrote it has finished.
    fn read_back(&self) -> Rgba8 {
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
