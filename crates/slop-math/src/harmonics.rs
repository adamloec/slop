//! Spherical harmonics — a low-frequency function on the sphere, in nine numbers.
//!
//! `docs/PLAN.md` §9.7 E6b. Here rather than in `slop-render` or `slop-cook`
//! because both need it and neither owns it: the cooker **projects** an
//! environment onto this basis, the renderer **carries** the result to the GPU,
//! and the shader **evaluates** it. One definition, or the projection and the
//! evaluation drift and the error looks like a strangely-lit scene.
//!
//! # Why nine numbers are enough, and only for this
//!
//! Diffuse irradiance is not an approximation of the environment — it is the
//! environment convolved with a cosine lobe, and that lobe is so wide that
//! everything above the second band is already gone. Ramamoorthi and Hanrahan's
//! result is that nine coefficients reconstruct it to within about one percent
//! for any input, which is why every engine stores diffuse ambient this way and
//! **none** stores specular this way. A sharp reflection is exactly the
//! high-frequency content this basis discards; that is E6c's prefiltered cube.
//!
//! So: 108 bytes for the diffuse term, against an irradiance cube map's image,
//! view, sampler, heap slot and upload.
//!
//! # The basis polar axis is Z, and the world's is Y
//!
//! The formulas below are the textbook ones, written with `z` as the polar axis,
//! applied to a world-space direction whose up axis is `y`. That is a rotation of
//! the basis and nothing more: the basis is complete, so a rotated one represents
//! the same functions, and **projection and evaluation use the same assignment**.
//! Stated because it looks like a bug on first reading and is not one.

use crate::Vec3;

/// How many coefficients an order-3 (two-band) expansion has.
pub const COEFFICIENTS: usize = 9;

/// The normalisation constants of the real spherical harmonic basis.
///
/// Written out rather than computed so they are visible at the point of use, and
/// checked against their closed forms by `the_basis_constants_are_what_they_say`.
const K0: f32 = 0.282_094_79; // 1/2 · √(1/π)
const K1: f32 = 0.488_602_51; // √(3/4π)
const K2: f32 = 1.092_548_4; // 1/2 · √(15/π)
const K20: f32 = 0.315_391_57; // 1/4 · √(5/π)
const K22: f32 = 0.546_274_2; // 1/4 · √(15/π)

/// Per-band weights for convolving radiance into diffuse reflectance.
///
/// The cosine lobe's own expansion, `Â_l`, divided by π — because what a shading
/// pass wants is the number a Lambertian albedo multiplies, not the irradiance
/// itself. `Â_0 = π`, `Â_1 = 2π/3`, `Â_2 = π/4`, so dividing through gives these.
///
/// The third band being a quarter and the fourth being zero is the whole reason
/// nine coefficients suffice: the lobe has almost no energy up there to keep.
const BANDS: [f32; 3] = [1.0, 2.0 / 3.0, 0.25];

/// The nine basis functions evaluated along `direction`.
///
/// `direction` must be normalised; the second-band terms are quadratic in it, so
/// a longer vector does not merely scale the result.
#[must_use]
pub fn basis(direction: Vec3) -> [f32; COEFFICIENTS] {
    let Vec3 { x, y, z } = direction;

    [
        K0,
        K1 * y,
        K1 * z,
        K1 * x,
        K2 * x * y,
        K2 * y * z,
        K20 * 3.0f32.mul_add(z * z, -1.0),
        K2 * x * z,
        K22 * (x * x - y * y),
    ]
}

/// A three-channel function on the sphere, to second order.
///
/// The coefficients are **raw projections** of radiance onto [`basis`] — a plain
/// mathematical object, with no shading convention baked in. The cosine
/// convolution happens at evaluation, in [`Sh9::diffuse`] and in its shader
/// twin, so the number stored in a cooked artifact means the same thing whatever
/// a renderer later does with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sh9 {
    /// One coefficient per basis function, in [`basis`]'s order.
    pub coefficients: [Vec3; COEFFICIENTS],
}

impl Default for Sh9 {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Sh9 {
    /// All coefficients zero — a black environment.
    pub const ZERO: Self = Self {
        coefficients: [Vec3::ZERO; COEFFICIENTS],
    };

    /// The projection of a uniform field of `radiance`.
    ///
    /// Only the constant band is non-zero, which is the definition of uniform.
    /// The factor is `∫ Y₀₀ dω = 2√π`, and [`Sh9::diffuse`] gives `radiance`
    /// back for every normal — the property `a_constant_field_reconstructs_to_
    /// itself` asserts, and the one that lets a caller with no environment
    /// render exactly as it did before there was one.
    #[must_use]
    pub fn constant(radiance: Vec3) -> Self {
        // 2√π, which is 4π · Y₀₀.
        const FULL_SPHERE: f32 = 3.544_907_7;

        let mut sh = Self::ZERO;
        sh.coefficients[0] = radiance * FULL_SPHERE;

        sh
    }

    /// Add one sample of `radiance` arriving from `direction`, covering
    /// `solid_angle` steradians.
    ///
    /// The projection integral, one term at a time. The caller supplies the solid
    /// angle rather than this deriving it, because what a sample covers depends
    /// on the parameterisation being integrated over — a cube texel and a
    /// panorama texel cover very different amounts of sky for the same pixel
    /// count, and getting that weight wrong is the classic way an environment
    /// comes out tinted by whatever is at its poles.
    pub fn accumulate(&mut self, direction: Vec3, radiance: Vec3, solid_angle: f32) {
        let basis = basis(direction);

        for (coefficient, weight) in self.coefficients.iter_mut().zip(basis) {
            *coefficient += radiance * (weight * solid_angle);
        }
    }

    /// What a Lambertian albedo facing `normal` multiplies.
    ///
    /// The irradiance arriving at that surface **divided by π** — because a
    /// Lambertian surface reflects `albedo / π · E`, and returning `E` would
    /// leave every caller to remember the division. Naming it `irradiance` would
    /// be naming it after the quantity it is not.
    ///
    /// **This is the CPU twin of the shader**, in the sense
    /// `slop-render/tests/cluster.rs` established: the value that ships is
    /// computed by `irradianceFrom` in `shaders/lib/environment.slang`, and this
    /// exists so the maths can be checked without a GPU. The two are the same
    /// formula written twice, which is a real cost — what makes it worth paying
    /// is that a wrong band weight is invisible in a rendered image and obvious
    /// against `a_constant_field_reconstructs_to_itself`.
    #[must_use]
    pub fn diffuse(&self, normal: Vec3) -> Vec3 {
        let basis = basis(normal);

        let mut total = Vec3::ZERO;
        for (index, (coefficient, weight)) in self.coefficients.iter().zip(basis).enumerate() {
            // Band 0 is index 0, band 1 is indices 1..4, band 2 is 4..9.
            let band = match index {
                0 => BANDS[0],
                1..=3 => BANDS[1],
                _ => BANDS[2],
            };

            total += *coefficient * (weight * band);
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_basis_constants_are_what_they_say() {
        // Written out for readability at the point of use, checked against their
        // closed forms here. A digit wrong in any of them tilts the
        // reconstruction in a way that looks like a differently-lit scene.
        let pi = std::f32::consts::PI;

        assert!((K0 - 0.5 * (1.0 / pi).sqrt()).abs() < 1e-7);
        assert!((K1 - (3.0 / (4.0 * pi)).sqrt()).abs() < 1e-7);
        assert!((K2 - 0.5 * (15.0 / pi).sqrt()).abs() < 1e-6);
        assert!((K20 - 0.25 * (5.0 / pi).sqrt()).abs() < 1e-7);
        assert!((K22 - 0.25 * (15.0 / pi).sqrt()).abs() < 1e-7);
    }

    /// Directions spread over the sphere, for asserting something everywhere.
    fn directions() -> Vec<Vec3> {
        let mut spread = vec![
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::NEG_Z,
        ];

        // A deterministic spiral, so this covers the space between the axes
        // without an RNG — which `docs/DESIGN.md` §2.14 would want seeded anyway.
        for index in 0..64 {
            let t = (index as f32 + 0.5) / 64.0;
            let y = 1.0f32 - 2.0 * t;
            let radius = (1.0 - y * y).max(0.0).sqrt();
            let angle = 2.399_963_2 * index as f32;

            spread.push(Vec3::new(
                radius * crate::scalar::cos(angle),
                y,
                radius * crate::scalar::sin(angle),
            ));
        }

        spread
    }

    #[test]
    fn a_constant_field_reconstructs_to_itself() {
        // **The test this module exists to pass.** A uniform environment must
        // light every direction identically and by exactly its own radiance —
        // which is what makes the band weights and the `2√π` checkable at all.
        //
        // Get either wrong and the reconstruction is uniform but the wrong
        // brightness, which in a rendered image looks like an exposure choice.
        // It is also what lets a caller with no cooked environment pass
        // `Sh9::constant(...)` and render exactly as it did before.
        let radiance = Vec3::new(0.18, 0.19, 0.22);
        let sh = Sh9::constant(radiance);

        for direction in directions() {
            let reconstructed = sh.diffuse(direction.normalize());

            assert!(
                (reconstructed - radiance).length() < 1e-5,
                "{direction:?} reconstructed to {reconstructed:?}, not {radiance:?}"
            );
        }
    }

    #[test]
    fn projecting_a_uniform_field_by_integration_matches_the_closed_form() {
        // `constant` is a shortcut for an integral, and this is the integral. If
        // the two disagree, either the `2√π` is wrong or `accumulate` weights its
        // samples wrongly — and both produce a plausible image.
        let radiance = Vec3::new(0.4, 0.6, 0.9);
        let samples = directions();
        let solid_angle = 4.0 * std::f32::consts::PI / samples.len() as f32;

        let mut integrated = Sh9::ZERO;
        for direction in &samples {
            integrated.accumulate(direction.normalize(), radiance, solid_angle);
        }

        let closed = Sh9::constant(radiance);

        assert!(
            (integrated.coefficients[0] - closed.coefficients[0]).length() < 0.05,
            "integrated {:?} against closed form {:?}",
            integrated.coefficients[0],
            closed.coefficients[0]
        );
    }

    #[test]
    fn a_light_from_one_side_is_brightest_facing_it() {
        // Direction, not just magnitude — the property a constant field cannot
        // check. A basis with a sign wrong in the first band reconstructs a
        // scene lit from the opposite side, which is entirely plausible to look
        // at and is the single most likely mistake here.
        let mut sh = Sh9::ZERO;
        sh.accumulate(Vec3::Y, Vec3::ONE, 1.0);

        let facing = sh.diffuse(Vec3::Y).x;
        let away = sh.diffuse(Vec3::NEG_Y).x;

        assert!(
            facing > away,
            "a light from +Y gives {facing} facing it and {away} facing away"
        );

        for axis in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z] {
            let sideways = sh.diffuse(axis).x;

            assert!(
                facing > sideways && sideways > away,
                "a light from +Y gives {sideways} at {axis:?}, which is not between \
                 {away} and {facing}"
            );
        }
    }

    #[test]
    fn every_axis_direction_is_distinguished() {
        // Six lights, six answers. A basis that collapsed two axes — the kind of
        // mistake a copied formula makes — would light two opposite walls
        // identically, and nothing else here would notice.
        let mut brightest = Vec::new();

        for source in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::NEG_Z,
        ] {
            let mut sh = Sh9::ZERO;
            sh.accumulate(source, Vec3::ONE, 1.0);

            let facing = sh.diffuse(source).x;

            for other in [
                Vec3::X,
                Vec3::NEG_X,
                Vec3::Y,
                Vec3::NEG_Y,
                Vec3::Z,
                Vec3::NEG_Z,
            ] {
                if other != source {
                    assert!(
                        facing > sh.diffuse(other).x + 1e-4,
                        "a light from {source:?} is not brighter facing it than at {other:?}"
                    );
                }
            }

            brightest.push(facing);
        }

        // And by the same amount each time: the basis must not favour an axis.
        for value in &brightest {
            assert!(
                (value - brightest[0]).abs() < 1e-5,
                "the basis is not isotropic: {brightest:?}"
            );
        }
    }

    #[test]
    fn accumulation_is_linear() {
        // Two lights projected together equal the two projected separately and
        // added, which is what makes a cook that partitions across threads give
        // the same answer as one that does not — `docs/DESIGN.md` §2.14's
        // requirement that a parallel result not depend on the partitioning.
        let mut both = Sh9::ZERO;
        both.accumulate(Vec3::Y, Vec3::new(1.0, 0.0, 0.0), 0.7);
        both.accumulate(Vec3::X, Vec3::new(0.0, 1.0, 0.0), 0.3);

        let mut first = Sh9::ZERO;
        first.accumulate(Vec3::Y, Vec3::new(1.0, 0.0, 0.0), 0.7);

        let mut second = Sh9::ZERO;
        second.accumulate(Vec3::X, Vec3::new(0.0, 1.0, 0.0), 0.3);

        for index in 0..COEFFICIENTS {
            let summed = first.coefficients[index] + second.coefficients[index];

            assert!(
                (both.coefficients[index] - summed).length() < 1e-6,
                "coefficient {index} is not linear"
            );
        }
    }

    #[test]
    fn a_black_environment_reflects_nothing() {
        for direction in directions() {
            assert_eq!(Sh9::ZERO.diffuse(direction.normalize()), Vec3::ZERO);
        }
    }
}
