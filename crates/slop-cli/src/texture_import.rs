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
//!
//! # Then compressed to BC7
//!
//! `docs/DESIGN.md` §2.8 wants block compression, and this is where it happens.
//! BC7 is a quarter the size of RGBA8 **in VRAM**, not merely on disk: the GPU
//! samples the compressed blocks directly and never expands them. On a real
//! scene that is the difference between a texture budget that fits and one that
//! does not.
//!
//! The encoder is Intel's ISPC texture compressor via `intel_tex_2`. Writing a
//! BC7 encoder is a serious project on its own — eight modes, partition tables,
//! endpoint fitting — and `docs/DESIGN.md` §3's write/take line puts it firmly on
//! the take side: it is a solved, self-contained, offline problem with no bearing
//! on the engine's architecture. It is a dependency of the **cooker only**, so
//! nothing it brings in can reach a shipped build (`slop-asset` invariant 7).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use slop_asset::texture::{Format, Texture};
use slop_asset::{Cache, CacheKey};
use slop_core::diagnostics::tracing::{debug, info, warn};

use crate::cook::Summary;

/// Bump to invalidate every cooked texture.
///
/// 2 — textures are compressed to BC7.
const COOKER_VERSION: u32 = 2;

/// Where source images live, relative to the project root.
const SOURCE_DIRECTORY: &str = "assets";

/// Texels along each edge of a block-compressed block.
const BLOCK: u32 = 4;

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

        let decoded = decode(&bytes).with_context(|| format!("decoding {}", source.display()))?;
        let uncompressed = decoded.pixels.len();
        let texture = compress(decoded);

        cache.prepare(&artifact)?;
        std::fs::write(&artifact, texture.write())
            .with_context(|| format!("writing {}", artifact.display()))?;
        cache.record(&artifact, &key)?;

        info!(
            logical,
            width = texture.width,
            height = texture.height,
            format = ?texture.format,
            bytes = texture.pixels.len(),
            was = uncompressed,
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

/// Compress an RGBA8 texture to BC7.
///
/// The dimensions are unchanged — the *real* ones are what the header carries.
/// What changes is the payload, which becomes 4×4 blocks.
pub(crate) fn compress(texture: Texture) -> Texture {
    debug_assert_eq!(texture.format, Format::Rgba8, "only RGBA8 is compressible");

    let padded_width = texture.width.div_ceil(BLOCK) * BLOCK;
    let padded_height = texture.height.div_ceil(BLOCK) * BLOCK;
    let padded = pad_to_blocks(&texture, padded_width, padded_height);

    // `alpha` rather than `opaque`: the opaque modes ignore the alpha channel
    // entirely and reconstruct it as 255. That is right for an albedo with no
    // transparency and silently wrong for anything using alpha for cutout or
    // blending, and nothing here knows which this is — that is what per-asset
    // import settings decide, and `docs/PLAN.md` §6.1 records their absence.
    //
    // `slow` because cooking is offline and cached. The quality difference
    // between the settings is visible on gradients, and the time difference is
    // paid once per source change rather than per run.
    let settings = intel_tex_2::bc7::alpha_slow_settings();
    let surface = intel_tex_2::RgbaSurface {
        width: padded_width,
        height: padded_height,
        stride: padded_width * 4,
        data: &padded,
    };

    // `intel_tex_2::bc7::calc_output_size` sizes the output as
    // `ceil(width * height / 16) * 16`, which is the block count only when both
    // dimensions are already multiples of four — for a 5×5 surface it returns
    // two blocks where four are needed. Padding first is what makes its
    // arithmetic and this crate's agree; passing an unpadded surface would
    // under-allocate and lose the last row of blocks.
    let pixels = intel_tex_2::bc7::compress_blocks(&settings, &surface);
    debug_assert_eq!(
        pixels.len(),
        Format::Bc7.payload_bytes(texture.width, texture.height)
    );

    Texture {
        width: texture.width,
        height: texture.height,
        format: Format::Bc7,
        pixels,
    }
}

/// Grow an RGBA8 image to `width × height` by repeating its edge pixels.
///
/// Edge replication rather than zero fill. The padding sits inside blocks whose
/// other texels *are* sampled, and BC7 fits one pair of endpoints per block — so
/// filling with black drags those endpoints toward black and dims the real
/// texels beside it. Repeating the edge costs the fit nothing.
fn pad_to_blocks(texture: &Texture, width: u32, height: u32) -> Vec<u8> {
    if texture.width == width && texture.height == height {
        return texture.pixels.clone();
    }

    let mut padded = Vec::with_capacity(width as usize * height as usize * 4);

    for y in 0..height {
        let source_y = y.min(texture.height - 1);

        for x in 0..width {
            let source_x = x.min(texture.width - 1);
            let at = ((source_y * texture.width + source_x) * 4) as usize;

            padded.extend_from_slice(&texture.pixels[at..at + 4]);
        }
    }

    padded
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

    /// An RGBA8 texture of `width × height`, with a recognisable pattern.
    fn sample(width: u32, height: u32) -> Texture {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);

        for y in 0..height {
            for x in 0..width {
                let bright = ((x / 4) + (y / 4)) % 2 == 0;
                let value = if bright { 220 } else { 40 };

                pixels.extend_from_slice(&[value, value / 2, x as u8, 255]);
            }
        }

        Texture {
            width,
            height,
            format: Format::Rgba8,
            pixels,
        }
    }

    #[test]
    fn compression_keeps_the_real_dimensions() {
        // The header carries the true size, never the padded one. Storing the
        // padded size would make every consumer undo it, and one would forget.
        let compressed = compress(sample(64, 64));

        assert_eq!(compressed.width, 64);
        assert_eq!(compressed.height, 64);
        assert_eq!(compressed.format, Format::Bc7);
    }

    #[test]
    fn compression_is_four_to_one() {
        let source = sample(64, 64);
        let uncompressed = source.pixels.len();
        let compressed = compress(source);

        assert_eq!(uncompressed, 64 * 64 * 4);
        assert_eq!(compressed.pixels.len(), uncompressed / 4);
    }

    #[test]
    fn a_size_that_is_not_a_multiple_of_four_still_covers_whole_blocks() {
        // The hazard `intel_tex_2` presents: its `calc_output_size` is
        // `ceil(width * height / 16) * 16`, which is the block count only when
        // both dimensions are already multiples of four. For 5x5 it returns two
        // blocks where four are needed. Padding first is what reconciles it, and
        // this is the test that fails if the padding is ever dropped.
        let compressed = compress(sample(5, 5));

        assert_eq!(compressed.width, 5);
        assert_eq!(compressed.height, 5);
        assert_eq!(
            compressed.pixels.len(),
            2 * 2 * 16,
            "5x5 needs 2x2 blocks, not ceil(25/16)"
        );
        assert_eq!(
            compressed.pixels.len(),
            Format::Bc7.payload_bytes(5, 5),
            "and the format crate must agree"
        );
    }

    #[test]
    fn every_awkward_size_agrees_with_the_format_crate() {
        // The cooker and the reader compute the payload size independently. If
        // they ever disagree, the reader either truncates the last blocks or
        // refuses the file as short.
        for (width, height) in [(1, 1), (3, 7), (4, 4), (13, 4), (4, 13), (17, 31), (64, 64)] {
            let compressed = compress(sample(width, height));

            assert_eq!(
                compressed.pixels.len(),
                Format::Bc7.payload_bytes(width, height),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn compressing_the_same_pixels_twice_gives_the_same_bytes() {
        // The cook cache keys on inputs and assumes the cooker is a function of
        // them. An encoder that varied run to run — a thread count leaking into
        // the result, say — would make every cook produce a different artifact
        // while every stamp still matched.
        let first = compress(sample(32, 32));
        let second = compress(sample(32, 32));

        assert_eq!(first.pixels, second.pixels);
    }

    #[test]
    fn padding_repeats_the_edge_rather_than_filling_with_black() {
        // BC7 fits one pair of endpoints per 4x4 block, so padding with black
        // drags the endpoints of an edge block toward black and dims the real
        // texels beside it. Repeating the edge costs the fit nothing.
        let texture = sample(2, 2);
        let padded = pad_to_blocks(&texture, 4, 4);

        assert_eq!(padded.len(), 4 * 4 * 4);

        let texel = |x: usize, y: usize| &padded[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];

        assert_eq!(texel(2, 0), texel(1, 0), "the last column repeats");
        assert_eq!(texel(3, 0), texel(1, 0));
        assert_eq!(texel(0, 2), texel(0, 1), "and the last row");
        assert_eq!(texel(3, 3), texel(1, 1), "including the corner");
    }

    #[test]
    fn padding_an_already_aligned_image_changes_nothing() {
        let texture = sample(8, 8);
        let padded = pad_to_blocks(&texture, 8, 8);

        assert_eq!(padded, texture.pixels);
    }

    #[test]
    fn an_unsupported_bit_depth_says_what_to_do_about_it() {
        let error =
            to_rgba8(&[0], png::ColorType::Grayscale, png::BitDepth::Four).expect_err("four bits");

        assert!(error.to_string().contains("bit depth"), "{error}");
    }
}
