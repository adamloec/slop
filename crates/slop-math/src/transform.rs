//! Translation, rotation and scale as separate components.
//!
//! Kept decomposed rather than stored as a [`Mat4`] because the engine needs the
//! parts individually far more often than it needs the product: the scene graph
//! composes them, the editor edits them, serialization writes them, and
//! `docs/DESIGN.md` §2.7's interpolated rendering blends them — and blending
//! matrices is not the same as blending the transforms they represent. A matrix
//! lerp of two rotations shrinks and skews the object; a quaternion slerp does
//! not.

use glam::{Mat4, Quat, Vec3};

/// A translation, rotation and scale, applied in that order: scale first, then
/// rotate, then translate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Position in the parent's space.
    pub translation: Vec3,
    /// Orientation. Assumed normalized; the constructors keep it so.
    pub rotation: Quat,
    /// Per-axis scale. Non-uniform values are permitted but see
    /// [`Transform::compose`].
    pub scale: Vec3,
}

impl Transform {
    /// No translation, no rotation, unit scale.
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    /// A transform with only a position.
    pub const fn from_translation(translation: Vec3) -> Self {
        Self {
            translation,
            ..Self::IDENTITY
        }
    }

    /// A transform with only an orientation.
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self {
            rotation,
            ..Self::IDENTITY
        }
    }

    /// A transform with only a scale.
    pub const fn from_scale(scale: Vec3) -> Self {
        Self {
            scale,
            ..Self::IDENTITY
        }
    }

    /// Replace the translation.
    #[must_use]
    pub const fn with_translation(mut self, translation: Vec3) -> Self {
        self.translation = translation;
        self
    }

    /// Replace the rotation.
    #[must_use]
    pub const fn with_rotation(mut self, rotation: Quat) -> Self {
        self.rotation = rotation;
        self
    }

    /// Replace the scale.
    #[must_use]
    pub const fn with_scale(mut self, scale: Vec3) -> Self {
        self.scale = scale;
        self
    }

    /// Collapse to a single matrix, scale then rotation then translation.
    pub fn to_matrix(self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Recover a transform from a matrix.
    ///
    /// Exact for any matrix this type could have produced. A matrix carrying
    /// shear — which composing non-uniform scale with rotation can introduce —
    /// has no exact TRS form, and the result is `glam`'s best approximation.
    pub fn from_matrix(matrix: Mat4) -> Self {
        let (scale, rotation, translation) = matrix.to_scale_rotation_translation();

        Self {
            translation,
            rotation,
            scale,
        }
    }

    /// The inverse, as a matrix.
    ///
    /// Deliberately returns a [`Mat4`] rather than a `Transform`. The inverse of
    /// a TRS with non-uniform scale is not itself a TRS — it needs the scale
    /// applied after the rotation, which this type cannot express — so a
    /// `Transform`-returning `inverse` would be silently wrong for exactly the
    /// cases where it matters. Returning a matrix is correct in every case and
    /// honest about what it is.
    pub fn inverse_matrix(self) -> Mat4 {
        self.to_matrix().inverse()
    }

    /// Apply to a point: scaled, rotated, then translated.
    pub fn transform_point(self, point: Vec3) -> Vec3 {
        self.rotation * (self.scale * point) + self.translation
    }

    /// Apply to a direction: scaled and rotated, but not translated.
    ///
    /// Note this is not the right transform for a **normal** under non-uniform
    /// scale — normals need the inverse transpose, or they stop being
    /// perpendicular to the surface. That belongs with the shading code that
    /// needs it, not here.
    pub fn transform_vector(self, vector: Vec3) -> Vec3 {
        self.rotation * (self.scale * vector)
    }

    /// Compose: `self` is the parent, `child` is expressed in `self`'s space.
    ///
    /// # Accuracy
    ///
    /// Exact when the parent's scale is uniform, or when parent and child
    /// rotations are axis-aligned with the scale. Composing a non-uniform parent
    /// scale with a child rotation genuinely produces shear, which no TRS triple
    /// can represent, so the result is an approximation.
    ///
    /// This is not a defect of this implementation — it is a property of
    /// decomposed transforms, and every engine using them has it. Scene
    /// hierarchies that need exactness under non-uniform scale must compose
    /// matrices via [`to_matrix`](Self::to_matrix) instead.
    #[must_use]
    pub fn compose(self, child: Self) -> Self {
        Self {
            translation: self.transform_point(child.translation),
            rotation: self.rotation * child.rotation,
            scale: self.scale * child.scale,
        }
    }

    /// Blend toward `target`, for `docs/DESIGN.md` §2.7's interpolated
    /// rendering.
    ///
    /// Translation and scale interpolate linearly; rotation uses spherical
    /// interpolation, so the object turns at a constant rate along the shortest
    /// arc instead of accelerating and shrinking through the midpoint.
    ///
    /// `alpha` is the accumulator fraction from `slop_core::FixedTimestep`, in
    /// `0.0..1.0`. Deliberately not an intra-doc link: `slop-math` sits below
    /// `slop-core` in the crate graph and must not depend on it just to
    /// document a relationship. Values outside the range extrapolate rather than
    /// clamp, which is occasionally useful and never silently wrong.
    #[must_use]
    pub fn interpolate(self, target: Self, alpha: f32) -> Self {
        Self {
            translation: self.translation.lerp(target.translation, alpha),
            rotation: self.rotation.slerp(target.rotation, alpha),
            scale: self.scale.lerp(target.scale, alpha),
        }
    }

    /// The local forward axis in world space — the rotation applied to
    /// [`FORWARD`](crate::FORWARD).
    pub fn forward(self) -> Vec3 {
        self.rotation * crate::FORWARD
    }

    /// The local right axis in world space.
    pub fn right(self) -> Vec3 {
        self.rotation * crate::RIGHT
    }

    /// The local up axis in world space.
    pub fn up(self) -> Vec3 {
        self.rotation * crate::UP
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl From<Transform> for Mat4 {
    fn from(transform: Transform) -> Self {
        transform.to_matrix()
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_2;

    use super::*;

    /// Loose enough to absorb quaternion round-tripping, tight enough that a
    /// wrong axis or sign fails.
    const EPSILON: f32 = 1e-5;

    fn quarter_turn_about_y() -> Quat {
        Quat::from_rotation_y(FRAC_PI_2)
    }

    #[test]
    fn identity_leaves_points_untouched() {
        let point = Vec3::new(1.0, 2.0, 3.0);

        assert_eq!(Transform::IDENTITY.transform_point(point), point);
        assert_eq!(Transform::default(), Transform::IDENTITY);
    }

    #[test]
    fn matrix_round_trips() {
        let transform = Transform {
            translation: Vec3::new(1.0, -2.0, 3.0),
            rotation: quarter_turn_about_y(),
            scale: Vec3::splat(2.0),
        };

        let restored = Transform::from_matrix(transform.to_matrix());

        assert!(
            restored
                .translation
                .abs_diff_eq(transform.translation, EPSILON)
        );
        assert!(restored.scale.abs_diff_eq(transform.scale, EPSILON));
        assert!(restored.rotation.abs_diff_eq(transform.rotation, EPSILON));
    }

    #[test]
    fn point_is_scaled_then_rotated_then_translated() {
        // Order matters: scaling after rotation would move the point elsewhere.
        let transform = Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            rotation: quarter_turn_about_y(),
            scale: Vec3::splat(2.0),
        };

        let moved = transform.transform_point(Vec3::X);

        // X scaled to 2, a quarter turn about Y sends +X to -Z, then translate.
        assert!(
            moved.abs_diff_eq(Vec3::new(10.0, 0.0, -2.0), EPSILON),
            "got {moved}"
        );
    }

    #[test]
    fn transform_vector_ignores_translation() {
        let transform = Transform::from_translation(Vec3::new(100.0, 100.0, 100.0))
            .with_scale(Vec3::splat(3.0));

        assert!(
            transform
                .transform_vector(Vec3::X)
                .abs_diff_eq(Vec3::new(3.0, 0.0, 0.0), EPSILON)
        );
    }

    #[test]
    fn composition_matches_matrix_multiplication() {
        // The property that lets the scene graph compose transforms instead of
        // matrices. Uniform scale, where the decomposed form is exact.
        let parent = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: quarter_turn_about_y(),
            scale: Vec3::splat(2.0),
        };
        let child = Transform {
            translation: Vec3::new(0.0, 1.0, 0.0),
            rotation: Quat::from_rotation_x(FRAC_PI_2),
            scale: Vec3::splat(0.5),
        };
        let point = Vec3::new(1.0, -1.0, 2.0);

        let composed = parent.compose(child).transform_point(point);
        let via_matrices = (parent.to_matrix() * child.to_matrix()).transform_point3(point);

        assert!(
            composed.abs_diff_eq(via_matrices, EPSILON),
            "composed {composed} vs matrices {via_matrices}"
        );
    }

    #[test]
    fn composing_with_identity_changes_nothing() {
        let transform = Transform {
            translation: Vec3::new(4.0, 5.0, 6.0),
            rotation: quarter_turn_about_y(),
            scale: Vec3::new(1.0, 2.0, 3.0),
        };

        let left = Transform::IDENTITY.compose(transform);
        let right = transform.compose(Transform::IDENTITY);

        assert!(left.translation.abs_diff_eq(transform.translation, EPSILON));
        assert!(
            right
                .translation
                .abs_diff_eq(transform.translation, EPSILON)
        );
        assert!(left.scale.abs_diff_eq(transform.scale, EPSILON));
    }

    #[test]
    fn inverse_matrix_undoes_the_transform() {
        // Non-uniform scale on purpose: this is exactly the case a
        // Transform-returning `inverse` would get silently wrong.
        let transform = Transform {
            translation: Vec3::new(3.0, -1.0, 7.0),
            rotation: quarter_turn_about_y(),
            scale: Vec3::new(1.0, 2.0, 4.0),
        };
        let point = Vec3::new(5.0, 6.0, -2.0);

        let round_tripped = transform
            .inverse_matrix()
            .transform_point3(transform.transform_point(point));

        assert!(
            round_tripped.abs_diff_eq(point, EPSILON),
            "got {round_tripped}"
        );
    }

    #[test]
    fn interpolation_reaches_both_endpoints() {
        let a = Transform::from_translation(Vec3::ZERO);
        let b = Transform::from_translation(Vec3::new(10.0, 0.0, 0.0))
            .with_rotation(quarter_turn_about_y());

        assert!(
            a.interpolate(b, 0.0)
                .translation
                .abs_diff_eq(a.translation, EPSILON)
        );
        assert!(
            a.interpolate(b, 1.0)
                .translation
                .abs_diff_eq(b.translation, EPSILON)
        );
    }

    #[test]
    fn interpolation_is_halfway_at_alpha_one_half() {
        let a = Transform::from_translation(Vec3::ZERO);
        let b = Transform::from_translation(Vec3::new(10.0, 20.0, -4.0));

        let mid = a.interpolate(b, 0.5);

        assert!(
            mid.translation
                .abs_diff_eq(Vec3::new(5.0, 10.0, -2.0), EPSILON)
        );
    }

    #[test]
    fn interpolated_rotation_keeps_unit_length() {
        // The reason rotation slerps rather than lerps: a linear blend of two
        // quaternions is not normalized, and an object rendered through it
        // visibly shrinks through the midpoint.
        let a = Transform::IDENTITY;
        let b = Transform::from_rotation(Quat::from_rotation_y(std::f32::consts::PI * 0.75));

        for step in 0..=10 {
            let alpha = step as f32 / 10.0;
            let length = a.interpolate(b, alpha).rotation.length();

            assert!(
                (length - 1.0).abs() < EPSILON,
                "alpha {alpha} gave length {length}"
            );
        }
    }

    #[test]
    fn local_axes_follow_the_stated_convention() {
        // Guards the crate-level conventions table. Forward is -Z, and an
        // unrotated transform's axes are the world axes.
        let identity = Transform::IDENTITY;

        assert!(identity.forward().abs_diff_eq(Vec3::NEG_Z, EPSILON));
        assert!(identity.right().abs_diff_eq(Vec3::X, EPSILON));
        assert!(identity.up().abs_diff_eq(Vec3::Y, EPSILON));
    }

    #[test]
    fn a_quarter_turn_about_up_sends_forward_to_left() {
        // Pins the rotation handedness. Right-handed, counter-clockwise viewed
        // from +Y: -Z rotates toward -X.
        let turned = Transform::from_rotation(quarter_turn_about_y());

        assert!(
            turned.forward().abs_diff_eq(Vec3::NEG_X, EPSILON),
            "got {}",
            turned.forward()
        );
    }
}
