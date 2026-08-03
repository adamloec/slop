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

use example_cube::Scene;
// `egui` through `slop-editor` rather than as a dependency of its own, so this
// test cannot end up on a different version than the overlay it drives.
use slop_editor::{Overlay, egui};
use slop_render::Target;
use slop_rhi::{
    Allocator, Buffer, BufferConfig, CommandPool, Device, DeviceSelection, Image, ImageConfig,
    ImageState, Instance, InstanceConfig, MemoryLocation, RhiError, ShaderModule, vk,
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
fn a_hot_reload_re_uploads_and_still_renders_the_reference() {
    // The end-to-end check for hot reload, and the one that catches the failures
    // the unit tests cannot: that the re-upload actually replaces the GPU
    // resources, that swapping the bindless slot leaves the descriptor pointing
    // at the new image, and that nothing is freed while the GPU still needs it.
    //
    // The cooked bytes are rewritten *identically*, which sounds pointless and
    // is the whole trick. It moves the file's timestamp, so the registry sees a
    // change and runs the entire reload path — poll, decode, re-upload, swap the
    // descriptor — and the render afterwards must still match the reference.
    // Rewriting with *different* content would prove the same mechanics but
    // leave the repository's cooked cache disagreeing with its source.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
    };

    for logical in ["textures/checker.tex", "meshes/cube.Cube.0.mesh"] {
        touch_cooked(logical);
    }

    assert!(
        renderer
            .scene
            .reload_changed()
            .expect("the reload must not fail"),
        "rewriting both artifacts must be noticed"
    );
    assert!(
        !renderer
            .scene
            .reload_changed()
            .expect("the second poll must not fail"),
        "and must not be noticed twice"
    );

    let image = renderer.render(CAPTURED_FRAME);

    let difference = Golden {
        reference: &reference_path(),
        failures: &failures_path(),
        tolerance: Tolerance::HARDWARE,
        // Always `Check`, never `Mode::from_env`. Approving a reference from a
        // reloaded scene would let a broken reload path define what "correct"
        // means; this test only ever gets to *disagree* with the reference that
        // `the_headless_cube_matches_its_reference` approves.
        mode: Mode::Check,
    }
    .check(&image)
    .unwrap_or_else(|failure| panic!("the reloaded cube did not match: {failure}"));

    println!("after reload, frame {CAPTURED_FRAME}: {difference}");
}

/// Rewrite a cooked artifact with its own bytes, so only its timestamp moves.
///
/// Written to a neighbouring file and renamed, because a plain overwrite is not
/// atomic: another test constructing a `Scene` at the same moment could read a
/// truncated artifact. `rename` over an existing file is atomic on both targets.
fn touch_cooked(logical: &str) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let path = slop_asset::Vfs::for_project(&project)
        .resolve(logical)
        .expect("a valid logical path");

    let bytes = std::fs::read(&path).expect("the artifact must be cooked");
    let temporary = path.with_extension("touch");

    std::fs::write(&temporary, &bytes).expect("writing the replacement");
    std::fs::rename(&temporary, &path).expect("renaming over the artifact");
}

#[test]
fn the_headless_cube_matches_its_reference() {
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
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

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
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

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
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

/// Build the harness, or skip if the assets it needs have not been cooked.
///
/// **A scene that fails to build is a failure, not a skip.** These tests used to
/// print "skipping" and return for *any* `Headless::new` error, which meant a
/// shader disagreeing with its Rust side, a broken pipeline, or a mangled
/// artifact all reported the suite green. That was found by breaking the cube
/// shader on purpose and watching every golden pass while the demo refused to
/// start.
///
/// The one legitimate skip is "nothing has been cooked yet", which is a state a
/// fresh clone is genuinely in and which no amount of correct code fixes. That
/// is checked for by name, before anything is constructed, so it cannot swallow
/// a real failure.
fn harness(device: &Arc<Device>, allocator: &Arc<Allocator>) -> Option<Headless> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let vfs = slop_asset::Vfs::for_project(&project);

    for logical in [
        "shaders/passes/cube.spv",
        "shaders/passes/cube.refl",
        "meshes/cube.Cube.0.mesh",
        "textures/checker.tex",
    ] {
        if !vfs.exists(logical) {
            eprintln!("skipping: '{logical}' is not cooked — run `cargo run -p slop-cli -- cook`");
            return None;
        }
    }

    match Headless::new(device, allocator) {
        Ok(headless) => Some(headless),
        Err(failure) => panic!("the scene must build once its assets are cooked: {failure}"),
    }
}

/// The scene plus an offscreen target and the readback path.
struct Headless {
    pool: CommandPool,
    /// The overlay renderer, built here rather than taken from `slop-app`.
    ///
    /// **This is the point of the split.** `slop_editor::DebugUi` needs a
    /// window — it owns the winit glue — and this test has none. `Overlay` is
    /// windowing-agnostic and takes tessellated triangles, so it can be driven
    /// with no event loop, no surface and no display at all. If the two halves
    /// were one type, none of the overlay tests below could exist.
    overlay: Overlay,
    scene: Scene,
    readback: Buffer,
    target: Image,
    allocator: Arc<Allocator>,
    device: Arc<Device>,
}

impl Headless {
    fn new(device: &Arc<Device>, allocator: &Arc<Allocator>) -> Result<Self, String> {
        let extent = vk::Extent2D {
            width: SIZE,
            height: SIZE,
        };

        let mut scene = Scene::new(device, allocator, extent, FORMAT)?;

        // Into the scene's heap, exactly as the windowed demo puts it into the
        // same heap through `DebugUi` — so what this test exercises is the same
        // descriptor arrangement the application uses.
        let vfs = slop_asset::Vfs::for_project(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(".."),
        );
        let module = ShaderModule::from_bytes(
            device,
            &vfs.read("shaders/passes/overlay.spv")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let reflection = slop_asset::Reflection::read(
            &vfs.read("shaders/passes/overlay.refl")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        let overlay = Overlay::new(device, scene.heap_mut(), &module, &reflection, FORMAT)
            .map_err(|error| error.to_string())?;

        let target = Image::new(
            allocator,
            &ImageConfig {
                name: "cube golden target",
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
                name: "cube golden readback",
                size: u64::from(SIZE) * u64::from(SIZE) * Rgba8::CHANNELS as u64,
                usage: vk::BufferUsageFlags::TRANSFER_DST,
                location: MemoryLocation::Readback,
            },
        )
        .map_err(|error| error.to_string())?;

        Ok(Self {
            overlay,
            allocator: Arc::clone(allocator),
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
        self.render_with(frame, &[], 1.0)
    }

    /// Apply egui's atlas changes, outside any recorded frame.
    ///
    /// Into the scene's heap, which is the same one the overlay was built
    /// against — the arrangement `slop_editor::debug` produces in the real
    /// application.
    fn upload_overlay(&mut self, delta: &egui::TexturesDelta) {
        self.overlay
            .update_textures(self.scene.heap_mut(), &self.allocator, delta)
            .expect("the overlay's textures must upload");
    }

    /// Render one frame with an overlay drawn over it.
    fn render_with(
        &mut self,
        frame: u64,
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) -> Rgba8 {
        self.pool.reset().expect("the pool must reset");
        let command = self
            .pool
            .allocate(1)
            .expect("allocation")
            .pop()
            .expect("one buffer was requested");

        command.begin().expect("begin");

        // A `Frame` built by hand rather than by `FrameRenderer`: this test has
        // no swapchain and no window, which is the whole point of it being
        // headless. One slot, because nothing here is in flight.
        //
        // **No overlay primitives, ever.** The debug UI is declared from live
        // state — frame counts, window size — and drawing it here would make the
        // reference depend on something other than the frame number, which is
        // exactly what `docs/DESIGN.md` §2.14 forbids and what makes a golden
        // image of a moving object possible at all.
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

        self.scene.record(&frame);

        if !primitives.is_empty() {
            self.overlay
                .draw(
                    self.scene.heap(),
                    &self.allocator,
                    &frame,
                    primitives,
                    pixels_per_point,
                )
                .expect("the overlay must draw");
        }

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

        slop_rhi::submit_recorded_and_wait(&self.device, &command)
            .expect("the capture submission must complete");

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

#[test]
fn the_debug_overlay_actually_draws_something() {
    // Validation being quiet says the overlay did not crash. It does not say a
    // single pixel changed — an overlay that silently drew nothing would pass
    // every other check in this file, because every other check renders without
    // one.
    //
    // So: the same frame, twice, with and without an overlay. They must differ.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
    };

    let context = egui::Context::default();

    // The screen rectangle is not optional. Without it egui takes the viewport
    // to be zero-sized and clips every widget away, tessellating nothing — which
    // is how this test first failed. `egui-winit` supplies it from the window,
    // so only a hand-built `RawInput` has to.
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(SIZE as f32, SIZE as f32),
        )),
        ..Default::default()
    };

    // Run the same UI twice and keep the second pass. egui measures a window on
    // the pass it first appears and can only place it on the next one, so a
    // single pass emits shapes that tessellate to nothing. A running application
    // never notices; a test that renders exactly one frame does.
    let declare = |ui: &mut egui::Ui| {
        // Filling most of the frame, so this cannot pass by a stray pixel.
        egui::Window::new("overlay")
            .fixed_pos([8.0, 8.0])
            .fixed_size([200.0, 160.0])
            .show(&ui.ctx().clone(), |ui| {
                ui.label("the quick brown fox");
                ui.label("jumps over the lazy dog");
            });
    };

    // Every pass's texture delta has to be applied, not just the last: the font
    // atlas arrives on the *first* pass and the second reports no changes. A
    // running application applies each frame's delta and never notices; keeping
    // only the second here left the atlas unuploaded and every draw skipped.
    let first = context.run_ui(input.clone(), &declare);
    renderer.upload_overlay(&first.textures_delta);

    let output = context.run_ui(input, &declare);
    let primitives = context.tessellate(output.shapes, output.pixels_per_point);

    assert!(
        !primitives.is_empty(),
        "egui must have tessellated something to draw"
    );

    renderer.upload_overlay(&output.textures_delta);

    let without = renderer.render(CAPTURED_FRAME);
    let with = renderer.render_with(CAPTURED_FRAME, &primitives, 1.0);

    // Counted rather than compared whole: `assert_ne!` on two images prints both
    // of them, which is a megabyte of numbers nobody reads.
    let changed = without
        .pixels()
        .iter()
        .zip(with.pixels())
        .filter(|(left, right)| left != right)
        .count();

    assert!(
        changed > 1000,
        "the overlay drew nothing: only {changed} bytes differ with and without it"
    );

    // And the difference must be where the window was, not everywhere — a
    // blend mode that wiped the attachment would also make these differ.
    let corner = |image: &Rgba8, x: u32, y: u32| {
        let at = ((y * image.width() + x) * Rgba8::CHANNELS as u32) as usize;
        image.pixels()[at..at + 4].to_vec()
    };

    assert_eq!(
        corner(&without, SIZE - 4, SIZE - 4),
        corner(&with, SIZE - 4, SIZE - 4),
        "the far corner is outside the overlay and must be untouched"
    );
}

#[test]
fn a_scaled_display_draws_the_same_interface() {
    // The bug this exists for: egui's vertex positions are in points and the
    // scissor rectangles are in physical pixels. Dividing geometry by the
    // physical size draws the interface at 1/scale while its clip rectangles
    // stay full size, so the left edge of every label is shaved off.
    //
    // Invisible at `pixels_per_point == 1.0`, which is what a headless test
    // defaults to and why the first overlay test passed while a 150%-scaled
    // display showed "rame 11447". Rendering the same UI at two scales and
    // requiring both to cover the same area is what catches it.
    let Some((device, allocator)) = headless() else {
        return;
    };

    let Some(mut renderer) = harness(&device, &allocator) else {
        return;
    };

    // Painted at each scale: the same interface in points, so the same region of
    // the screen in pixels, whatever the scale factor says.
    let covered = |renderer: &mut Headless, pixels_per_point: f32| -> usize {
        let context = egui::Context::default();

        // Told to egui, not merely to the renderer. Passing a scale to
        // `tessellate` alone leaves egui laying out at 1.0, so the test would
        // not reproduce a scaled display at all — which is how the first
        // attempt at this test passed against the bug.
        context.set_pixels_per_point(pixels_per_point);

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    SIZE as f32 / pixels_per_point,
                    SIZE as f32 / pixels_per_point,
                ),
            )),
            ..Default::default()
        };
        let declare = |ui: &mut egui::Ui| {
            egui::Window::new("scaled")
                .fixed_pos([4.0, 4.0])
                .show(&ui.ctx().clone(), |ui| ui.label("MMMMMMMM"));
        };

        let first = context.run_ui(input.clone(), &declare);
        renderer.upload_overlay(&first.textures_delta);

        let output = context.run_ui(input, &declare);
        renderer.upload_overlay(&output.textures_delta);

        assert_eq!(
            output.pixels_per_point, pixels_per_point,
            "egui must agree about the scale"
        );

        let primitives = context.tessellate(output.shapes, output.pixels_per_point);
        let plain = renderer.render(CAPTURED_FRAME);
        let drawn = renderer.render_with(CAPTURED_FRAME, &primitives, pixels_per_point);

        plain
            .pixels()
            .iter()
            .zip(drawn.pixels())
            .filter(|(left, right)| left != right)
            .count()
    };

    let unscaled = covered(&mut renderer, 1.0);
    let scaled = covered(&mut renderer, 1.5);

    assert!(unscaled > 1000 && scaled > 1000, "both must draw something");

    // At 1.5x scale the same interface in points covers substantially *more*
    // physical pixels — that is what display scaling is. Not the full 2.25x pure
    // area would predict: coverage is counted as bytes that changed, and a
    // translucent panel over a varying background does not change every byte it
    // covers.
    //
    // The direction is what discriminates. With geometry in physical units and
    // scissors in points, the interface *shrinks* as the scale rises: measured
    // at 0.71x against the bug, 1.61x once fixed.
    let ratio = scaled as f32 / unscaled as f32;
    assert!(
        (1.25..3.0).contains(&ratio),
        "the interface covers {ratio:.2}x the area at 1.5x scale; scaling up must \
         cover more, so geometry and scissors disagree about which units they are in"
    );
}
