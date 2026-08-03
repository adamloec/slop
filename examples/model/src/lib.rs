//! What the windowed viewer and the headless golden test both need.
//!
//! The split exists for one reason: **the thing a human looks at and the thing
//! CI checks must be the same code.** A golden test that framed the model with
//! its own camera would still catch a broken `MeshRenderer`, and would stop
//! catching a broken *camera* — and worse, would drift from the demo silently,
//! so that "the test passes" and "the window looks right" gradually stop being
//! the same claim.
//!
//! `main.rs` keeps everything a window implies: the event loop, the swapchain,
//! the debug UI, the world the inspector edits. None of that can exist headless,
//! and none of it is what a golden image is checking.

use slop_asset::Vfs;
use slop_math::{Mat4, Quat, Vec3};

/// Which model to draw, when nothing says otherwise.
pub const DEFAULT_MODEL: &str = "models/cube.model";

/// Vertical field of view, in degrees.
pub const FIELD_OF_VIEW: f32 = 55.0;

/// Radians of orbit per frame.
///
/// Per *frame* rather than per second — `docs/DESIGN.md` §2.14 makes the frame
/// number the only clock a reproducible render may read, and that is exactly
/// what makes a golden image of a moving camera possible at all.
pub const RADIANS_PER_FRAME: f32 = 0.006;

/// The orbiting camera.
///
/// A component in the windowed viewer, so the inspector can edit it live — every
/// field appears in the debug UI without a line of UI code naming it, because
/// `slop-reflect` describes the type and `slop_editor::inspector` walks the
/// description.
///
/// This is not the scene representation. `docs/DESIGN.md` gives `slop-scene` the
/// runtime tree at M5, and putting the *model's* geometry into a world is that
/// work rather than this.
#[derive(slop_reflect::Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct OrbitCamera {
    /// Distance from the model's centre.
    pub distance: f32,
    /// How far above the centre the camera sits, as a fraction of `distance`.
    pub height: f32,
    /// Radians of orbit per frame. See [`RADIANS_PER_FRAME`].
    pub radians_per_frame: f32,
    /// Vertical field of view, in degrees.
    pub field_of_view: f32,
    /// Whether the camera advances. Off freezes it where it is, which is what
    /// makes a still worth looking at.
    pub orbiting: bool,
}

impl OrbitCamera {
    /// A camera framing a model of the given radius.
    ///
    /// Far enough that the whole model fits the vertical field of view, with a
    /// margin so nothing is clipped by the near plane.
    #[must_use]
    pub fn framing(radius: f32) -> Self {
        Self {
            distance: radius / slop_math::scalar::tan(FIELD_OF_VIEW.to_radians() * 0.5) * 1.4,
            height: 0.35,
            radians_per_frame: RADIANS_PER_FRAME,
            field_of_view: FIELD_OF_VIEW,
            orbiting: true,
        }
    }
}

/// The view-projection matrix for one frame.
///
/// `angle` is accumulated by the caller rather than derived from a frame number,
/// so that editing `radians_per_frame` takes effect from *now* instead of
/// teleporting the camera to wherever the new speed says frame 5000 should be.
#[must_use]
pub fn camera(aspect: f32, centre: Vec3, angle: f32, settings: OrbitCamera) -> Mat4 {
    let distance = settings.distance;
    let eye = centre
        + Quat::from_rotation_y(angle) * Vec3::new(0.0, distance * settings.height, distance);

    let view = slop_math::look_at(eye, centre, slop_math::UP);

    // The engine's own projection, not glam's: it is reverse-Z and infinite,
    // matching the depth comparison `slop-rhi` configures, and it flips Y for
    // Vulkan's clip space. Reaching for `Mat4::perspective_rh` here would draw
    // the model inverted and fail every depth test.
    //
    // The near plane scales with the model so that a metre-wide cube and a
    // hundred-metre building both get sensible precision.
    let projection = slop_math::perspective(
        settings.field_of_view.to_radians(),
        aspect,
        distance * 0.005,
    );

    projection * view
}

/// The model's centre and radius, from the meshes it names.
///
/// Read from the cooked artifacts rather than guessed, so pointing this at
/// Sponza frames Sponza and pointing it at a cube frames a cube. A renderer that
/// needed a hand-tuned camera per asset would be a demo rather than a viewer.
#[must_use]
pub fn bounds(vfs: &Vfs, logical: &str) -> (Vec3, f32) {
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

    (centre, (max - min).length() * 0.5)
}

/// Cooked assets, found by walking up from wherever this was run.
///
/// The starting directory is chosen here rather than inside `slop-asset` —
/// `docs/CONVENTIONS.md` §5.1 keeps a library from reading the environment on
/// its caller's behalf, and which directory is right depends on how the program
/// was launched.
///
/// # Errors
///
/// Fails if the current directory cannot be read, or if no ancestor of it holds
/// a cooked cache — which in a fresh clone means `cook` has not been run.
pub fn assets() -> Result<Vfs, String> {
    let here = std::env::current_dir().map_err(|error| error.to_string())?;

    Vfs::discover(&here).map_err(|error| error.to_string())
}
