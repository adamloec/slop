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
//! width      u32      pixels
//! height     u32      pixels
//! format     u32      a Format discriminant
//! pixel data          width × height × 4, tightly packed
//! ```
//!
//! Little-endian and decoded from bytes, for the reasons [`mesh`](crate::mesh)
//! gives: `fs::read` returns a buffer aligned to 1, and byte order should be a
//! property of the format rather than of whichever machine cooked it. Pixels are
//! bytes, so only the header is parsed and the payload is copied.
//!
//! # Uncompressed, for now
//!
//! `docs/DESIGN.md` §2.8 wants block compression — BC7 and friends — which is
//! what makes a texture cheap in VRAM rather than merely cheap to parse. That is
//! another step over the same pixels, recorded in `docs/PLAN.md` §6.1. The
//! header carries a [`Format`] discriminant now precisely so that adding one is
//! a new variant rather than a new format.

use thiserror::Error;

/// The first eight bytes of every cooked texture.
const MAGIC: &[u8; 8] = b"SLOPTEX0";

/// What this module knows how to read.
pub const VERSION: u32 = 1;

/// Bytes before the pixel data.
const HEADER: usize = 24;

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
}

impl Format {
    /// The discriminant written into the header.
    const fn code(self) -> u32 {
        match self {
            Self::Rgba8 => 0,
        }
    }

    /// Bytes per pixel.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Rgba8),
            _ => None,
        }
    }
}

/// Why a cooked texture could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextureError {
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
    /// Tightly packed pixel data, top row first.
    pub pixels: Vec<u8>,
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
        out.extend_from_slice(&self.pixels);

        out
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

        let format = Format::from_code(code).ok_or(TextureError::UnknownFormat { code })?;

        if width == 0 || height == 0 {
            return Err(TextureError::Empty { width, height });
        }

        let expected = HEADER + width as usize * height as usize * format.bytes_per_pixel();
        if bytes.len() < expected {
            return Err(TextureError::Truncated {
                expected,
                found: bytes.len(),
            });
        }

        Ok(Self {
            width,
            height,
            format,
            pixels: bytes[HEADER..expected].to_vec(),
        })
    }

    /// Bytes one row occupies.
    pub const fn stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_pixel()
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Texture {
        Texture {
            width: 2,
            height: 2,
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
            format: Format::Rgba8,
            pixels: Vec::new(),
        };

        assert!(matches!(
            Texture::read(&texture.write()),
            Err(TextureError::Empty { width: 0, .. })
        ));
    }
}
