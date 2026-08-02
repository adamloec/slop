//! The cooked material format.
//!
//! What a surface looks like: its colour, how metallic and how rough it is, and
//! which textures supply those per-texel. One material is what a draw call binds
//! besides its geometry.
//!
//! # Metallic-roughness, because glTF is
//!
//! `docs/DESIGN.md` §2.8 makes glTF the import format, and glTF's material model
//! is metallic-roughness physically-based rendering: a base colour, a metallic
//! factor, a roughness factor, and optional textures modulating each. Adopting a
//! different parameterisation would mean converting on import and losing
//! information in the process, which is the one thing an asset pipeline must not
//! do.
//!
//! # Textures are named, not embedded
//!
//! A material references its textures by logical path, so two materials sharing
//! a normal map share the artifact rather than each carrying a copy. It also
//! means a material is small — a few hundred bytes — and can be reloaded without
//! touching the textures it names.
//!
//! # Layout
//!
//! ```text
//! magic          8 bytes  "SLOPMATL"
//! version        u32      VERSION
//! base_color     4 × f32  linear RGBA multiplier
//! metallic       f32
//! roughness      f32
//! emissive       3 × f32  linear RGB
//! alpha_cutoff   f32
//! flags          u32      see `Flags`
//! texture_count  u32
//! textures                count × { slot u32, length u32, path bytes }
//! ```
//!
//! Little-endian and decoded field by field, for the reasons
//! [`mesh`](crate::mesh) gives. Paths are UTF-8 and length-prefixed rather than
//! terminated, so a path containing anything at all is unambiguous.

use thiserror::Error;

/// The first eight bytes of every cooked material.
const MAGIC: &[u8; 8] = b"SLOPMATL";

/// What this module knows how to read.
pub const VERSION: u32 = 1;

/// Bytes before the texture list.
const HEADER: usize = 60;

/// Why a cooked material could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MaterialError {
    /// The bytes are not a cooked material.
    #[error("not a cooked material: expected magic {expected:?}, found {found:?}")]
    NotAMaterial {
        /// What every cooked material starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// A cooked material from a different version of the format.
    #[error("cooked material is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// A texture slot this build does not know.
    #[error("cooked material uses texture slot {code}, which this build does not know")]
    UnknownSlot {
        /// The discriminant in the file.
        code: u32,
    },

    /// A texture path is not UTF-8.
    #[error("a texture path in the material is not valid UTF-8")]
    NotUtf8,

    /// The file ends before it says it should.
    #[error("cooked material is truncated: needed {expected} bytes, {found} present")]
    Truncated {
        /// How many bytes were needed at that point.
        expected: usize,
        /// How many there are.
        found: usize,
    },
}

/// What a texture contributes to a surface.
///
/// Only the four glTF supplies that this pipeline reads. Occlusion is deliberately
/// absent: it is a *baked* term that a real-time renderer computes or ignores, and
/// carrying it before something consumes it would be storing a number nothing
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TextureSlot {
    /// Albedo, multiplied by [`Material::base_color`]. sRGB-encoded on disk.
    BaseColor,
    /// Blue channel is metallic, green is roughness — glTF's packing.
    MetallicRoughness,
    /// Tangent-space normals.
    Normal,
    /// Light this surface emits, multiplied by [`Material::emissive`].
    Emissive,
}

impl TextureSlot {
    /// The discriminant written into the file.
    const fn code(self) -> u32 {
        match self {
            Self::BaseColor => 0,
            Self::MetallicRoughness => 1,
            Self::Normal => 2,
            Self::Emissive => 3,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::BaseColor),
            1 => Some(Self::MetallicRoughness),
            2 => Some(Self::Normal),
            3 => Some(Self::Emissive),
            _ => None,
        }
    }

    /// Whether the texture's bytes are sRGB-encoded rather than linear.
    ///
    /// The distinction that goes wrong silently. Colour textures are authored in
    /// sRGB and must be converted to linear before lighting maths; normals and
    /// packed metallic-roughness are *data* and must not be. Sampling a normal
    /// map through an sRGB view bends every normal toward the surface and looks
    /// like a lighting bug rather than a format one.
    pub const fn is_srgb(self) -> bool {
        match self {
            Self::BaseColor | Self::Emissive => true,
            Self::MetallicRoughness | Self::Normal => false,
        }
    }
}

/// How a surface's alpha is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// Fully opaque; alpha is ignored entirely.
    #[default]
    Opaque,
    /// A fragment is drawn or discarded by comparing against
    /// [`Material::alpha_cutoff`]. What foliage and chain-link use.
    Mask,
    /// Blended against what is behind it. Needs sorting, which nothing does yet.
    Blend,
}

impl AlphaMode {
    const fn bits(self) -> u32 {
        match self {
            Self::Opaque => 0,
            Self::Mask => 1,
            Self::Blend => 2,
        }
    }

    fn from_bits(bits: u32) -> Option<Self> {
        match bits {
            0 => Some(Self::Opaque),
            1 => Some(Self::Mask),
            2 => Some(Self::Blend),
            _ => None,
        }
    }
}

/// Bit positions in the header's `flags` field.
mod flags {
    /// Alpha mode, two bits wide.
    pub(super) const ALPHA_MODE: u32 = 0b11;
    /// Set when back faces must not be culled.
    pub(super) const DOUBLE_SIDED: u32 = 1 << 2;
}

/// A material, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    /// Linear RGBA multiplier over the base colour texture.
    pub base_color: [f32; 4],
    /// Zero for a dielectric, one for a metal.
    pub metallic: f32,
    /// Zero for a mirror, one for fully diffuse.
    pub roughness: f32,
    /// Linear RGB light this surface emits.
    pub emissive: [f32; 3],
    /// Below this alpha a fragment is discarded, under [`AlphaMode::Mask`].
    pub alpha_cutoff: f32,
    /// How alpha is interpreted.
    pub alpha_mode: AlphaMode,
    /// Whether back faces are drawn. Foliage cards need this.
    pub double_sided: bool,
    /// Logical paths of the textures this material samples, by slot.
    ///
    /// Sorted by slot, so a cooked material is a fixed point: cooking the same
    /// source twice produces identical bytes whatever order the importer
    /// discovered them in.
    pub textures: Vec<(TextureSlot, String)>,
}

impl Default for Material {
    /// glTF's defaults, which are also sensible ones.
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            metallic: 1.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            alpha_cutoff: 0.5,
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            textures: Vec::new(),
        }
    }
}

impl Material {
    /// The logical path of one slot's texture, if the material has one.
    pub fn texture(&self, slot: TextureSlot) -> Option<&str> {
        self.textures
            .iter()
            .find(|(candidate, _)| *candidate == slot)
            .map(|(_, path)| path.as_str())
    }

    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER);

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());

        for channel in self.base_color {
            out.extend_from_slice(&channel.to_le_bytes());
        }

        out.extend_from_slice(&self.metallic.to_le_bytes());
        out.extend_from_slice(&self.roughness.to_le_bytes());

        for channel in self.emissive {
            out.extend_from_slice(&channel.to_le_bytes());
        }

        out.extend_from_slice(&self.alpha_cutoff.to_le_bytes());

        let mut bits = self.alpha_mode.bits();
        if self.double_sided {
            bits |= flags::DOUBLE_SIDED;
        }
        out.extend_from_slice(&bits.to_le_bytes());

        debug_assert_eq!(out.len(), HEADER - 4, "the header layout moved");

        out.extend_from_slice(&(self.textures.len() as u32).to_le_bytes());

        // Sorted, so identical inputs cook to identical bytes whatever order the
        // importer walked them in — which is what lets the content hash decide
        // staleness rather than merely detect it.
        let mut textures = self.textures.clone();
        textures.sort_by_key(|(slot, _)| *slot);

        for (slot, path) in &textures {
            out.extend_from_slice(&slot.code().to_le_bytes());
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
        }

        out
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`MaterialError`] for anything that is not a cooked material of this
    /// version, names a slot this build does not know, or ends early.
    pub fn read(bytes: &[u8]) -> Result<Self, MaterialError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(MaterialError::NotAMaterial {
                expected: "SLOPMATL",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(MaterialError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let base_color = [
            read_f32(bytes, 12),
            read_f32(bytes, 16),
            read_f32(bytes, 20),
            read_f32(bytes, 24),
        ];
        let metallic = read_f32(bytes, 28);
        let roughness = read_f32(bytes, 32);
        let emissive = [
            read_f32(bytes, 36),
            read_f32(bytes, 40),
            read_f32(bytes, 44),
        ];
        let alpha_cutoff = read_f32(bytes, 48);
        let bits = read_u32(bytes, 52);
        let count = read_u32(bytes, 56) as usize;

        let alpha_mode =
            AlphaMode::from_bits(bits & flags::ALPHA_MODE).ok_or(MaterialError::UnknownSlot {
                code: bits & flags::ALPHA_MODE,
            })?;

        let mut textures = Vec::with_capacity(count);
        let mut at = HEADER;

        for _ in 0..count {
            let needed = at + 8;
            if bytes.len() < needed {
                return Err(MaterialError::Truncated {
                    expected: needed,
                    found: bytes.len(),
                });
            }

            let code = read_u32(bytes, at);
            let length = read_u32(bytes, at + 4) as usize;
            at += 8;

            if bytes.len() < at + length {
                return Err(MaterialError::Truncated {
                    expected: at + length,
                    found: bytes.len(),
                });
            }

            let slot = TextureSlot::from_code(code).ok_or(MaterialError::UnknownSlot { code })?;
            let path = std::str::from_utf8(&bytes[at..at + length])
                .map_err(|_| MaterialError::NotUtf8)?
                .to_owned();

            at += length;
            textures.push((slot, path));
        }

        Ok(Self {
            base_color,
            metallic,
            roughness,
            emissive,
            alpha_cutoff,
            alpha_mode,
            double_sided: bits & flags::DOUBLE_SIDED != 0,
            textures,
        })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn read_f32(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Material {
        Material {
            base_color: [0.8, 0.2, 0.1, 1.0],
            metallic: 0.0,
            roughness: 0.7,
            emissive: [0.0, 0.0, 0.5],
            alpha_cutoff: 0.25,
            alpha_mode: AlphaMode::Mask,
            double_sided: true,
            textures: vec![
                (TextureSlot::BaseColor, String::from("textures/wall.tex")),
                (TextureSlot::Normal, String::from("textures/wall_n.tex")),
            ],
        }
    }

    #[test]
    fn a_material_round_trips() {
        let material = sample();

        assert_eq!(Material::read(&material.write()), Ok(material));
    }

    #[test]
    fn the_default_material_round_trips() {
        let material = Material::default();

        assert_eq!(Material::read(&material.write()), Ok(material));
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = sample().write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_f32(&bytes, 12), 0.8, "base colour red");
        assert_eq!(read_f32(&bytes, 32), 0.7, "roughness");
        assert_eq!(read_u32(&bytes, 56), 2, "texture count");
    }

    #[test]
    fn textures_are_written_in_slot_order_whatever_order_they_arrive_in() {
        // The cook cache keys on content and assumes the cooker is a function of
        // its inputs. An importer that discovered textures in a different order
        // between runs would otherwise produce different bytes for one source.
        let mut shuffled = sample();
        shuffled.textures.reverse();

        assert_eq!(shuffled.write(), sample().write());
    }

    #[test]
    fn a_texture_is_found_by_slot() {
        let material = sample();

        assert_eq!(
            material.texture(TextureSlot::BaseColor),
            Some("textures/wall.tex")
        );
        assert_eq!(material.texture(TextureSlot::Emissive), None);
    }

    #[test]
    fn colour_textures_are_srgb_and_data_textures_are_not() {
        // The distinction that fails silently: sampling a normal map through an
        // sRGB view bends every normal toward the surface, which reads as a
        // lighting bug rather than a format one.
        assert!(TextureSlot::BaseColor.is_srgb());
        assert!(TextureSlot::Emissive.is_srgb());
        assert!(!TextureSlot::Normal.is_srgb());
        assert!(!TextureSlot::MetallicRoughness.is_srgb());
    }

    #[test]
    fn every_slot_survives_its_discriminant() {
        for slot in [
            TextureSlot::BaseColor,
            TextureSlot::MetallicRoughness,
            TextureSlot::Normal,
            TextureSlot::Emissive,
        ] {
            assert_eq!(TextureSlot::from_code(slot.code()), Some(slot));
        }
    }

    #[test]
    fn every_alpha_mode_survives_its_bits() {
        for mode in [AlphaMode::Opaque, AlphaMode::Mask, AlphaMode::Blend] {
            let material = Material {
                alpha_mode: mode,
                // Set too, because they share the flags word and a shifted mask
                // would let one corrupt the other.
                double_sided: true,
                ..Material::default()
            };

            let back = Material::read(&material.write()).expect("valid");

            assert_eq!(back.alpha_mode, mode);
            assert!(back.double_sided, "{mode:?} clobbered the double-sided bit");
        }
    }

    #[test]
    fn a_truncated_texture_list_is_refused() {
        let mut bytes = sample().write();
        bytes.truncate(bytes.len() - 3);

        assert!(matches!(
            Material::read(&bytes),
            Err(MaterialError::Truncated { .. })
        ));
    }

    #[test]
    fn a_count_that_lies_is_refused_rather_than_panicking() {
        // The shape of a fuzz crash: a header claiming more textures than the
        // file holds must not index past the end.
        let mut bytes = sample().write();
        bytes[56..60].copy_from_slice(&999_u32.to_le_bytes());

        assert!(matches!(
            Material::read(&bytes),
            Err(MaterialError::Truncated { .. })
        ));
    }

    #[test]
    fn an_unknown_slot_is_refused_rather_than_assumed() {
        let mut bytes = sample().write();
        bytes[60..64].copy_from_slice(&77_u32.to_le_bytes());

        assert!(matches!(
            Material::read(&bytes),
            Err(MaterialError::UnknownSlot { code: 77 })
        ));
    }

    #[test]
    fn something_that_is_not_a_material_is_refused() {
        assert!(matches!(
            Material::read(b"SLOPMESH\0\0\0\0"),
            Err(MaterialError::NotAMaterial { .. })
        ));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Material::read(&[]).is_err());
    }

    #[test]
    fn a_different_version_says_so_rather_than_decoding_garbage() {
        let mut bytes = sample().write();
        bytes[8..12].copy_from_slice(&99_u32.to_le_bytes());

        assert_eq!(
            Material::read(&bytes),
            Err(MaterialError::Version {
                expected: VERSION,
                found: 99,
            })
        );
    }

    #[test]
    fn the_encoding_is_little_endian_whatever_the_host_is() {
        let bytes = Material {
            metallic: 1.0,
            ..Material::default()
        }
        .write();

        // 1.0f32 is 0x3F800000, so a big-endian writer puts 0x3F first.
        assert_eq!(&bytes[28..32], &[0x00, 0x00, 0x80, 0x3F]);
    }
}
