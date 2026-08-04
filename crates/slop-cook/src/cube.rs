//! A cube of radiance, and the mapping between a face texel and a direction.
//!
//! `docs/PLAN.md` §9.7 E6a. What a [`Panorama`](crate::panorama::Panorama) is
//! projected onto, what the spherical-harmonic projection reads, and what the
//! prefilter both reads and writes.
//!
//! # Why a cube rather than the panorama itself
//!
//! §9.7 argues it against octahedral mapping and the short version is seams: a
//! cube's faces filter into each other in hardware, and every alternative needs a
//! hand-maintained border on every mip level. But the reason it happens *here*,
//! at cook time, is different — the equirectangular parameterisation wildly
//! oversamples the poles, so integrating over it directly weights the top and
//! bottom of the sky by the several thousand texels they occupy rather than by
//! the solid angle they cover. A cube's distortion is bounded and correctable.
//!
//! # The face table is the most error-prone thing here
//!
//! Six faces, each with its own axis order and two sign flips, and a mistake in
//! any of them renders as an environment that is rotated, mirrored, or has two
//! faces transposed. That reads as "the source is odd" rather than as a bug, and
//! no reference image distinguishes it. So [`direction_of`] and [`face_of`] are
//! inverses and `every_face_survives_the_round_trip` is what says so.
//!
//! The table is Vulkan's, which is Direct3D's — the layer order `+X, -X, +Y, -Y,
//! +Z, -Z` and the `s`/`t` axes per face are fixed by the API, not chosen here.
//! Choosing differently would mean the CPU and the hardware sampler disagreed
//! about which texel a direction lands on, which is not a thing that can be
//! debugged from an image.

use slop_math::{Sh9, Vec3, scalar};

use crate::panorama::Panorama;

/// How many faces a cube has, which is also its array layer count.
pub(crate) const FACES: usize = 6;

/// One face of a cube map, in Vulkan's array-layer order.
///
/// The discriminants are the layer indices, so a face and the layer it uploads
/// into cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Face {
    PositiveX = 0,
    NegativeX = 1,
    PositiveY = 2,
    NegativeY = 3,
    PositiveZ = 4,
    NegativeZ = 5,
}

impl Face {
    /// Every face, in layer order.
    pub(crate) const ALL: [Self; FACES] = [
        Self::PositiveX,
        Self::NegativeX,
        Self::PositiveY,
        Self::NegativeY,
        Self::PositiveZ,
        Self::NegativeZ,
    ];

    /// Which array layer this face is.
    pub(crate) const fn layer(self) -> usize {
        self as usize
    }
}

/// The direction a point on `face` looks along, for `u` and `v` in −1..1.
///
/// Not normalised — the caller normalises when it needs a unit vector, and the
/// unnormalised form is what the solid-angle arithmetic wants. `u` runs along the
/// face's `s` axis and `v` along its `t` axis, with `v = -1` at the **top** row,
/// matching how an image is stored.
#[must_use]
pub(crate) fn direction_of(face: Face, u: f32, v: f32) -> Vec3 {
    match face {
        Face::PositiveX => Vec3::new(1.0, -v, -u),
        Face::NegativeX => Vec3::new(-1.0, -v, u),
        Face::PositiveY => Vec3::new(u, 1.0, v),
        Face::NegativeY => Vec3::new(u, -1.0, -v),
        Face::PositiveZ => Vec3::new(u, -v, 1.0),
        Face::NegativeZ => Vec3::new(-u, -v, -1.0),
    }
}

/// Which face `direction` lands on, and where.
///
/// The inverse of [`direction_of`], returning `(face, u, v)` with `u` and `v` in
/// −1..1. A direction along a face's edge belongs to whichever face the tie-break
/// below picks; that is arbitrary and harmless, since both give the same
/// direction back.
///
/// **Test-only for now**, and that is the honest state rather than a permanent
/// one: nothing in the cooker yet needs to go from a direction to a texel, and
/// this exists because a mapping with only one direction implemented cannot be
/// checked against anything. E6c's prefilter is the first caller — it samples the
/// source cube along a reflection vector — and the gate comes off with it.
#[cfg(test)]
#[must_use]
pub(crate) fn face_of(direction: Vec3) -> (Face, f32, f32) {
    let absolute = direction.abs();

    // The major axis: whichever component is largest is the face the direction
    // pierces, and dividing by it is the projection onto that face's plane.
    let (face, major) = if absolute.x >= absolute.y && absolute.x >= absolute.z {
        (
            if direction.x > 0.0 {
                Face::PositiveX
            } else {
                Face::NegativeX
            },
            absolute.x,
        )
    } else if absolute.y >= absolute.z {
        (
            if direction.y > 0.0 {
                Face::PositiveY
            } else {
                Face::NegativeY
            },
            absolute.y,
        )
    } else {
        (
            if direction.z > 0.0 {
                Face::PositiveZ
            } else {
                Face::NegativeZ
            },
            absolute.z,
        )
    };

    let scale = 1.0 / major;

    let (u, v) = match face {
        Face::PositiveX => (-direction.z * scale, -direction.y * scale),
        Face::NegativeX => (direction.z * scale, -direction.y * scale),
        Face::PositiveY => (direction.x * scale, direction.z * scale),
        Face::NegativeY => (direction.x * scale, -direction.z * scale),
        Face::PositiveZ => (direction.x * scale, -direction.y * scale),
        Face::NegativeZ => (-direction.x * scale, -direction.y * scale),
    };

    (face, u, v)
}

/// A cube of radiance at one resolution.
///
/// Six square faces of linear RGB, in layer order. One level — a chain is a
/// `Vec<Cube>`, built by [`halved`](Cube::halved), because the levels have
/// different sizes and a single flat buffer would need the offset arithmetic that
/// `slop_asset::environment` already owns for the artifact.
pub(crate) struct Cube {
    /// Texels along each edge of a face.
    pub size: u32,
    /// Six faces, each `size * size` texels, row-major from the top.
    pub faces: [Vec<Vec3>; FACES],
}

impl std::fmt::Debug for Cube {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cube")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Cube {
    /// The direction the texel at `(x, y)` of `face` looks along, normalised.
    ///
    /// Texel **centres**, at `(i + 0.5) / size`. Sampling at texel corners
    /// instead shifts the whole environment by half a texel and squashes it by a
    /// texel overall, which is invisible on a sky and wrong everywhere it is
    /// compared against something.
    #[must_use]
    pub(crate) fn texel_direction(size: u32, face: Face, x: u32, y: u32) -> Vec3 {
        let step = 2.0 / size as f32;

        let u = (x as f32 + 0.5).mul_add(step, -1.0);
        let v = (y as f32 + 0.5).mul_add(step, -1.0);

        direction_of(face, u, v).normalize()
    }

    /// How much sky the texel at `(x, y)` covers, in steradians.
    ///
    /// **Not `4π / (6 · size²)`.** A cube's texels are equal in area on the cube
    /// and very unequal on the sphere: a corner texel of a face subtends roughly
    /// a fifth of what a centre texel does, because it is further from the centre
    /// and more steeply inclined. Weighting an integral by texel count instead of
    /// solid angle overweights the eight corners of the cube by a factor of five,
    /// which tilts the reconstructed lighting towards whatever happens to be
    /// diagonal from the origin.
    ///
    /// The closed form is the spherical excess of the texel's rectangle, which is
    /// exact rather than a small-angle approximation — and exactness is what
    /// makes `the_solid_angles_of_a_cube_sum_to_the_whole_sphere` a real check
    /// rather than a tolerance chosen to pass.
    #[must_use]
    pub(crate) fn texel_solid_angle(size: u32, x: u32, y: u32) -> f32 {
        // The area of the spherical rectangle from the face centre out to
        // `(u, v)`, which the four corners below combine by inclusion-exclusion.
        fn corner(u: f32, v: f32) -> f32 {
            scalar::atan2(u * v, u.mul_add(u, v.mul_add(v, 1.0)).sqrt())
        }

        let step = 2.0 / size as f32;

        let u0 = (x as f32).mul_add(step, -1.0);
        let u1 = u0 + step;
        let v0 = (y as f32).mul_add(step, -1.0);
        let v1 = v0 + step;

        corner(u0, v0) - corner(u0, v1) - corner(u1, v0) + corner(u1, v1)
    }

    /// Project this cube onto spherical harmonics.
    ///
    /// The diffuse half of image-based lighting: nine coefficients that replace
    /// `docs/PLAN.md` §6.1's flat ambient term. Every texel contributes weighted
    /// by the solid angle it covers, which is the whole reason the projection
    /// happens on a cube rather than on the source panorama — see this module's
    /// documentation.
    #[must_use]
    pub(crate) fn harmonics(&self) -> Sh9 {
        let mut sh = Sh9::ZERO;

        for face in Face::ALL {
            let texels = &self.faces[face.layer()];

            for y in 0..self.size {
                for x in 0..self.size {
                    let radiance = texels[(y * self.size + x) as usize];
                    let direction = Self::texel_direction(self.size, face, x, y);

                    sh.accumulate(
                        direction,
                        radiance,
                        Self::texel_solid_angle(self.size, x, y),
                    );
                }
            }
        }

        sh
    }

    /// Project a panorama onto a cube of `size` texels per edge.
    #[must_use]
    pub(crate) fn from_panorama(panorama: &Panorama, size: u32) -> Self {
        let faces = Face::ALL.map(|face| {
            let mut texels = Vec::with_capacity(size as usize * size as usize);

            for y in 0..size {
                for x in 0..size {
                    texels.push(panorama.sample(Self::texel_direction(size, face, x, y)));
                }
            }

            texels
        });

        Self { size, faces }
    }

    /// This cube at half the resolution, by a 2×2 box filter within each face.
    ///
    /// **Within** each face: the four texels averaged for an edge texel are the
    /// four that exist, and the neighbouring face's texels are not consulted. The
    /// error that introduces is confined to one texel at each face boundary and
    /// shrinks with the level, which is what every offline pipeline accepts here.
    /// Doing it properly means resampling across the seam, which is a different
    /// and much larger piece of work than mip generation.
    ///
    /// # Panics
    ///
    /// If the cube is already 1×1, which has no half. Callers walk a chain whose
    /// length they computed, so reaching this is a programming error.
    #[must_use]
    pub(crate) fn halved(&self) -> Self {
        assert!(self.size > 1, "a 1x1 cube face cannot be halved");

        let size = self.size / 2;
        let source = self.size as usize;

        let faces = std::array::from_fn(|layer| {
            let from = &self.faces[layer];
            let mut texels = Vec::with_capacity(size as usize * size as usize);

            for y in 0..size as usize {
                for x in 0..size as usize {
                    let sum = from[y * 2 * source + x * 2]
                        + from[y * 2 * source + x * 2 + 1]
                        + from[(y * 2 + 1) * source + x * 2]
                        + from[(y * 2 + 1) * source + x * 2 + 1];

                    texels.push(sum * 0.25);
                }
            }

            texels
        });

        Self { size, faces }
    }

    /// Append this cube's six faces to `into`, as `Rgba16Float` texels.
    ///
    /// The payload half of `slop_asset::environment`'s layout: faces in layer
    /// order, row-major within a face, which is what one copy region per face
    /// expects. Alpha is one — the channel exists because `R16G16B16_SFLOAT` is
    /// in the specification and almost nowhere in hardware, not because anything
    /// reads it.
    pub(crate) fn encode(&self, into: &mut Vec<u8>) {
        // Driven by `Face::ALL` rather than by iterating the array, so the order
        // the bytes are written in is the layer order by construction instead of
        // by the two happening to agree.
        for face in Face::ALL {
            for texel in &self.faces[face.layer()] {
                for channel in [texel.x, texel.y, texel.z, 1.0] {
                    into.extend_from_slice(&to_half(channel).to_le_bytes());
                }
            }
        }
    }

    /// This cube and every halving of it, largest first, down to 1×1.
    ///
    /// The chain is not decoration and not only for sampling. §9.7's first named
    /// trap is that importance-sampling a bright sun at a few hundred samples per
    /// texel produces fireflies; the standard answer is for a wide sample cone to
    /// read an already-filtered level, and this is the chain it reads.
    #[must_use]
    pub(crate) fn chain(self) -> Vec<Self> {
        let mut levels = vec![self];

        while levels
            .last()
            .expect("the chain always has its base level")
            .size
            > 1
        {
            let next = levels
                .last()
                .expect("the chain always has its base level")
                .halved();

            levels.push(next);
        }

        levels
    }
}

/// A finite `f32` as an IEEE-754 half.
///
/// Written rather than taken, for the same reason the Radiance decoder is: it is
/// forty lines of bit manipulation with an exactly-specified answer, and the
/// alternative is a dependency in the crate whose whole purpose is that nothing
/// linking it ships.
///
/// Rounding is to nearest, ties away from zero, rather than the ties-to-even the
/// hardware would do. The difference is one unit in the last place on exactly
/// representable midpoints, which for a radiance value is nothing — and stating
/// it is cheaper than implying an exactness this does not have.
fn to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let mantissa = bits & 0x007f_ffff;
    let exponent = ((bits >> 23) & 0xff) as i32;

    // Infinities and NaNs keep their identity. A NaN reaching here is a bug
    // upstream, but turning it into a large finite number would hide that bug
    // inside an environment map, which is the worst place to look for one.
    if exponent == 0xff {
        return sign | 0x7c00 | u16::from(mantissa != 0) << 9;
    }

    // f32 biases by 127 and f16 by 15.
    let exponent = exponent - 127 + 15;

    if exponent >= 0x1f {
        // Past what a half can name. Infinity rather than the largest finite
        // value: a clamp would make an overflowing environment merely very
        // bright, which is indistinguishable from one that is very bright.
        return sign | 0x7c00;
    }

    if exponent <= 0 {
        // Subnormal, or below even that. The implicit leading one has to be put
        // back before shifting, because a subnormal half has none.
        if exponent < -10 {
            return sign;
        }

        let mantissa = mantissa | 0x0080_0000;
        let shift = (14 - exponent) as u32;
        let rounded = (mantissa >> shift) + ((mantissa >> (shift - 1)) & 1);

        return sign | rounded as u16;
    }

    // Rounding can carry into the exponent — 0x3ff + 1 becomes 0x400 — and
    // adding rather than or-ing is what lets it, which is the correct result.
    let rounded = (mantissa >> 13) + ((mantissa >> 12) & 1);

    sign | (((exponent as u32) << 10) + rounded) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_face_survives_the_round_trip() {
        // The table's own consistency, in both directions. A swapped axis or a
        // wrong sign on any of the six passes every other test in this file and
        // fails this one — and in a rendered image it looks like a rotated
        // environment, which is not distinguishable from a differently-shot one.
        for face in Face::ALL {
            for u in [-0.9_f32, -0.3, 0.0, 0.45, 0.9] {
                for v in [-0.9_f32, -0.3, 0.0, 0.45, 0.9] {
                    let direction = direction_of(face, u, v);
                    let (back, back_u, back_v) = face_of(direction);

                    assert_eq!(back, face, "{face:?} at ({u}, {v}) landed on {back:?}");
                    assert!(
                        (back_u - u).abs() < 1e-5 && (back_v - v).abs() < 1e-5,
                        "{face:?} at ({u}, {v}) came back as ({back_u}, {back_v})"
                    );
                }
            }
        }
    }

    #[test]
    fn each_face_centre_looks_along_its_own_axis() {
        // What the names mean, stated rather than implied by the table. The
        // centre of the `+X` face must look along `+X`; if it does not, the
        // layer order and the axis order disagree and the upload puts every face
        // in the wrong slot.
        let expected = [
            (Face::PositiveX, Vec3::X),
            (Face::NegativeX, Vec3::NEG_X),
            (Face::PositiveY, Vec3::Y),
            (Face::NegativeY, Vec3::NEG_Y),
            (Face::PositiveZ, Vec3::Z),
            (Face::NegativeZ, Vec3::NEG_Z),
        ];

        for (face, axis) in expected {
            assert_eq!(direction_of(face, 0.0, 0.0), axis, "{face:?}");
        }
    }

    #[test]
    fn the_layer_order_is_the_one_vulkan_fixes() {
        // Not a choice. A cube view's layers are +X, -X, +Y, -Y, +Z, -Z, and
        // reordering them here would make the CPU and the hardware sampler
        // disagree about which texel a direction reads — which cannot be
        // debugged from an image.
        for (index, face) in Face::ALL.into_iter().enumerate() {
            assert_eq!(face.layer(), index);
        }
    }

    #[test]
    fn a_texel_direction_is_a_unit_vector_at_the_texel_centre() {
        let direction = Cube::texel_direction(4, Face::PositiveZ, 0, 0);

        assert!((direction.length() - 1.0).abs() < 1e-6);

        // The first texel's centre is at s = 0.125, so u = -0.75, and on +Z that
        // is x = -0.75 before normalising. A corner-sampled implementation would
        // give -1.0 here.
        let (face, u, v) = face_of(direction);
        assert_eq!(face, Face::PositiveZ);
        assert!((u + 0.75).abs() < 1e-5, "u = {u}");
        assert!((v + 0.75).abs() < 1e-5, "v = {v}");
    }

    /// A panorama whose radiance is a function of direction, so a projection
    /// that reads the wrong place gives the wrong answer rather than the same one.
    fn directional(width: u32, height: u32) -> Panorama {
        let mut texels = Vec::with_capacity(width as usize * height as usize);

        for y in 0..height {
            for x in 0..width {
                let u = (x as f32 + 0.5) / width as f32;
                let v = (y as f32 + 0.5) / height as f32;
                let direction = Panorama::direction_at(u, v);

                // Shifted into the positive range so a sign error is visible as
                // a value rather than cancelling out.
                texels.push(direction * 0.5 + Vec3::splat(0.5));
            }
        }

        Panorama {
            width,
            height,
            texels,
        }
    }

    #[test]
    fn projecting_a_panorama_puts_each_direction_on_the_right_face() {
        // The end-to-end check on the two mappings agreeing: the panorama
        // encodes its own direction, so every cube texel must read back the
        // direction that texel looks along. A mismatch between the panorama's
        // convention and the cube's shows here and nowhere else.
        let panorama = directional(256, 128);
        let cube = Cube::from_panorama(&panorama, 8);

        for face in Face::ALL {
            for y in 0..8 {
                for x in 0..8 {
                    let expected = Cube::texel_direction(8, face, x, y);
                    let read = cube.faces[face.layer()][(y * 8 + x) as usize];
                    let decoded = (read - Vec3::splat(0.5)) * 2.0;

                    assert!(
                        (decoded - expected).length() < 0.05,
                        "{face:?} texel ({x}, {y}) looks along {expected:?} but read {decoded:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_solid_angles_of_a_cube_sum_to_the_whole_sphere() {
        // The one check that says the weights are right rather than merely
        // plausible. Six faces of texels must cover 4π steradians exactly, and
        // any weighting that is uniform-per-texel, off by a factor, or using the
        // wrong face parameterisation misses it.
        for size in [1, 2, 8, 32] {
            let per_face: f32 = (0..size)
                .flat_map(|y| (0..size).map(move |x| (x, y)))
                .map(|(x, y)| Cube::texel_solid_angle(size, x, y))
                .sum();

            let whole = per_face * FACES as f32;

            assert!(
                (whole - 4.0 * std::f32::consts::PI).abs() < 1e-4,
                "a {size}x{size} cube covers {whole} steradians, not 4π"
            );
        }
    }

    #[test]
    fn a_corner_texel_covers_far_less_sky_than_a_centre_one() {
        // Why the solid angle is computed at all. If texels were equal on the
        // sphere this would be one, and the projection could weight by count —
        // the shortcut that overweights the cube's eight corners.
        let size = 32;

        let centre = Cube::texel_solid_angle(size, size / 2, size / 2);
        let corner = Cube::texel_solid_angle(size, 0, 0);

        assert!(
            centre > corner * 4.0,
            "a centre texel covers {centre} and a corner {corner} — too close for \
             the weighting to matter, which means one of them is wrong"
        );
    }

    #[test]
    fn a_constant_environment_projects_to_a_constant_reconstruction() {
        // End to end through the projection: a uniform sky must light every
        // direction by exactly its own radiance. This is `Sh9`'s own property
        // driven through the cube's solid angles, so it fails if the weights do
        // not sum to 4π even when each one is individually plausible.
        let radiance = Vec3::new(0.25, 0.5, 0.75);
        let panorama = Panorama {
            width: 16,
            height: 8,
            texels: vec![radiance; 128],
        };

        let sh = Cube::from_panorama(&panorama, 16).harmonics();

        for direction in [
            Vec3::X,
            Vec3::NEG_Y,
            Vec3::Z,
            Vec3::new(0.4, 0.5, -0.7).normalize(),
        ] {
            let reconstructed = sh.diffuse(direction);

            assert!(
                (reconstructed - radiance).length() < 1e-3,
                "{direction:?} reconstructed to {reconstructed:?}, not {radiance:?}"
            );
        }
    }

    #[test]
    fn a_sky_brighter_above_reconstructs_brighter_facing_up() {
        // Direction, which a constant sky cannot check. A projection that had
        // the cube's vertical axis inverted — or the basis's — would light every
        // surface from below, and that is entirely plausible to look at.
        let mut panorama = Panorama {
            width: 32,
            height: 16,
            texels: vec![Vec3::splat(0.1); 512],
        };

        for y in 0..8 {
            for x in 0..32 {
                panorama.texels[y * 32 + x] = Vec3::ONE;
            }
        }

        let sh = Cube::from_panorama(&panorama, 16).harmonics();

        assert!(
            sh.diffuse(Vec3::Y).x > sh.diffuse(Vec3::NEG_Y).x * 2.0,
            "up reads {:?} and down {:?}",
            sh.diffuse(Vec3::Y),
            sh.diffuse(Vec3::NEG_Y)
        );
    }

    #[test]
    fn a_constant_environment_stays_constant_through_the_whole_chain() {
        // The property a box filter must have and the one a weighting mistake
        // breaks: averaging four equal values cannot produce a different value.
        // A chain that drifts here would make every roughness level of the
        // prefilter drift with it.
        let panorama = Panorama {
            width: 16,
            height: 8,
            texels: vec![Vec3::new(0.25, 0.5, 0.75); 128],
        };

        for level in Cube::from_panorama(&panorama, 8).chain() {
            for face in &level.faces {
                for texel in face {
                    assert!(
                        (*texel - Vec3::new(0.25, 0.5, 0.75)).length() < 1e-5,
                        "level {} drifted to {texel:?}",
                        level.size
                    );
                }
            }
        }
    }

    #[test]
    fn the_chain_halves_down_to_one_by_one() {
        let chain = Cube::from_panorama(
            &Panorama {
                width: 4,
                height: 2,
                texels: vec![Vec3::ONE; 8],
            },
            8,
        )
        .chain();

        let sizes: Vec<u32> = chain.iter().map(|level| level.size).collect();

        assert_eq!(sizes, vec![8, 4, 2, 1]);

        for level in &chain {
            for face in &level.faces {
                assert_eq!(face.len(), (level.size * level.size) as usize);
            }
        }
    }

    #[test]
    fn known_values_encode_to_the_halves_they_should() {
        // Against the specification's own numbers rather than against a second
        // implementation. Each of these exercises a different branch: an exact
        // power of two, a value needing mantissa bits, the largest finite half,
        // an overflow, a subnormal, and the signs.
        for (value, expected) in [
            (0.0_f32, 0x0000_u16),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (-1.0, 0xbc00),
            (0.5, 0x3800),
            (2.0, 0x4000),
            (65504.0, 0x7bff),
            // Past the largest half, so infinity.
            (131_008.0, 0x7c00),
            // The smallest normal half, and half of it, which is subnormal.
            (6.103_515_6e-5, 0x0400),
            (3.051_757_8e-5, 0x0200),
            // Far below even a subnormal.
            (1.0e-20, 0x0000),
        ] {
            assert_eq!(
                to_half(value),
                expected,
                "{value} encoded to {:#06x}, expected {expected:#06x}",
                to_half(value)
            );
        }
    }

    #[test]
    fn a_not_a_number_stays_one_rather_than_becoming_bright() {
        // A NaN reaching the encoder is a bug upstream. Turning it into a large
        // finite value would hide that bug inside an environment map, which is
        // the single worst place to go looking for one.
        let encoded = to_half(f32::NAN);

        assert_eq!(encoded & 0x7c00, 0x7c00, "the exponent must be all ones");
        assert_ne!(encoded & 0x03ff, 0, "the mantissa must be non-zero");
    }

    #[test]
    fn an_encoded_cube_is_six_faces_of_rgba_halves() {
        // The size arithmetic the artifact's header promises, checked against
        // what the encoder actually appends. A mismatch here writes a payload
        // the reader will call truncated — or worse, will not.
        let cube = Cube {
            size: 2,
            faces: std::array::from_fn(|_| vec![Vec3::ONE; 4]),
        };

        let mut encoded = Vec::new();
        cube.encode(&mut encoded);

        assert_eq!(
            encoded.len(),
            FACES * 4 * 4 * 2,
            "6 faces, 4 texels, RGBA16"
        );

        // The first texel is white with an opaque alpha, in little-endian
        // halves. Reading this as anything else means the channel order is
        // wrong, which renders as a colour-swapped sky.
        assert_eq!(
            &encoded[..8],
            &[0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c]
        );
    }

    #[test]
    fn halving_averages_the_four_texels_it_covers() {
        // Asserted on values rather than on sizes: a filter that read the wrong
        // four texels still halves the image correctly.
        let size = 2;
        let faces = std::array::from_fn(|_| {
            vec![
                Vec3::splat(0.0),
                Vec3::splat(1.0),
                Vec3::splat(2.0),
                Vec3::splat(5.0),
            ]
        });

        let halved = Cube { size, faces }.halved();

        assert_eq!(halved.size, 1);
        assert_eq!(halved.faces[0][0], Vec3::splat(2.0));
    }
}
