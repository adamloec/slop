//! Projection matrices, in the engine's conventions.
//!
//! The conventions table in the crate docs stops being a comment here. Every
//! matrix in this module encodes three decisions at once, and they must agree
//! with two things outside this crate:
//!
//! | Decision | Also lives in |
//! |---|---|
//! | Reversed depth — near maps to 1.0, far to 0.0 | `slop_rhi::DEPTH_COMPARE`, `slop_rhi::DEPTH_CLEAR` |
//! | Depth range `[0, 1]`, not OpenGL's `[-1, 1]` | Vulkan's own clip space |
//! | Framebuffer Y points down | absorbed here, so no viewport flip is needed |
//!
//! Two of the three agreeing produces a plausible image that is wrong. That is
//! why `docs/DESIGN.md` §1.2 principle 6 classed reverse-Z as a rewrite rather
//! than a refactor, and why it was settled at M0 with nothing yet consuming it.
//!
//! # Why reversed depth
//!
//! Floating point has most of its precision near zero. A conventional
//! projection spends most of its depth range near the *far* plane, so the two
//! are exactly mismatched and distant geometry z-fights. Mapping near to 1.0
//! and far to 0.0 aligns them, and the precision is close to free.
//!
//! # Why the Y flip lives here
//!
//! Vulkan's framebuffer origin is top-left with +Y down, while the world is
//! right-handed Y-up. Something has to reconcile them. The alternatives are a
//! negative-height viewport — which works, but silently changes the sense of
//! front-face winding — or flipping in every vertex shader, which is a rule
//! nobody remembers. Folding it into the projection matrix means the rest of the
//! engine never thinks about it, and `slop_rhi`'s counter-clockwise front face
//! stays true as written.

use crate::{Mat4, Vec3};

/// A right-handed perspective projection with reversed depth.
///
/// `vertical_fov` is in radians, `aspect` is width over height. The far plane is
/// at infinity: reversed depth makes an infinite far plane *free* — the
/// precision that would be lost is precision the reversed mapping was not using
/// — and it removes far-plane clipping as a source of popping entirely.
///
/// # Panics
///
/// Panics in debug builds if `aspect` or `near` is not positive. Both produce a
/// matrix full of infinities and NaNs, which propagates into every transformed
/// vertex and shows up as nothing being drawn — a long way from the call that
/// caused it.
pub fn perspective(vertical_fov: f32, aspect: f32, near: f32) -> Mat4 {
    debug_assert!(aspect > 0.0, "aspect ratio must be positive, got {aspect}");
    debug_assert!(near > 0.0, "the near plane must be positive, got {near}");

    let focal = 1.0 / crate::scalar::tan(vertical_fov * 0.5);

    // Column-major construction, matching `glam`'s storage. Reading this as a
    // table of rows is the usual way to get it transposed.
    //
    // The [2][2] and [3][2] entries are what make depth reversed *and*
    // infinite: with them, view-space z of -near maps to 1.0 and z approaching
    // -infinity maps to 0.0. The conventional form has a far term in both.
    //
    // The negated [1][1] is the Y flip described in the module docs.
    Mat4::from_cols_array(&[
        focal / aspect,
        0.0,
        0.0,
        0.0,
        //
        0.0,
        -focal,
        0.0,
        0.0,
        //
        0.0,
        0.0,
        0.0,
        -1.0,
        //
        0.0,
        0.0,
        near,
        0.0,
    ])
}

/// A right-handed orthographic projection with reversed depth.
///
/// For shadow cascades and 2D overlays. Unlike [`perspective`], the far plane is
/// finite and required — an orthographic projection has no perspective divide,
/// so there is no infinite form.
///
/// # Panics
///
/// Panics in debug builds if the volume is degenerate in any axis.
pub fn orthographic(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Mat4 {
    debug_assert!(right > left, "the view volume must have positive width");
    debug_assert!(top > bottom, "the view volume must have positive height");
    debug_assert!(far > near, "the far plane must be beyond the near plane");

    let width = right - left;
    let height = top - bottom;
    let depth = far - near;

    Mat4::from_cols_array(&[
        2.0 / width,
        0.0,
        0.0,
        0.0,
        //
        0.0,
        // Negated, for the same Y flip as `perspective`.
        -2.0 / height,
        0.0,
        0.0,
        //
        0.0,
        0.0,
        // Positive, which is the reversal: view space looks down -Z, so a more
        // distant point has a *more negative* z, and a positive scale turns
        // that into a smaller depth. The conventional non-reversed matrix has
        // `-1 / depth` here, and the sign is the whole difference.
        1.0 / depth,
        0.0,
        //
        -(right + left) / width,
        (top + bottom) / height,
        far / depth,
        1.0,
    ])
}

/// A right-handed view matrix looking from `eye` toward `target`.
///
/// A thin wrapper over `glam`'s `look_at_rh`, present so that call sites name
/// the engine rather than remembering which handedness suffix to reach for —
/// picking `look_at_lh` by mistake mirrors the world, which looks like a
/// modelling bug.
///
/// `up` need not be perpendicular to the view direction, only non-parallel.
pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
    // The `camera::rh` path rather than the deprecated `Mat4::look_at_rh`. The
    // handedness is in the module name here, which is a small improvement on
    // its being in a suffix.
    glam::camera::rh::view::look_at_mat4(eye, target, up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FORWARD, UP, Vec4};

    /// Project a world-space point and perform the perspective divide.
    fn project(matrix: Mat4, point: Vec3) -> Vec3 {
        let clip = matrix * Vec4::new(point.x, point.y, point.z, 1.0);

        Vec3::new(clip.x / clip.w, clip.y / clip.w, clip.z / clip.w)
    }

    #[test]
    fn the_near_plane_maps_to_one_and_distance_decreases_toward_zero() {
        // The single most important property in this file. Getting it backwards
        // is not a validation error and not a crash — the depth test simply
        // keeps the furthest surface at every pixel.
        let projection = perspective(60_f32.to_radians(), 16.0 / 9.0, 0.1);

        // View space looks down -Z, so a point 0.1 ahead is at z = -0.1.
        let near = project(projection, Vec3::new(0.0, 0.0, -0.1));
        let mid = project(projection, Vec3::new(0.0, 0.0, -10.0));
        let far = project(projection, Vec3::new(0.0, 0.0, -10_000.0));

        assert!(
            (near.z - 1.0).abs() < 1e-5,
            "the near plane must map to 1.0, got {}",
            near.z
        );
        assert!(
            near.z > mid.z && mid.z > far.z,
            "depth must decrease with distance: {} {} {}",
            near.z,
            mid.z,
            far.z
        );
        assert!(
            far.z > 0.0 && far.z < 0.001,
            "a very distant point should approach 0.0, got {}",
            far.z
        );
    }

    #[test]
    fn depth_stays_inside_the_zero_to_one_range() {
        // Vulkan clip space, not OpenGL's [-1, 1]. A matrix built for OpenGL
        // puts half the scene behind the near plane, where it is clipped away.
        let projection = perspective(90_f32.to_radians(), 1.0, 0.05);

        for step in 0..1000 {
            let distance = 0.05 + step as f32 * 5.0;
            let depth = project(projection, Vec3::new(0.0, 0.0, -distance)).z;

            assert!(
                (0.0..=1.0).contains(&depth),
                "depth {depth} escaped [0, 1] at {distance}"
            );
        }
    }

    #[test]
    fn the_projection_flips_y_for_vulkans_framebuffer() {
        // A point above the centre in world space must land in the *upper* half
        // of the framebuffer, which is negative Y in Vulkan's clip space. Miss
        // this and the whole scene renders upside down — which looks correct on
        // a symmetric test scene, and is why a cube is a better check than a
        // sphere.
        let projection = perspective(90_f32.to_radians(), 1.0, 0.1);
        let above = project(projection, Vec3::new(0.0, 1.0, -2.0));

        assert!(
            above.y < 0.0,
            "world-space up must map to negative clip Y, got {}",
            above.y
        );
    }

    #[test]
    fn x_is_not_flipped() {
        // Guards against fixing the Y flip by negating the wrong column, which
        // mirrors the scene instead — invisible on symmetric content and
        // obvious on text.
        let projection = perspective(90_f32.to_radians(), 1.0, 0.1);
        let right = project(projection, Vec3::new(1.0, 0.0, -2.0));

        assert!(
            right.x > 0.0,
            "world-space right must stay positive in clip X, got {}",
            right.x
        );
    }

    #[test]
    fn a_wider_aspect_ratio_compresses_x_and_leaves_y_alone() {
        let square = perspective(90_f32.to_radians(), 1.0, 0.1);
        let wide = perspective(90_f32.to_radians(), 2.0, 0.1);

        let point = Vec3::new(1.0, 1.0, -2.0);

        assert!(project(wide, point).x < project(square, point).x);
        assert!((project(wide, point).y - project(square, point).y).abs() < 1e-6);
    }

    #[test]
    fn the_view_matrix_puts_the_target_straight_ahead() {
        // Straight ahead in view space is -Z, per the crate's conventions.
        let view = look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, UP);
        let origin = view.transform_point3(Vec3::ZERO);

        assert!((origin.x).abs() < 1e-6);
        assert!((origin.y).abs() < 1e-6);
        assert!(
            origin.z < 0.0,
            "the target must land down -Z, got {}",
            origin.z
        );
        assert!((origin.z + 5.0).abs() < 1e-5, "and at the eye's distance");
    }

    #[test]
    fn the_view_matrix_agrees_with_the_forward_constant() {
        // `FORWARD` is -Z. A camera at the origin looking along it must leave a
        // point in front of it in front of it.
        let view = look_at(Vec3::ZERO, FORWARD, UP);
        let ahead = view.transform_point3(FORWARD * 3.0);

        assert!((ahead.z + 3.0).abs() < 1e-5, "got {}", ahead.z);
    }

    #[test]
    fn orthographic_depth_is_reversed_too() {
        // The same reversal, in the form an orthographic matrix takes. Shadow
        // cascades use this, and a cascade with conventional depth against a
        // reversed comparison renders shadows inside out.
        let projection = orthographic(-1.0, 1.0, -1.0, 1.0, 0.1, 100.0);

        let near = project(projection, Vec3::new(0.0, 0.0, -0.1));
        let far = project(projection, Vec3::new(0.0, 0.0, -100.0));

        assert!(
            (near.z - 1.0).abs() < 1e-5,
            "near must be 1.0, got {}",
            near.z
        );
        assert!(far.z.abs() < 1e-5, "far must be 0.0, got {}", far.z);
    }

    #[test]
    fn orthographic_maps_its_volume_to_the_unit_square() {
        let projection = orthographic(-4.0, 4.0, -2.0, 2.0, 0.1, 10.0);

        let corner = project(projection, Vec3::new(4.0, 2.0, -1.0));

        assert!((corner.x - 1.0).abs() < 1e-6, "got {}", corner.x);
        // Negative, because of the same Y flip.
        assert!((corner.y + 1.0).abs() < 1e-6, "got {}", corner.y);
    }
}
