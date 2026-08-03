//! The cooked texture format.
//!
//! What a PNG import produces and what the engine uploads. As with
//! [`mesh`](crate::mesh), both sides have to agree on it — `slop-cli` writes it,
//! the engine reads it — so it lives with the pipeline that produces it.
//!
//! # Layout
//!
//! ```text
//! magic      8 bytes  "SLOPTEX0"
//! version    u32      VERSION
//! width      u32      level zero, in pixels
//! height     u32      level zero, in pixels
//! format     u32      a Format discriminant
//! mip_levels u32      how many levels follow, including level zero
//! pixel data          every level, largest first, tightly packed
//! ```
//!
//! # Mip levels
//!
//! Level zero is the full-size image; each level after it is half the previous
//! in each dimension, floored at one, down to 1×1. They are concatenated with no
//! padding and no offset table — [`Texture::level`] walks the chain to find one,
//! so a stored table cannot disagree with the payload it describes.
//!
//! The whole chain costs about a third more than level zero alone, which is the
//! sum of a geometric series with ratio ¼ and the reason mips are affordable.
//! They are generated at cook time rather than on the GPU because a
//! block-compressed level cannot be filtered down after the fact — see
//! `slop-cli`'s `texture_import::generate_mips`.
//!
//! Little-endian and decoded from bytes, for the reasons [`mesh`](crate::mesh)
//! gives: `fs::read` returns a buffer aligned to 1, and byte order should be a
//! property of the format rather than of whichever machine cooked it. Pixels are
//! bytes, so only the header is parsed and the payload is copied.
//!
//! # Two payload shapes, one header
//!
//! [`Format::Rgba8`] stores `width × height × 4` loose bytes. [`Format::Bc7`]
//! stores 4×4 blocks of sixteen bytes each, `ceil(w/4) × ceil(h/4)` of them —
//! four times smaller, and the GPU samples it without ever expanding it, which
//! is what `docs/DESIGN.md` §2.8 wants block compression *for*. The saving is in
//! VRAM and bandwidth at sample time, not in file size.
//!
//! The dimensions in the header are always the **real** ones. A 63×63 BC7
//! texture stores 16×16 blocks covering 64×64 texels, and the extra row and
//! column are padding that is never sampled. Storing the padded size instead
//! would make every consumer undo it, and one of them would forget.
//!
//! This module never decodes BC7 — nothing on the CPU needs to. It knows how
//! many bytes a payload should be, and the hardware does the rest.

use thiserror::Error;

/// The first eight bytes of every cooked texture.
const MAGIC: &[u8; 8] = b"SLOPTEX0";

/// What this module knows how to read.
///
/// Bumped to 2 when mip levels were added. The header grew rather than the
/// meaning of a field changing, so a version 1 artifact is rejected outright
/// instead of being read as a one-level version 2 — a cooked cache from before
/// the change is regenerated, which is what the content hash already forces.
pub const VERSION: u32 = 2;

/// Bytes before the pixel data.
const HEADER: usize = 28;

/// How a cooked texture's pixels are stored.
///
/// A discriminant in the header rather than an assumption, so that adding block
/// compression later is a new variant rather than a new format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Eight bits per channel, four channels, tightly packed.
    ///
    /// **Not sRGB-encoded.** Whether the values are interpreted as sRGB is the
    /// image view's business, and baking that choice into the pixels would make
    /// one texture unusable as a normal map.
    Rgba8,

    /// BC7, four bits per texel, in 4×4 blocks of sixteen bytes.
    ///
    /// The desktop colour-texture format: four channels, and the only BC format
    /// that handles both sharp colour transitions and alpha well. Lossy, but
    /// near-perceptually-lossless on photographic content, which is why it is
    /// what a shipped game uses for albedo.
    ///
    /// Also **not** sRGB-encoded, for the same reason as [`Format::Rgba8`] —
    /// BC7 has an sRGB Vulkan format and a UNORM one over identical bytes, so
    /// the choice stays with the view.
    Bc7,
}

/// Texels along each edge of a block-compressed block.
const BLOCK: usize = 4;

/// Bytes one BC7 block occupies. Fixed, which is what makes the size arithmetic
/// exact rather than a bound.
const BC7_BLOCK_BYTES: usize = 16;

impl Format {
    /// The discriminant written into the header.
    const fn code(self) -> u32 {
        match self {
            Self::Rgba8 => 0,
            Self::Bc7 => 1,
        }
    }

    /// Whether the payload is 4×4 blocks rather than loose pixels.
    pub const fn is_block_compressed(self) -> bool {
        match self {
            Self::Rgba8 => false,
            Self::Bc7 => true,
        }
    }

    /// How many bytes a `width × height` image occupies in this format.
    ///
    /// The one place the two payload shapes are reconciled. A block format
    /// rounds *up* to whole blocks, so a 63×63 BC7 image is the same size as a
    /// 64×64 one — the padding is real bytes on disk and in VRAM, and pretending
    /// otherwise is how a buffer ends up one block short.
    pub const fn payload_bytes(self, width: u32, height: u32) -> usize {
        let width = width as usize;
        let height = height as usize;

        match self {
            Self::Rgba8 => width * height * 4,
            Self::Bc7 => width.div_ceil(BLOCK) * height.div_ceil(BLOCK) * BC7_BLOCK_BYTES,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Rgba8),
            1 => Some(Self::Bc7),
            _ => None,
        }
    }
}

/// Why a cooked texture could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextureError {
    /// The mip level count is zero, or more levels than the dimensions allow.
    #[error(
        "a {width}x{height} texture can have at most {possible} mip levels, and this claims \
         {found}"
    )]
    MipLevels {
        /// What the header says.
        found: u32,
        /// What a full chain down to 1x1 would be.
        possible: u32,
        /// Level zero's width.
        width: u32,
        /// Level zero's height.
        height: u32,
    },

    /// The bytes are not a cooked texture.
    #[error("not a cooked texture: expected magic {expected:?}, found {found:?}")]
    NotATexture {
        /// What every cooked texture starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// A cooked texture from a different version of the format.
    #[error("cooked texture is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// A pixel format this build does not know.
    #[error("cooked texture uses pixel format {code}, which this build does not know")]
    UnknownFormat {
        /// The discriminant in the header.
        code: u32,
    },

    /// The file ends before it says it should.
    #[error("cooked texture is truncated: {expected} bytes declared, {found} present")]
    Truncated {
        /// How many bytes the header implies.
        expected: usize,
        /// How many there are.
        found: usize,
    },

    /// A dimension is zero, so there are no pixels to upload.
    #[error("cooked texture is {width}x{height}, which has no pixels")]
    Empty {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },
}

/// A texture, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// How the pixels are stored.
    pub format: Format,
    /// How many mip levels [`pixels`](Self::pixels) holds, including level 0.
    ///
    /// One means no mips. Generated at cook time rather than on the GPU, because
    /// a block-compressed texture cannot be filtered down after the fact: BC7
    /// blocks would have to be decoded, halved and recompressed, which is both
    /// slow and lossier than compressing each level from the original pixels.
    pub mip_levels: u32,
    /// Every level's pixels, largest first, tightly packed and concatenated.
    ///
    /// One allocation rather than a `Vec<Vec<u8>>` so an upload is one staging
    /// buffer and one copy region per level, rather than one buffer per level.
    /// Use [`level`](Self::level) to find a level within it.
    pub pixels: Vec<u8>,
}

/// Where one mip level lives, and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    /// Byte offset into [`Texture::pixels`].
    pub offset: usize,
    /// Length in bytes.
    pub bytes: usize,
    /// This level's width in texels. Never zero.
    pub width: u32,
    /// This level's height in texels. Never zero.
    pub height: u32,
}

/// Halve a dimension, without ever reaching zero.
///
/// A 256×1 texture's chain is 128×1, 64×1 … 1×1: the short edge stops at one
/// while the long edge keeps halving. Letting it reach zero would produce an
/// empty level that Vulkan rejects.
#[must_use]
pub const fn halve(size: u32) -> u32 {
    if size > 1 { size / 2 } else { 1 }
}

/// How many levels a full chain down to 1×1 has.
///
/// The count Vulkan calls `VK_REMAINING_MIP_LEVELS` resolves to: one level per
/// halving of the *longer* edge, plus the original.
#[must_use]
pub const fn full_mip_chain(width: u32, height: u32) -> u32 {
    let mut longest = if width > height { width } else { height };
    let mut levels = 1;

    while longest > 1 {
        longest /= 2;
        levels += 1;
    }

    levels
}

impl Texture {
    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.pixels.len());

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.format.code().to_le_bytes());
        out.extend_from_slice(&self.mip_levels.to_le_bytes());
        out.extend_from_slice(&self.pixels);

        out
    }

    /// Where level `index` lives within [`pixels`](Self::pixels).
    ///
    /// Returns `None` past the last level. Computed by walking the chain rather
    /// than stored, so an offset table cannot disagree with the payload it
    /// describes.
    #[must_use]
    pub fn level(&self, index: u32) -> Option<Level> {
        if index >= self.mip_levels {
            return None;
        }

        let mut offset = 0;
        let mut width = self.width;
        let mut height = self.height;

        for _ in 0..index {
            offset += self.format.payload_bytes(width, height);
            width = halve(width);
            height = halve(height);
        }

        Some(Level {
            offset,
            bytes: self.format.payload_bytes(width, height),
            width,
            height,
        })
    }

    /// Every level, largest first.
    pub fn levels(&self) -> impl Iterator<Item = Level> + '_ {
        (0..self.mip_levels).filter_map(|index| self.level(index))
    }

    /// Bytes every level together occupies.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.levels().map(|level| level.bytes).sum()
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`TextureError`] for anything that is not a cooked texture of this
    /// version and a known pixel format, or is shorter than its header claims.
    pub fn read(bytes: &[u8]) -> Result<Self, TextureError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(TextureError::NotATexture {
                expected: "SLOPTEX0",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(TextureError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let width = read_u32(bytes, 12);
        let height = read_u32(bytes, 16);
        let code = read_u32(bytes, 20);
        let mip_levels = read_u32(bytes, 24);

        let format = Format::from_code(code).ok_or(TextureError::UnknownFormat { code })?;

        if width == 0 || height == 0 {
            return Err(TextureError::Empty { width, height });
        }

        // Checked before the payload size is computed from it: a corrupt count
        // would otherwise drive the loop below, and a very large one would spend
        // a long time summing levels that cannot exist.
        let possible = full_mip_chain(width, height);
        if mip_levels == 0 || mip_levels > possible {
            return Err(TextureError::MipLevels {
                found: mip_levels,
                possible,
                width,
                height,
            });
        }

        // Built without its pixels so `payload_bytes` can walk the chain, then
        // filled in. The alternative is duplicating the walk here.
        let mut texture = Self {
            width,
            height,
            format,
            mip_levels,
            pixels: Vec::new(),
        };

        let expected = HEADER + texture.payload_bytes();
        if bytes.len() < expected {
            return Err(TextureError::Truncated {
                expected,
                found: bytes.len(),
            });
        }

        texture.pixels = bytes[HEADER..expected].to_vec();

        Ok(texture)
    }

    /// Bytes one row of pixels occupies.
    ///
    /// # Panics
    ///
    /// If the format is block-compressed. A BC7 payload has no rows — it has
    /// rows of *blocks*, each covering four scanlines — so a stride is not a
    /// thing it has. Returning something plausible instead would be handing a
    /// caller a number that reads four rows at once.
    pub const fn stride(&self) -> usize {
        assert!(
            !self.format.is_block_compressed(),
            "a block-compressed texture has no row stride"
        );

        self.width as usize * 4
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod mip_tests {
    use super::*;

    fn chained(width: u32, height: u32, format: Format) -> Texture {
        let levels = full_mip_chain(width, height);

        let mut texture = Texture {
            width,
            height,
            format,
            mip_levels: levels,
            pixels: Vec::new(),
        };
        texture.pixels = vec![7; texture.payload_bytes()];

        texture
    }

    #[test]
    fn a_square_chain_ends_at_one_by_one() {
        assert_eq!(full_mip_chain(256, 256), 9, "256 halves nine times to 1");
        assert_eq!(full_mip_chain(1, 1), 1);
    }

    #[test]
    fn a_long_thin_chain_follows_the_longer_edge() {
        // 256x1 must keep halving the long edge while the short one stays at 1.
        // Following the *shorter* edge would stop after one level and leave a
        // 128-wide texture with no mips, which is exactly where aliasing shows.
        assert_eq!(full_mip_chain(256, 1), 9);
        assert_eq!(halve(1), 1, "a dimension never reaches zero");
    }

    #[test]
    fn levels_tile_the_payload_without_gaps_or_overlap() {
        // The property `level` exists to guarantee: every level's bytes sit end
        // to end, so one staging buffer and one copy per level covers exactly
        // the payload. An off-by-one here uploads garbage into a level and shows
        // as a texture that is right up close and wrong at distance.
        for (width, height) in [(256, 256), (64, 32), (5, 5), (1, 1), (256, 1)] {
            for format in [Format::Rgba8, Format::Bc7] {
                let texture = chained(width, height, format);

                let mut expected_offset = 0;
                for level in texture.levels() {
                    assert_eq!(
                        level.offset, expected_offset,
                        "{width}x{height} {format:?} has a gap"
                    );
                    expected_offset += level.bytes;
                }

                assert_eq!(
                    expected_offset,
                    texture.pixels.len(),
                    "{width}x{height} {format:?} levels must cover the payload exactly"
                );
            }
        }
    }

    #[test]
    fn a_chain_round_trips_through_write_and_read() {
        let texture = chained(64, 32, Format::Bc7);
        let decoded = Texture::read(&texture.write()).expect("a written texture must read back");

        assert_eq!(decoded, texture);
        assert_eq!(decoded.mip_levels, 7);
    }

    #[test]
    fn more_levels_than_the_dimensions_allow_is_rejected() {
        // Rather than trusted and used to walk off the end of the payload.
        let mut texture = chained(4, 4, Format::Rgba8);
        texture.mip_levels = 9;

        let failure = Texture::read(&texture.write()).expect_err("an impossible chain is invalid");

        assert!(
            matches!(failure, TextureError::MipLevels { possible: 3, .. }),
            "{failure:?}"
        );
    }

    #[test]
    fn zero_levels_is_rejected() {
        let mut texture = chained(4, 4, Format::Rgba8);
        texture.mip_levels = 0;

        assert!(matches!(
            Texture::read(&texture.write()),
            Err(TextureError::MipLevels { found: 0, .. })
        ));
    }

    #[test]
    fn a_truncated_chain_is_caught() {
        // Level zero present, later levels missing. Reading only level zero's
        // size would accept this and leave the rest of the chain uninitialised.
        let texture = chained(64, 64, Format::Bc7);
        let mut bytes = texture.write();
        bytes.truncate(HEADER + texture.format.payload_bytes(64, 64));

        assert!(matches!(
            Texture::read(&bytes),
            Err(TextureError::Truncated { .. })
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Texture {
        Texture {
            width: 2,
            height: 2,
            mip_levels: 1,
            format: Format::Rgba8,
            pixels: vec![
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255, //
                255, 255, 255, 255,
            ],
        }
    }

    #[test]
    fn a_texture_round_trips() {
        let texture = small();

        assert_eq!(Texture::read(&texture.write()), Ok(texture));
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = small().write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_u32(&bytes, 12), 2, "width");
        assert_eq!(read_u32(&bytes, 16), 2, "height");
        assert_eq!(read_u32(&bytes, 20), 0, "Rgba8");
        assert_eq!(bytes.len(), HEADER + 16);
    }

    /// A BC7 texture of `width × height`, with plausible block bytes.
    fn compressed(width: u32, height: u32) -> Texture {
        Texture {
            width,
            height,
            mip_levels: 1,
            format: Format::Bc7,
            pixels: (0..Format::Bc7.payload_bytes(width, height))
                .map(|index| index as u8)
                .collect(),
        }
    }

    #[test]
    fn a_block_compressed_texture_round_trips() {
        let texture = compressed(8, 8);

        assert_eq!(Texture::read(&texture.write()), Ok(texture));
    }

    #[test]
    fn a_block_payload_rounds_up_to_whole_blocks() {
        // A 63x63 image is 16x16 blocks, the same as a 64x64 one. The padding is
        // real bytes on disk and in VRAM; pretending otherwise is how a buffer
        // ends up one block short.
        assert_eq!(Format::Bc7.payload_bytes(64, 64), 16 * 16 * 16);
        assert_eq!(Format::Bc7.payload_bytes(63, 63), 16 * 16 * 16);
        assert_eq!(Format::Bc7.payload_bytes(1, 1), 16, "one block minimum");
        assert_eq!(Format::Bc7.payload_bytes(5, 5), 4 * 16, "2x2 blocks");
    }

    #[test]
    fn a_block_format_is_a_quarter_of_raw_pixels() {
        // True only for sizes that are already whole blocks, which is the
        // comparison worth making — it is the ratio the format exists for.
        assert_eq!(
            Format::Bc7.payload_bytes(256, 256) * 4,
            Format::Rgba8.payload_bytes(256, 256)
        );
    }

    #[test]
    fn a_truncated_block_payload_is_refused() {
        // The failure a wrong size rule produces: the reader accepts a file with
        // its last row of blocks missing and hands the GPU a short buffer.
        let mut bytes = compressed(8, 8).write();
        bytes.truncate(bytes.len() - 16);

        assert!(matches!(
            Texture::read(&bytes),
            Err(TextureError::Truncated { .. })
        ));
    }

    #[test]
    fn the_two_formats_have_different_codes() {
        // A shared discriminant would make every cooked texture decode as
        // whichever variant came first, with the payload size rule to match.
        let raw = small().write();
        let block = compressed(8, 8).write();

        assert_eq!(read_u32(&raw, 20), 0, "Rgba8");
        assert_eq!(read_u32(&block, 20), 1, "Bc7");
    }

    #[test]
    #[should_panic(expected = "no row stride")]
    fn a_block_compressed_texture_has_no_row_stride() {
        // Returning something plausible would hand a caller a number that reads
        // four scanlines at once.
        let _ = compressed(8, 8).stride();
    }

    #[test]
    fn pixels_survive_byte_for_byte() {
        // The payload is copied rather than decoded, so this really checks that
        // the header size is right and nothing is off by a row.
        let texture = small();
        let back = Texture::read(&texture.write()).expect("valid");

        assert_eq!(back.pixels, texture.pixels);
        assert_eq!(back.stride(), 8);
    }

    #[test]
    fn something_that_is_not_a_texture_is_refused() {
        let error = Texture::read(b"not a texture here").expect_err("wrong magic");

        assert!(matches!(error, TextureError::NotATexture { .. }));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Texture::read(&[]).is_err());
        assert!(Texture::read(b"SLOP").is_err());
    }

    #[test]
    fn a_different_version_says_so() {
        let mut bytes = small().write();
        bytes[8] = 42;

        assert!(matches!(
            Texture::read(&bytes),
            Err(TextureError::Version { found: 42, .. })
        ));
    }

    #[test]
    fn an_unknown_pixel_format_is_refused_rather_than_assumed() {
        // What lets a build from before block compression say so, instead of
        // uploading compressed blocks as though they were RGBA.
        let mut bytes = small().write();
        bytes[20] = 7;

        assert!(matches!(
            Texture::read(&bytes),
            Err(TextureError::UnknownFormat { code: 7 })
        ));
    }

    #[test]
    fn a_truncated_texture_is_refused() {
        let mut bytes = small().write();
        bytes.truncate(bytes.len() - 4);

        assert!(matches!(
            Texture::read(&bytes),
            Err(TextureError::Truncated { .. })
        ));
    }

    #[test]
    fn a_zero_dimension_is_refused() {
        // Caught before the size arithmetic, which would otherwise report a
        // zero-byte texture as valid and hand the GPU nothing.
        let texture = Texture {
            width: 0,
            height: 4,
            mip_levels: 1,
            format: Format::Rgba8,
            pixels: Vec::new(),
        };

        assert!(matches!(
            Texture::read(&texture.write()),
            Err(TextureError::Empty { width: 0, .. })
        ));
    }
}
