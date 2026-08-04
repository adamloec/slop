//! The specular half of an environment: the sky, blurred by roughness.
//!
//! `docs/PLAN.md` §9.7 E6c. The counterpart of the nine spherical-harmonic
//! coefficients E6b produced — those hold the diffuse term, which is the
//! environment convolved with a very wide cosine lobe and therefore has no
//! detail left to store. A reflection does. A mirror shows the sky exactly, a
//! rough surface shows it smeared, and the width of that smear is what roughness
//! means.
//!
//! # The split-sum approximation, and which half this is
//!
//! Integrating the full specular response per fragment is not affordable, so it
//! is split into two factors that are each precomputed: the environment
//! prefiltered against the reflection lobe, and a scalar that depends only on
//! roughness and viewing angle. This module is the first. The second is the
//! analytic fit in the shader — §9.7 argues that against a lookup table, and the
//! short version is that it depends on the BRDF rather than on the environment,
//! so cooking it per-asset would be wrong.
//!
//! The approximation the split makes is assuming the view direction equals the
//! normal, which is what lets one lookup serve every viewing angle. It costs
//! stretched reflections at grazing angles — the well-known artefact, accepted by
//! every engine that ships this — and buys a chain of images indexed by one
//! number.
//!
//! # A level is a roughness, not a resolution
//!
//! From here the chain stops being mips of one image. Level zero is the
//! environment untouched, which is roughness zero and also what a skybox draws;
//! each level below it is the same sky convolved with a wider lobe, and its lower
//! resolution is a consequence rather than the point — a blurred image has no
//! detail worth storing at full size. `slop_asset::environment` records this,
//! because a consumer assuming "mip level" meant "smaller copy" would be wrong in
//! a way that renders plausibly.
//!
//! # Why sampling reads the source's own chain
//!
//! §9.7's first named trap. A few hundred samples cannot resolve a sun that
//! occupies a handful of texels: most samples miss it entirely and a few land on
//! it, so neighbouring output texels differ enormously and the result is speckled
//! with fireflies. It looks like a compression artefact rather than an
//! undersampled integral.
//!
//! The fix is Karis's: compare the solid angle one sample is responsible for
//! against the solid angle of a source texel, and read a level of the source
//! whose texels are about that big. A wide sample cone then reads an
//! already-averaged level instead of gambling on one texel. That is why
//! [`Cube::chain`](crate::cube::Cube::chain) is built before this runs, and why
//! it is a prerequisite rather than an optimisation.

use slop_core::JobSystem;
use slop_math::{Vec3, scalar};

use crate::cube::{Cube, FACES, Face};

/// How many directions each output texel integrates over.
///
/// The number everyone converges on. Fewer speckles even with the mip trick
/// above; many more costs cook time and buys nothing visible, because the
/// remaining error is dominated by the source's own resolution rather than by
/// the sample count.
const SAMPLES: u32 = 128;

/// The roughness a chain of `levels` assigns to level `index`.
///
/// Linear from zero at the sharpest level to one at the smallest. Linear in
/// **roughness**, not in the lobe's width — which is the convention the shader
/// has to match when it turns a material's roughness into a level, and the reason
/// that mapping is one expression stated in one place rather than two that agree
/// by inspection.
#[must_use]
pub(crate) fn roughness_of(index: u32, levels: u32) -> f32 {
    if levels <= 1 {
        return 0.0;
    }

    index as f32 / (levels - 1) as f32
}

/// Prefilter `source` into one chain level per roughness.
///
/// `source` is the environment's own mip chain, largest first, as
/// [`Cube::chain`] produces it. The result has the same shape and the same
/// sizes — level zero is the source untouched, because roughness zero is a
/// mirror and convolving with a delta would only lose precision to the sampling.
#[must_use]
pub(crate) fn prefilter(jobs: &JobSystem, source: Vec<Cube>) -> Vec<Cube> {
    let levels = u32::try_from(source.len()).expect("a chain is far shorter than u32::MAX");

    let mut chain = Vec::with_capacity(source.len());

    for index in 0..levels {
        if index == 0 {
            // Roughness zero. Taken rather than integrated: the lobe is a delta,
            // so every sample would return the same direction and the only
            // effect of running the integral would be to blur the sky by the
            // width of one bilinear tap.
            chain.push(Cube {
                size: source[0].size,
                faces: source[0].faces.clone(),
            });
            continue;
        }

        chain.push(level(
            jobs,
            &source,
            source[index as usize].size,
            roughness_of(index, levels),
        ));
    }

    chain
}

/// One prefiltered level, at `size` texels per edge.
///
/// **Partitioned by row, not by face.** Six faces would cap the speedup at six
/// however many cores there are, and would leave five idle at the small levels;
/// a row is the largest unit that still gives every worker something to take.
///
/// Each row writes only its own slice and reads only the immutable source, so
/// this is `docs/CONVENTIONS.md` §9's partitioning rather than locking — and the
/// result cannot depend on how the work was divided or on which worker finished
/// first, which `docs/DESIGN.md` §2.14 requires of a cooked artifact.
fn level(jobs: &JobSystem, source: &[Cube], size: u32, roughness: f32) -> Cube {
    let width = size as usize;

    let faces = std::array::from_fn(|layer| {
        let face = Face::ALL[layer];
        let mut rows = vec![Vec3::ZERO; width * width];

        jobs.for_each_indexed(
            rows.as_mut_slice()
                .chunks_mut(width)
                .collect::<Vec<_>>()
                .as_mut_slice(),
            |y, row| {
                for (x, texel) in row.iter_mut().enumerate() {
                    *texel = convolve(
                        source,
                        Cube::texel_direction(size, face, x as u32, y as u32),
                        roughness,
                    );
                }
            },
        );

        rows
    });

    Cube { size, faces }
}

/// The reflected radiance a surface of `roughness` facing `normal` gathers.
///
/// **Normal, view and reflection are all assumed equal** — the split-sum
/// simplification described in this module's docs. That is what makes the answer
/// a function of one direction and therefore storable in a cube.
fn convolve(source: &[Cube], normal: Vec3, roughness: f32) -> Vec3 {
    let base = source[0].size;

    // What one texel of the source covers, on average. Not the exact solid angle
    // of a particular texel — this is choosing a mip level, so the average is
    // what the comparison wants, and using the exact value would make the level
    // vary across a face for no benefit.
    let texel_solid_angle = 4.0 * std::f32::consts::PI / (FACES as f32 * (base * base) as f32);

    let mut total = Vec3::ZERO;
    let mut weight = 0.0;

    for index in 0..SAMPLES {
        let (u, v) = hammersley(index, SAMPLES);
        let half = importance_sample_ggx(u, v, roughness, normal);

        // Reflect the view — which is the normal, by the assumption above —
        // about the sampled half vector.
        let light = (2.0 * normal.dot(half) * half - normal).normalize_or_zero();

        let cosine = normal.dot(light);
        if cosine <= 0.0 {
            // Below the horizon. Discarded rather than clamped, and the weight
            // is not accumulated either — including it would darken every
            // rough surface by the fraction of the lobe pointing into the
            // surface.
            continue;
        }

        // Karis's mip selection. `pdf` is the density this sample was drawn
        // with, so `1 / (samples · pdf)` is the solid angle it is responsible
        // for; the level whose texels are about that size is the one to read.
        let half_cosine = normal.dot(half).max(0.0);
        let density =
            distribution_ggx(half_cosine, roughness) * half_cosine / (4.0 * half_cosine).max(1e-4);
        let sample_solid_angle = 1.0 / (SAMPLES as f32 * density).max(1e-4);

        let level = if roughness == 0.0 {
            0.0
        } else {
            0.5 * scalar::log2(sample_solid_angle / texel_solid_angle)
        };

        total += sample_chain(source, light, level) * cosine;
        weight += cosine;
    }

    if weight <= 0.0 {
        // Unreachable in practice — the lobe is centred on the normal, so at
        // least one sample is above the horizon — but returning the unfiltered
        // sky is a better failure than a division by zero.
        return source[0].sample(normal);
    }

    total / weight
}

/// Sample the chain at a fractional level, trilinearly.
///
/// Linear between the two neighbouring levels, which is what makes the
/// firefly suppression smooth: a hard level change would show as a visible ring
/// wherever the sample cone crossed the boundary.
fn sample_chain(chain: &[Cube], direction: Vec3, level: f32) -> Vec3 {
    let last = chain.len() - 1;
    let level = level.clamp(0.0, last as f32);

    let lower = level.floor() as usize;
    let upper = (lower + 1).min(last);
    let blend = level - lower as f32;

    chain[lower]
        .sample(direction)
        .lerp(chain[upper].sample(direction), blend)
}

/// The GGX normal distribution, as a density over the half vector.
fn distribution_ggx(half_cosine: f32, roughness: f32) -> f32 {
    // Squared, which is the convention every real-time GGX implementation uses:
    // perceptual roughness is what an artist sets, and its square is what the
    // distribution wants. Skipping the square makes low roughness look far
    // blurrier than the material says.
    let alpha = roughness * roughness;
    let alpha_squared = alpha * alpha;

    let denominator = half_cosine.mul_add(half_cosine * (alpha_squared - 1.0), 1.0);

    alpha_squared / (std::f32::consts::PI * denominator * denominator).max(1e-7)
}

/// A half vector drawn from the GGX distribution around `normal`.
fn importance_sample_ggx(u: f32, v: f32, roughness: f32, normal: Vec3) -> Vec3 {
    let alpha = roughness * roughness;

    let phi = 2.0 * std::f32::consts::PI * u;
    // Inverting the GGX distribution's CDF, which is what makes this importance
    // sampling rather than sampling the hemisphere and weighting afterwards: the
    // samples land where the lobe has energy, so a hundred of them do the work
    // of many thousand uniform ones.
    let cos_theta = ((1.0 - v) / v.mul_add(alpha.mul_add(alpha, -1.0), 1.0))
        .max(0.0)
        .sqrt();
    let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();

    let (sin_phi, cos_phi) = scalar::sin_cos(phi);

    // Into world space, around the normal. Any tangent perpendicular to it does
    // — the distribution is rotationally symmetric about the normal — but it
    // must not be parallel to it, which is what the choice below avoids.
    let up = if normal.y.abs() < 0.999 {
        Vec3::Y
    } else {
        Vec3::X
    };
    let tangent = up.cross(normal).normalize_or_zero();
    let bitangent = normal.cross(tangent);

    (tangent * (sin_theta * cos_phi) + bitangent * (sin_theta * sin_phi) + normal * cos_theta)
        .normalize_or_zero()
}

/// The `index`th point of the Hammersley sequence over `count` points.
///
/// **Deterministic, and that is required rather than convenient.**
/// `docs/DESIGN.md` §2.14 makes reproducibility a property of the build, and a
/// cooked artifact produced from an RNG would differ between two machines
/// cooking the same source. A low-discrepancy sequence is also simply better
/// here: it covers the lobe more evenly than random points, so it converges
/// faster for the same sample count.
fn hammersley(index: u32, count: u32) -> (f32, f32) {
    // The radical inverse in base two: the index's bits, reversed, read as a
    // fraction. That is what spreads consecutive samples apart instead of
    // clustering them.
    let reversed = index.reverse_bits();

    (
        index as f32 / count as f32,
        reversed as f32 * 2.328_306_4e-10,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panorama::Panorama;

    /// A chain built from a uniform sky.
    fn flat_chain(size: u32, radiance: Vec3) -> Vec<Cube> {
        let panorama = Panorama {
            width: 16,
            height: 8,
            texels: vec![radiance; 128],
        };

        Cube::from_panorama(&panorama, size).chain()
    }

    #[test]
    fn a_uniform_sky_survives_every_roughness() {
        // **The test this module exists to pass.** Convolving a constant with
        // anything normalised gives the constant back, so every level of a
        // uniform sky must be that same value. It catches a weight that does not
        // sum to one, a lobe that leaks below the horizon, and a mip selection
        // that reads off the end of the chain — three independent mistakes, each
        // of which renders as an environment that is merely a bit too dark.
        let radiance = Vec3::new(0.25, 0.5, 0.75);
        let chain = prefilter(&JobSystem::new(), flat_chain(16, radiance));

        for (index, level) in chain.iter().enumerate() {
            for face in &level.faces {
                for texel in face {
                    assert!(
                        (*texel - radiance).length() < 1e-3,
                        "level {index} drifted to {texel:?} from {radiance:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn level_zero_is_the_sky_itself() {
        // Roughness zero is a mirror. Taken rather than integrated, so this is
        // exact rather than close — and a skybox drawing from level zero shows
        // the environment rather than a slightly blurred copy of it.
        let source = flat_chain(8, Vec3::ONE);
        let mut marked = source;
        marked[0].faces[0][0] = Vec3::new(9.0, 9.0, 9.0);

        let chain = prefilter(&JobSystem::new(), marked);

        assert_eq!(chain[0].faces[0][0], Vec3::new(9.0, 9.0, 9.0));
    }

    #[test]
    fn roughness_runs_from_zero_to_one_across_the_chain() {
        // The mapping the shader has to match. Off by one at either end and a
        // fully rough material reads a level that is not the roughest, which
        // shows as reflections that never quite blur out.
        assert_eq!(roughness_of(0, 5), 0.0);
        assert_eq!(roughness_of(4, 5), 1.0);
        assert!((roughness_of(2, 5) - 0.5).abs() < 1e-6);

        // A chain of one level is a mirror and nothing else, rather than a
        // division by zero.
        assert_eq!(roughness_of(0, 1), 0.0);
    }

    /// A sky that is bright in one direction and dark elsewhere.
    ///
    /// The patch sits on the **equator** at the middle column, which is
    /// `slop_math::FORWARD` — see `Panorama::direction_at`. Deliberately not at
    /// the pole: a few rows below the top is not the same direction as straight
    /// up, and a test that assumed it was would fail for a reason that has
    /// nothing to do with the prefilter.
    fn spotlit(size: u32) -> Vec<Cube> {
        let mut panorama = Panorama {
            width: 64,
            height: 32,
            texels: vec![Vec3::splat(0.02); 64 * 32],
        };

        // Small and very bright, which is the shape that produces fireflies if
        // the sampling is naive.
        for y in 15..18 {
            for x in 30..34 {
                panorama.texels[y * 64 + x] = Vec3::splat(40.0);
            }
        }

        Cube::from_panorama(&panorama, size).chain()
    }

    #[test]
    fn a_rougher_level_is_smoother_than_a_sharper_one() {
        // What prefiltering *is*, as a measurable property rather than a claim.
        // Variance across a face must fall monotonically as roughness rises; a
        // level that does not blur is one whose lobe width is not being applied.
        let chain = prefilter(&JobSystem::new(), spotlit(32));

        let variance = |cube: &Cube| -> f32 {
            // The face the bright patch is on, so there is variation to measure.
            let texels = &cube.faces[Face::NegativeZ.layer()];
            let mean = texels.iter().map(|texel| texel.x).sum::<f32>() / texels.len() as f32;

            texels
                .iter()
                .map(|texel| (texel.x - mean) * (texel.x - mean))
                .sum::<f32>()
                / texels.len() as f32
        };

        let mut previous = variance(&chain[0]);

        for level in &chain[1..4] {
            let current = variance(level);

            assert!(
                current <= previous + 1e-4,
                "a rougher level has more variance ({current}) than a sharper one \
                 ({previous})"
            );

            previous = current;
        }
    }

    #[test]
    fn a_bright_spot_does_not_produce_fireflies() {
        // §9.7's first named trap, as an assertion. Without the mip selection a
        // handful of output texels catch the bright patch and their neighbours
        // do not, so adjacent texels differ by orders of magnitude. Bounding the
        // *ratio* between neighbours is what distinguishes a smooth gradient
        // from a speckled one — the mean is unchanged either way, which is why
        // an energy check would not catch this.
        let chain = prefilter(&JobSystem::new(), spotlit(32));
        let level = &chain[2];
        let size = level.size as usize;

        for face in &level.faces {
            for y in 0..size {
                for x in 1..size {
                    let left = face[y * size + x - 1].x;
                    let right = face[y * size + x].x;

                    let ratio = (left.max(right) + 1e-4) / (left.min(right) + 1e-4);

                    assert!(
                        ratio < 6.0,
                        "neighbouring texels differ by {ratio}x at ({x}, {y}) — \
                         that is a firefly, not a gradient"
                    );
                }
            }
        }
    }

    #[test]
    fn a_reflection_still_points_the_way_it_came_from() {
        // Blurring must not *move* the sky. A prefilter with the lobe built
        // around the wrong axis produces a plausible blurred environment whose
        // reflections point somewhere else, and no energy or smoothness check
        // would notice.
        let chain = prefilter(&JobSystem::new(), spotlit(32));

        // The patch sits on the forward axis, so that must stay the brightest
        // direction at every roughness — blurring widens the highlight without
        // relocating it.
        for level in &chain[..4] {
            let towards = level.sample(slop_math::FORWARD).x;
            let away = level.sample(-slop_math::FORWARD).x;

            assert!(
                towards > away,
                "a sky bright forward reads {towards} forward and {away} behind at \
                 size {}",
                level.size
            );
        }
    }

    #[test]
    fn the_sample_sequence_is_deterministic_and_spread_out() {
        // Deterministic because `docs/DESIGN.md` §2.14 makes a cooked artifact a
        // function of its source, and spread out because that is the only reason
        // to prefer Hammersley over counting.
        assert_eq!(hammersley(7, 64), hammersley(7, 64));

        let points: Vec<f32> = (0..16).map(|index| hammersley(index, 16).1).collect();

        // Consecutive points must not cluster: the radical inverse alternates
        // across the unit interval, so no two in a row are close.
        for pair in points.windows(2) {
            assert!(
                (pair[0] - pair[1]).abs() > 0.1,
                "consecutive samples {pair:?} are clustered"
            );
        }

        assert!(points.iter().all(|point| (0.0..1.0).contains(point)));
    }
}
