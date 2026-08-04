//! The cooked environment format.
//!
//! `docs/PLAN.md` §9.7. What an HDR panorama becomes, and what image-based
//! lighting reads. As with [`texture`](crate::texture), both sides have to agree
//! on it — `slop-cook` writes it, the engine uploads it — so it lives with the
//! pipeline that produces it.
//!
//! # Layout
//!
//! ```text
//! magic      8 bytes  "SLOPENV0"
//! version    u32      VERSION
//! size       u32      level zero's edge, in texels
//! mip_levels u32      how many levels follow, including level zero
//! format     u32      a texture::Format discriminant
//! faces      u32      always FACES; written so a change is a format change
//! texels              every level, largest first; six faces within each level
//! ```
//!
//! # Why the faces are inside the levels, not the other way round
//!
//! A cube map's mip chain is two nested loops and either nesting is expressible.
//! This one is level-major because of what writes it: the prefilter produces one
//! whole roughness level at a time across all six faces, and the level-major
//! order lets it append. The reader does not care — [`Environment::face`] does
//! the arithmetic either way — and Vulkan does not care, since a copy region
//! names a mip level and an array layer independently.
//!
//! # What is *in* a level changes, and that is a version bump
//!
//! At E6a a level is the environment box-filtered — an ordinary mip chain. From
//! E6c a level is the environment **prefiltered for a roughness**, which is a
//! different image with the same shape. Nothing about the layout moves, so the
//! change is [`VERSION`] plus a recook, which is exactly what a content-hashed
//! cache is for. Recording it here rather than in a commit message, because a
//! consumer that assumed "mip level" meant "smaller copy" would be wrong in a way
//! that renders plausibly.
//!
//! Little-endian and decoded from bytes, for the reasons [`mesh`](crate::mesh)
//! gives. Texels are bytes, so only the header is parsed and the payload is
//! copied — this module never decodes a half float, because nothing on the CPU
//! needs to.

use thiserror::Error;

use crate::texture::{Format, full_mip_chain, halve};

/// The first eight bytes of every cooked environment.
const MAGIC: &[u8; 8] = b"SLOPENV0";

/// What this module knows how to read.
pub const VERSION: u32 = 1;

/// Bytes before the texel data.
///
/// The magic and five `u32`. Stated as one number rather than summed at each use
/// so the reader and the writer cannot disagree, and asserted against the layout
/// by `the_header_is_where_the_layout_says`.
const HEADER: usize = 28;

/// Faces in a cube, which is also its array layer count.
///
/// Written into the header rather than assumed on both sides. It cannot vary
/// today; recording it is what makes a future octahedral or 2D environment a
/// **refused** artifact rather than one read as a cube with the wrong stride.
pub const FACES: u32 = 6;

/// Why a cooked environment could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvironmentError {
    /// The bytes are not a cooked environment.
    #[error("not a cooked environment: expected magic {expected:?}, found {found:?}")]
    NotAnEnvironment {
        /// What every cooked environment starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// A cooked environment from a different version of the format.
    #[error("cooked environment is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// A face count this build cannot interpret.
    #[error("cooked environment has {found} faces, not {expected}")]
    Faces {
        /// What a cube has.
        expected: u32,
        /// What the file claims.
        found: u32,
    },

    /// A texel format this build does not know, or one an environment cannot use.
    #[error("cooked environment uses texel format {code}, which it cannot be stored in")]
    UnknownFormat {
        /// The discriminant in the header.
        code: u32,
    },

    /// The edge length is zero, so there are no texels.
    #[error("cooked environment has faces {size}x{size}, which have no texels")]
    Empty {
        /// The declared edge length.
        size: u32,
    },

    /// The mip level count is zero, or more levels than the size allows.
    #[error(
        "a {size}x{size} environment can have at most {possible} mip levels, and this claims \
         {found}"
    )]
    MipLevels {
        /// What the header says.
        found: u32,
        /// What a full chain down to 1×1 would be.
        possible: u32,
        /// Level zero's edge length.
        size: u32,
    },

    /// The file ends before it says it should.
    #[error("cooked environment is truncated: {expected} bytes declared, {found} present")]
    Truncated {
        /// How many bytes the header implies.
        expected: usize,
        /// How many there are.
        found: usize,
    },
}

/// An environment, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    /// Level zero's edge length, in texels. Faces are square.
    pub size: u32,
    /// How many levels [`texels`](Self::texels) holds, including level zero.
    pub mip_levels: u32,
    /// How the texels are stored.
    pub format: Format,
    /// Every level's texels, largest first, six faces within each level.
    ///
    /// One allocation rather than a nested `Vec`, for [`Texture`]'s reason: an
    /// upload is one staging buffer and one copy region per face per level.
    ///
    /// [`Texture`]: crate::texture::Texture
    pub texels: Vec<u8>,
}

/// Where one face of one level lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceLevel {
    /// Byte offset into [`Environment::texels`].
    pub offset: usize,
    /// Length in bytes.
    pub bytes: usize,
    /// This level's edge length, in texels. Never zero.
    pub size: u32,
}

impl Environment {
    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.texels.len());

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.mip_levels.to_le_bytes());
        out.extend_from_slice(&self.format.code().to_le_bytes());
        out.extend_from_slice(&FACES.to_le_bytes());
        out.extend_from_slice(&self.texels);

        out
    }

    /// Where face `face` of level `level` lives.
    ///
    /// Returns `None` past the last level or the last face. Computed by walking
    /// the chain rather than stored, for [`Texture::level`]'s reason: an offset
    /// table cannot then disagree with the payload it describes.
    ///
    /// [`Texture::level`]: crate::texture::Texture::level
    #[must_use]
    pub fn face(&self, level: u32, face: u32) -> Option<FaceLevel> {
        if level >= self.mip_levels || face >= FACES {
            return None;
        }

        let mut offset = 0;
        let mut size = self.size;

        for _ in 0..level {
            offset += self.format.payload_bytes(size, size) * FACES as usize;
            size = halve(size);
        }

        let bytes = self.format.payload_bytes(size, size);

        Some(FaceLevel {
            offset: offset + bytes * face as usize,
            bytes,
            size,
        })
    }

    /// Every face of every level, level-major, in the order they are stored.
    pub fn faces(&self) -> impl Iterator<Item = FaceLevel> + '_ {
        (0..self.mip_levels)
            .flat_map(move |level| (0..FACES).map(move |face| (level, face)))
            .filter_map(|(level, face)| self.face(level, face))
    }

    /// Bytes every face of every level together occupies.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.faces().map(|face| face.bytes).sum()
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`EnvironmentError`] for anything that is not a cooked environment of this
    /// version, face count and storable format, or is shorter than its header
    /// claims.
    pub fn read(bytes: &[u8]) -> Result<Self, EnvironmentError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(EnvironmentError::NotAnEnvironment {
                expected: "SLOPENV0",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(EnvironmentError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let size = read_u32(bytes, 12);
        let mip_levels = read_u32(bytes, 16);
        let code = read_u32(bytes, 20);
        let faces = read_u32(bytes, 24);

        if faces != FACES {
            return Err(EnvironmentError::Faces {
                expected: FACES,
                found: faces,
            });
        }

        // Refused rather than accepted and stored badly. A block-compressed
        // environment is not a thing that can be prefiltered, and an eight-bit
        // one is not high dynamic range — so both are the wrong artifact under
        // the right magic, which is worth a named error.
        let format = match Format::from_code(code) {
            Some(Format::Rgba16Float) => Format::Rgba16Float,
            _ => return Err(EnvironmentError::UnknownFormat { code }),
        };

        if size == 0 {
            return Err(EnvironmentError::Empty { size });
        }

        // Checked before it drives the walk below, so a corrupt count cannot
        // spend a long time summing levels that cannot exist.
        let possible = full_mip_chain(size, size);
        if mip_levels == 0 || mip_levels > possible {
            return Err(EnvironmentError::MipLevels {
                found: mip_levels,
                possible,
                size,
            });
        }

        // Built without its texels so `payload_bytes` can walk the chain, then
        // filled in — the alternative is duplicating the walk here.
        let mut environment = Self {
            size,
            mip_levels,
            format,
            texels: Vec::new(),
        };

        let expected = HEADER + environment.payload_bytes();
        if bytes.len() < expected {
            return Err(EnvironmentError::Truncated {
                expected,
                found: bytes.len(),
            });
        }

        environment.texels = bytes[HEADER..expected].to_vec();

        Ok(environment)
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment of `size` with a full chain and plausible texel bytes.
    fn chained(size: u32) -> Environment {
        let mut environment = Environment {
            size,
            mip_levels: full_mip_chain(size, size),
            format: Format::Rgba16Float,
            texels: Vec::new(),
        };
        environment.texels = (0..environment.payload_bytes())
            .map(|index| index as u8)
            .collect();

        environment
    }

    #[test]
    fn an_environment_round_trips() {
        let environment = chained(4);

        assert_eq!(Environment::read(&environment.write()), Ok(environment));
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = chained(4).write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_u32(&bytes, 12), 4, "size");
        assert_eq!(read_u32(&bytes, 16), 3, "4, 2, 1");
        assert_eq!(read_u32(&bytes, 20), 2, "Rgba16Float");
        assert_eq!(read_u32(&bytes, 24), FACES);

        // Where the payload starts, which is the one thing `HEADER` claims and
        // the field offsets above do not. A `HEADER` that disagrees with the
        // fields reads every texel four bytes late — which decodes without an
        // error and renders as noise.
        assert_eq!(bytes.len(), HEADER + chained(4).payload_bytes());
    }

    #[test]
    fn faces_tile_the_payload_without_gaps_or_overlap() {
        // The property `face` exists to guarantee: every face of every level sits
        // end to end, so one copy region per face covers exactly the payload. An
        // off-by-one here uploads one face's texels into another, which renders
        // as an environment with two sides swapped — plausible, and not a thing
        // any reference image would catch.
        for size in [64, 8, 2, 1] {
            let environment = chained(size);

            let mut expected_offset = 0;
            for face in environment.faces() {
                assert_eq!(face.offset, expected_offset, "gap at size {size}");
                expected_offset += face.bytes;
            }

            assert_eq!(
                expected_offset,
                environment.texels.len(),
                "size {size} must cover the payload exactly"
            );
        }
    }

    #[test]
    fn every_level_holds_six_faces_of_the_same_size() {
        let environment = chained(8);

        for level in 0..environment.mip_levels {
            let sizes: Vec<u32> = (0..FACES)
                .map(|face| {
                    environment
                        .face(level, face)
                        .expect("every face of every level exists")
                        .size
                })
                .collect();

            assert_eq!(sizes.len(), 6);
            assert!(sizes.iter().all(|size| *size == sizes[0]));
        }
    }

    #[test]
    fn a_face_past_the_end_is_none_rather_than_wrapping() {
        // Six faces, not seven, and asking for the seventh must not silently
        // return the first of the next level.
        let environment = chained(4);

        assert!(environment.face(0, FACES).is_none());
        assert!(environment.face(environment.mip_levels, 0).is_none());
    }

    #[test]
    fn a_level_is_a_quarter_of_the_one_above_it() {
        // The arithmetic that makes the whole chain about a third more than
        // level zero. If a level were sized from the wrong edge the payload
        // would still tile, and only this would notice.
        let environment = chained(16);

        let base = environment.face(0, 0).expect("level zero");
        let next = environment.face(1, 0).expect("level one");

        assert_eq!(base.bytes, next.bytes * 4);
        assert_eq!(next.size, 8);
    }

    #[test]
    fn a_truncated_environment_is_refused() {
        let environment = chained(8);
        let mut bytes = environment.write();
        bytes.truncate(bytes.len() - 8);

        assert!(matches!(
            Environment::read(&bytes),
            Err(EnvironmentError::Truncated { .. })
        ));
    }

    #[test]
    fn a_chain_missing_its_smaller_levels_is_caught() {
        // Level zero present, the rest absent. Checking only level zero's size
        // would accept this and leave the rest of the chain uninitialised —
        // which renders as a rough surface reading whatever was in memory.
        let environment = chained(16);
        let mut bytes = environment.write();
        bytes.truncate(HEADER + environment.face(0, 0).expect("level zero").bytes * 6);

        assert!(matches!(
            Environment::read(&bytes),
            Err(EnvironmentError::Truncated { .. })
        ));
    }

    #[test]
    fn something_that_is_not_an_environment_is_refused() {
        let failure = Environment::read(b"SLOPTEX0 and then some")
            .expect_err("a cooked texture is not a cooked environment");

        assert!(matches!(failure, EnvironmentError::NotAnEnvironment { .. }));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Environment::read(&[]).is_err());
        assert!(Environment::read(b"SLOP").is_err());
    }

    #[test]
    fn a_different_version_says_so() {
        let mut bytes = chained(4).write();
        bytes[8] = 42;

        assert!(matches!(
            Environment::read(&bytes),
            Err(EnvironmentError::Version { found: 42, .. })
        ));
    }

    #[test]
    fn a_format_an_environment_cannot_use_is_refused() {
        // `Rgba8` is a perfectly good texture format and a meaningless
        // environment: it has no dynamic range at all. Accepting it would upload
        // a quarter of the bytes the header implies.
        let mut bytes = chained(4).write();
        bytes[20] = 0;

        assert!(matches!(
            Environment::read(&bytes),
            Err(EnvironmentError::UnknownFormat { code: 0 })
        ));
    }

    #[test]
    fn a_face_count_that_is_not_six_is_refused() {
        // What makes a future non-cube environment a refused artifact rather
        // than one read as a cube with the wrong stride.
        let mut bytes = chained(4).write();
        bytes[24] = 1;

        assert!(matches!(
            Environment::read(&bytes),
            Err(EnvironmentError::Faces { found: 1, .. })
        ));
    }

    #[test]
    fn more_levels_than_the_size_allows_is_rejected() {
        let mut environment = chained(4);
        environment.mip_levels = 9;

        assert!(matches!(
            Environment::read(&environment.write()),
            Err(EnvironmentError::MipLevels { possible: 3, .. })
        ));
    }

    #[test]
    fn zero_levels_is_rejected() {
        let mut environment = chained(4);
        environment.mip_levels = 0;

        assert!(matches!(
            Environment::read(&environment.write()),
            Err(EnvironmentError::MipLevels { found: 0, .. })
        ));
    }

    #[test]
    fn a_zero_size_is_refused_before_the_arithmetic_runs() {
        let environment = Environment {
            size: 0,
            mip_levels: 1,
            format: Format::Rgba16Float,
            texels: Vec::new(),
        };

        assert!(matches!(
            Environment::read(&environment.write()),
            Err(EnvironmentError::Empty { size: 0 })
        ));
    }
}
