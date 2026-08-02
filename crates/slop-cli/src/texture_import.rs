//! Turning PNGs into cooked textures — `docs/DESIGN.md` §2.8.
//!
//! The third asset kind, and the one that puts the `Cooker` question to rest for
//! now: it is one source to one artifact like a shader, not one to many like a
//! glTF. Three kinds, two shapes, and the only thing all three share is the
//! [`Cache`] — which is what was factored out and what all three drive.
//!
//! # Decoded to RGBA8, always
//!
//! A PNG may be greyscale, palettised, sixteen bits per channel, with or without
//! alpha. The GPU wants one layout, so the decode expands everything to eight-bit
//! RGBA rather than carrying the variation forward — a renderer that had to
//! branch on source encoding would be paying for the file format at draw time.
//!
//! Sixteen-bit sources are **narrowed**, which loses precision that a PNG can
//! carry and a `Rgba8` texture cannot. Worth knowing rather than worth refusing:
//! the alternative is failing to cook a file an artist exported at higher depth
//! for no reason, and the wider format arrives with block compression.
//!
//! # Not sRGB-encoded here
//!
//! The bytes are whatever the file held. Whether they are *interpreted* as sRGB
//! is the image view's business at upload time, and baking the choice into the
//! pixels would make one texture unusable as a normal map.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use slop_asset::texture::{Format, Texture};
use slop_asset::{Cache, CacheKey};
use slop_core::diagnostics::tracing::{debug, info, warn};

use crate::cook::Summary;

/// Bump to invalidate every cooked texture.
const COOKER_VERSION: u32 = 1;

/// Where source images live, relative to the project root.
const SOURCE_DIRECTORY: &str = "assets";

/// Cook every PNG under `root/assets` into `root/.slop/cache/textures`.
///
/// # Errors
///
/// Fails if a file cannot be read or decoded, or the cache cannot be written.
pub(crate) fn textures(root: &Path, force: bool) -> Result<Summary> {
    let source_root = root.join(SOURCE_DIRECTORY);
    let cache = Cache::for_project(root);

    if !source_root.is_dir() {
        warn!(path = %source_root.display(), "no assets directory; nothing to cook");
        return Ok(Summary::default());
    }

    let mut sources = Vec::new();
    collect_images(&source_root, &mut sources)?;
    sources.sort();

    let mut summary = Summary::default();

    for source in &sources {
        let relative = source
            .strip_prefix(&source_root)
            .expect("collected paths are under the source root");
        let logical = logical_path(relative);
        let artifact = cache.artifact(&logical);

        let bytes =
            std::fs::read(source).with_context(|| format!("reading image {}", source.display()))?;

        let key = CacheKey::builder()
            .input("cooker", &COOKER_VERSION.to_le_bytes())
            .input("format", &slop_asset::texture::VERSION.to_le_bytes())
            .input("source", &bytes)
            .finish();

        if !force && cache.is_current(&artifact, &key) {
            debug!(logical, "up to date");
            summary.skipped += 1;
            continue;
        }

        let texture = decode(&bytes).with_context(|| format!("decoding {}", source.display()))?;

        cache.prepare(&artifact)?;
        std::fs::write(&artifact, texture.write())
            .with_context(|| format!("writing {}", artifact.display()))?;
        cache.record(&artifact, &key)?;

        info!(
            logical,
            width = texture.width,
            height = texture.height,
            "cooked"
        );
        summary.cooked += 1;
    }

    Ok(summary)
}

/// Where a cooked texture is addressed from.
fn logical_path(relative: &Path) -> String {
    let cooked = relative.with_extension("tex");
    let segments: Vec<String> = cooked
        .components()
        .map(|segment| segment.as_os_str().to_string_lossy().into_owned())
        .collect();

    format!("textures/{}", segments.join("/"))
}

/// Decode PNG bytes to the cooked layout.
fn decode(bytes: &[u8]) -> Result<Texture> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().context("reading PNG header")?;

    let mut raw = vec![0; reader.output_buffer_size().context("image is too large")?];
    let info = reader.next_frame(&mut raw).context("reading pixels")?;
    raw.truncate(info.buffer_size());

    let pixels = to_rgba8(&raw, info.color_type, info.bit_depth)?;

    Ok(Texture {
        width: info.width,
        height: info.height,
        format: Format::Rgba8,
        pixels,
    })
}

/// Expand whatever the file held into eight-bit RGBA.
fn to_rgba8(raw: &[u8], color: png::ColorType, depth: png::BitDepth) -> Result<Vec<u8>> {
    // Sixteen-bit samples are big-endian in a PNG and are narrowed by taking the
    // high byte — see the module docs on why this is narrowed rather than
    // refused.
    let step = match depth {
        png::BitDepth::Eight => 1,
        png::BitDepth::Sixteen => 2,
        other => bail!(
            "bit depth {other:?} is not supported; expand it to 8 or 16 bits per channel first"
        ),
    };

    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            bail!("indexed colour is not supported; the decoder should have expanded it")
        }
    };

    let stride = channels * step;
    let mut pixels = Vec::with_capacity(raw.len() / stride * 4);

    for sample in raw.chunks_exact(stride) {
        // Index 0 of each channel is its high byte at either depth, so this
        // narrows sixteen-bit samples without a separate branch.
        let at = |channel: usize| sample[channel * step];

        let (red, green, blue, alpha) = match color {
            png::ColorType::Grayscale => (at(0), at(0), at(0), 255),
            png::ColorType::GrayscaleAlpha => (at(0), at(0), at(0), at(1)),
            png::ColorType::Rgb => (at(0), at(1), at(2), 255),
            png::ColorType::Rgba => (at(0), at(1), at(2), at(3)),
            png::ColorType::Indexed => unreachable!("rejected above"),
        };

        pixels.extend_from_slice(&[red, green, blue, alpha]);
    }

    Ok(pixels)
}

/// Recursively gather `.png` files.
fn collect_images(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
        let path = entry.path();

        if path.is_dir() {
            collect_images(&path, found)?;
            continue;
        }

        let is_image = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension == "png");

        if is_image {
            found.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_becomes_a_logical_path_under_textures() {
        assert_eq!(
            logical_path(Path::new("props").join("wood.png").as_path()),
            "textures/props/wood.tex"
        );
    }

    #[test]
    fn a_logical_path_uses_forward_slashes_on_every_platform() {
        let logical = logical_path(Path::new("a").join("b").join("c.png").as_path());

        assert!(!logical.contains('\\'), "{logical}");
        assert_eq!(logical, "textures/a/b/c.tex");
    }

    #[test]
    fn rgb_gains_an_opaque_alpha_channel() {
        // A source without alpha must not decode to a transparent texture, which
        // is the failure that looks like the object vanished.
        let raw = [10, 20, 30, 40, 50, 60];

        let pixels = to_rgba8(&raw, png::ColorType::Rgb, png::BitDepth::Eight).expect("valid");

        assert_eq!(pixels, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn greyscale_expands_across_all_three_channels() {
        let raw = [7, 200];

        let pixels =
            to_rgba8(&raw, png::ColorType::Grayscale, png::BitDepth::Eight).expect("valid");

        assert_eq!(pixels, vec![7, 7, 7, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn greyscale_with_alpha_keeps_its_alpha() {
        let raw = [7, 128];

        let pixels =
            to_rgba8(&raw, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight).expect("valid");

        assert_eq!(pixels, vec![7, 7, 7, 128]);
    }

    #[test]
    fn rgba_passes_through_unchanged() {
        let raw = [1, 2, 3, 4];

        let pixels = to_rgba8(&raw, png::ColorType::Rgba, png::BitDepth::Eight).expect("valid");

        assert_eq!(pixels, raw.to_vec());
    }

    #[test]
    fn sixteen_bit_samples_narrow_to_their_high_byte() {
        // Big-endian in a PNG, so the high byte comes first. Taking the low byte
        // instead would turn a smooth gradient into noise.
        let raw = [0xAB, 0xCD, 0x12, 0x34, 0x56, 0x78];

        let pixels = to_rgba8(&raw, png::ColorType::Rgb, png::BitDepth::Sixteen).expect("valid");

        assert_eq!(pixels, vec![0xAB, 0x12, 0x56, 255]);
    }

    #[test]
    fn an_unsupported_bit_depth_says_what_to_do_about_it() {
        let error =
            to_rgba8(&[0], png::ColorType::Grayscale, png::BitDepth::Four).expect_err("four bits");

        assert!(error.to_string().contains("bit depth"), "{error}");
    }
}
