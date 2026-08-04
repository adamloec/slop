//! Turning HDR panoramas into cooked environments — `docs/PLAN.md` §9.7 E6a.
//!
//! The fourth asset kind, and the second of the one-source-to-one-artifact shape
//! that [`import::texture`](crate::import::texture) has. What is new is that the
//! artifact is not a reformatting of the source: a panorama is
//! **reparameterised** onto a cube and filtered, so the cooker is doing real work
//! rather than repackaging.
//!
//! # What a level means, and what it will mean
//!
//! Today the chain is an ordinary set of mips — each level the box-filtered
//! halving of the one above. From E6c a level is the environment **prefiltered
//! for a roughness**, and the chain stops being a resolution ladder and becomes a
//! roughness one. The shape does not move, so that is a `COOKER_VERSION` bump and
//! a recook.
//!
//! The chain is not decoration in the meantime. §9.7's first named trap is that
//! importance-sampling a bright sun at a few hundred samples per texel produces
//! fireflies in the prefiltered result; the standard answer is for a wide sample
//! cone to read an already-filtered level, so the prefilter reads exactly this
//! chain. Building it now is the prerequisite rather than a placeholder.
//!
//! # Why the cube is 256 and the panorama is not
//!
//! [`SIZE`] is a resampling decision, not a fidelity one. A 4K panorama carries
//! about a thousand texels across a cube face at the equator and far fewer at the
//! poles, where the equirectangular parameterisation piles texels up on nothing.
//! Keeping the source resolution would keep that distortion; the whole reason to
//! move to a cube is that its texel density is within a factor of about three
//! everywhere, so an integral over it can be weighted correctly.

use std::path::Path;

use anyhow::{Context, Result};
use slop_asset::environment::Environment;
use slop_asset::texture::Format;
use slop_asset::{Cache, CacheKey};
use slop_core::diagnostics::tracing::{debug, info, warn};

use crate::cube::Cube;
use crate::import::Summary;
use crate::panorama::Panorama;
use crate::sources::{self, Sources};

/// Bump to invalidate every cooked environment.
///
/// 1 — the source panorama projected onto a cube, with a mip chain.
const COOKER_VERSION: u32 = 1;

/// Where source panoramas live, relative to the project root.
const SOURCE_DIRECTORY: &str = "assets";

/// What a source panorama looks like.
const PANORAMAS: Sources<'static> = Sources {
    extensions: &["hdr"],
    skip: None,
};

/// Texels along each edge of the cooked cube's largest face.
///
/// 256 is where the skybox and the prefilter meet. Level zero is what a sky is
/// drawn from, so it wants to be sharp; every level below it is a roughness step,
/// and 256 gives nine of them down to 1×1 — more gradations than a material's
/// roughness can usefully name, which is the right side to err on. The whole
/// chain is about 2 MB, so the cost of being generous here is not the reason to
/// pick a number.
const SIZE: u32 = 256;

/// Cook every `.hdr` under `root/assets` into `root/.slop/cache/environments`.
///
/// # Errors
///
/// Fails if a file cannot be read or decoded, or the cache cannot be written.
pub(crate) fn environments(root: &Path, force: bool) -> Result<Summary> {
    let source_root = root.join(SOURCE_DIRECTORY);
    let cache = Cache::for_project(root);

    if !source_root.is_dir() {
        warn!(path = %source_root.display(), "no assets directory; nothing to cook");
        return Ok(Summary::default());
    }

    let mut sources = Vec::new();
    sources::collect(&source_root, &PANORAMAS, &mut sources)?;
    sources.sort();

    let mut summary = Summary::default();

    for source in &sources {
        let relative = source
            .strip_prefix(&source_root)
            .expect("collected paths are under the source root");
        let logical = logical_path(relative);
        let artifact = cache.artifact(&logical);

        let bytes = std::fs::read(source)
            .with_context(|| format!("reading panorama {}", source.display()))?;

        let key = CacheKey::builder()
            .input("cooker", &COOKER_VERSION.to_le_bytes())
            .input("format", &slop_asset::environment::VERSION.to_le_bytes())
            // The cube's edge is an input, not a constant of the cooker: changing
            // it changes every byte of the artifact while the source is
            // untouched, and a key blind to it would keep serving the old size.
            .input("size", &SIZE.to_le_bytes())
            .input("source", &bytes)
            .finish();

        if !force && cache.is_current(&artifact, &key) {
            debug!(logical, "up to date");
            summary.skipped += 1;
            continue;
        }

        let panorama = Panorama::decode_radiance(&bytes)
            .with_context(|| format!("decoding {}", source.display()))?;

        let environment = cook(&panorama);

        cache.prepare(&artifact)?;
        std::fs::write(&artifact, environment.write())
            .with_context(|| format!("writing {}", artifact.display()))?;
        cache.record(&artifact, &key)?;

        info!(
            logical,
            source_width = panorama.width,
            source_height = panorama.height,
            size = environment.size,
            levels = environment.mip_levels,
            bytes = environment.texels.len(),
            "cooked"
        );
        summary.cooked += 1;
    }

    Ok(summary)
}

/// Project a panorama onto a cube and build its chain.
///
/// Split from the walk above so the transformation is testable without a
/// filesystem — which is most of what there is to get wrong here.
fn cook(panorama: &Panorama) -> Environment {
    let levels = Cube::from_panorama(panorama, SIZE).chain();

    let mut texels = Vec::new();
    for level in &levels {
        level.encode(&mut texels);
    }

    Environment {
        size: SIZE,
        mip_levels: u32::try_from(levels.len()).expect("a mip chain is far shorter than u32::MAX"),
        format: Format::Rgba16Float,
        texels,
    }
}

/// Where a cooked environment is addressed from.
fn logical_path(relative: &Path) -> String {
    let cooked = relative.with_extension("env");
    let segments: Vec<String> = cooked
        .components()
        .map(|segment| segment.as_os_str().to_string_lossy().into_owned())
        .collect();

    format!("environments/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_asset::texture::full_mip_chain;
    use slop_math::Vec3;

    /// A panorama of constant radiance, which is the one input whose correct
    /// output is known without reimplementing the cooker to check it.
    fn flat(value: Vec3) -> Panorama {
        Panorama {
            width: 32,
            height: 16,
            texels: vec![value; 32 * 16],
        }
    }

    #[test]
    fn a_cooked_environment_reads_back_as_what_was_written() {
        // The end-to-end property: the cooker's output is a valid artifact of
        // the format the runtime will read. A header that disagrees with the
        // payload it describes is caught here rather than at upload time, where
        // it would be a driver complaint about a copy region.
        let cooked = cook(&flat(Vec3::ONE));
        let decoded = Environment::read(&cooked.write()).expect("the cooker writes valid bytes");

        assert_eq!(decoded, cooked);
        assert_eq!(decoded.size, SIZE);
        assert_eq!(decoded.mip_levels, full_mip_chain(SIZE, SIZE));
    }

    #[test]
    fn the_payload_is_exactly_what_the_header_implies() {
        // The two halves are computed independently — the encoder appends texels
        // and the format walks the chain — so this is a real check rather than a
        // restatement. An off-by-one in either is a payload the reader either
        // refuses or, worse, reads shifted.
        let cooked = cook(&flat(Vec3::ONE));

        assert_eq!(cooked.texels.len(), cooked.payload_bytes());
    }

    #[test]
    fn every_face_of_every_level_is_present() {
        let cooked = cook(&flat(Vec3::splat(0.5)));

        for level in 0..cooked.mip_levels {
            for face in 0..slop_asset::environment::FACES {
                let placed = cooked
                    .face(level, face)
                    .expect("every face of every level exists");

                assert!(
                    placed.offset + placed.bytes <= cooked.texels.len(),
                    "level {level} face {face} runs past the payload"
                );
            }
        }
    }

    #[test]
    fn a_logical_path_lands_under_environments() {
        assert_eq!(
            logical_path(Path::new("vendor/studio.hdr")),
            "environments/vendor/studio.hdr".replace(".hdr", ".env")
        );
    }
}
