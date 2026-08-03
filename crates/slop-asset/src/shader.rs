//! The cooked shader reflection format.
//!
//! What a shader *says about itself*: where its vertex inputs are, and how big
//! its push constant block is. Cooked beside the SPIR-V, from the same compile,
//! so the two can never describe different shaders.
//!
//! # Why this exists
//!
//! Without it, every fact about a shader is stated twice — once in the shader
//! and once in Rust — and nothing checks that the two agree. A field added to
//! the shader's `VertexIn` and not to the Rust attribute table does not fail to
//! compile; it makes the GPU read the previous vertex's data, and the symptom is
//! geometry that looks scrambled. `docs/PLAN.md` §6.1 carried that as a known
//! cost from M0 until this landed.
//!
//! # Layout
//!
//! ```text
//! magic          8 bytes  "SLOPREFL"
//! version        u32      VERSION
//! push_bytes     u32      size of the push constant block, 0 if there is none
//! input_count    u32      vertex inputs that follow
//! inputs                  input_count × { location u32, format u32 }
//! ```
//!
//! Little-endian and decoded field by field, for the reasons
//! [`mesh`](crate::mesh) gives.
//!
//! # What it deliberately does not carry
//!
//! **Byte offsets.** Reflection says a shader reads location 1 as three floats;
//! it does not say where in a vertex buffer that lives, because that is the
//! application's layout decision — interleaved or separate streams, packed or
//! padded. [`Reflection::interleaved`] computes the offsets for the one
//! convention the cooked mesh format uses, and keeps that choice visible at the
//! call site rather than baked into the artifact.
//!
//! **Descriptor bindings.** The bindless heap layout is fixed and shared by
//! every shader (`docs/DESIGN.md` §2.2), so there is nothing per-shader to
//! derive. Reflection *could* be used to check a shader agrees with the heap,
//! and `docs/PLAN.md` §6.1 records that as waiting for a second consumer.

use thiserror::Error;

/// The first eight bytes of every cooked reflection artifact.
const MAGIC: &[u8; 8] = b"SLOPREFL";

/// What this module knows how to read.
///
/// Version 2 added the compute thread-group size. Cooked artifacts regenerate
/// from source, so a format change costs a `COOKER_VERSION` bump and nothing
/// else — which is the whole reason the cooked formats are allowed to be rigid.
pub const VERSION: u32 = 2;

/// Bytes before the input array.
///
/// Magic (8), version (4), push constant bytes (4), input count (4), then the
/// three thread-group dimensions (12).
const HEADER: usize = 32;

/// Bytes per input entry.
const INPUT: usize = 8;

/// Why cooked reflection could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReflectionError {
    /// The bytes are not cooked reflection.
    #[error("not cooked reflection: expected magic {expected:?}, found {found:?}")]
    NotReflection {
        /// What every cooked reflection artifact starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// Cooked by a different version of the format.
    #[error("cooked reflection is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// A vertex format discriminant this build does not know.
    #[error(
        "vertex input at location {location} uses format {code}, which this build does not know"
    )]
    UnknownFormat {
        /// Which input.
        location: u32,
        /// The discriminant in the file.
        code: u32,
    },

    /// The file ends before it says it should.
    #[error("cooked reflection is truncated: {expected} bytes declared, {found} present")]
    Truncated {
        /// How many bytes the header implies.
        expected: usize,
        /// How many there are.
        found: usize,
    },
}

/// The type of one vertex input.
///
/// Only the float vectors, because that is what the shaders have. Integer
/// formats arrive with skinning, which needs joint indices — another variant and
/// a `COOKER_VERSION` bump, which is what cooking makes cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexFormat {
    /// One 32-bit float.
    Float32,
    /// Two.
    Float32x2,
    /// Three.
    Float32x3,
    /// Four.
    Float32x4,
}

impl VertexFormat {
    /// The discriminant written into the file.
    const fn code(self) -> u32 {
        match self {
            Self::Float32 => 0,
            Self::Float32x2 => 1,
            Self::Float32x3 => 2,
            Self::Float32x4 => 3,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Float32),
            1 => Some(Self::Float32x2),
            2 => Some(Self::Float32x3),
            3 => Some(Self::Float32x4),
            _ => None,
        }
    }

    /// How many bytes one of these occupies.
    pub const fn size(self) -> u32 {
        match self {
            Self::Float32 => 4,
            Self::Float32x2 => 8,
            Self::Float32x3 => 12,
            Self::Float32x4 => 16,
        }
    }

    /// How many float components it has.
    pub const fn components(self) -> u32 {
        self.size() / 4
    }

    /// The format for `components` floats, if there is one.
    pub const fn from_components(components: u32) -> Option<Self> {
        match components {
            1 => Some(Self::Float32),
            2 => Some(Self::Float32x2),
            3 => Some(Self::Float32x3),
            4 => Some(Self::Float32x4),
            _ => None,
        }
    }
}

/// One input a vertex shader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexInput {
    /// The shader's `location`, which is what a pipeline binds against.
    pub location: u32,
    /// What the shader expects to find there.
    pub format: VertexFormat,
}

/// One input, placed in an interleaved vertex buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedInput {
    /// The shader's `location`.
    pub location: u32,
    /// What the shader expects.
    pub format: VertexFormat,
    /// Bytes from the start of a vertex.
    pub offset: u32,
}

/// What a cooked shader says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reflection {
    /// Bytes the push constant block occupies. Zero if the shader has none.
    pub push_constant_bytes: u32,
    /// Vertex inputs, in ascending location order.
    pub vertex_inputs: Vec<VertexInput>,
    /// The compute entry point's `[numthreads(x, y, z)]`, or `None` for a shader
    /// with no compute stage.
    ///
    /// **Carried so a dispatch does not restate it.** A caller must divide the
    /// work by exactly these numbers and round up; naming them a second time in
    /// Rust means the two can disagree, and disagreeing dispatches too few groups
    /// and silently leaves the tail of the work undone. That is not a crash — it
    /// is a cropped image or a half-filled buffer.
    pub thread_group: Option<[u32; 3]>,
}

impl Reflection {
    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.vertex_inputs.len() * INPUT);

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.push_constant_bytes.to_le_bytes());
        out.extend_from_slice(&(self.vertex_inputs.len() as u32).to_le_bytes());

        // All zeroes when there is no compute stage. A zero-sized workgroup is
        // meaningless, so it doubles as the absent marker without costing a
        // separate flag.
        for axis in self.thread_group.unwrap_or([0; 3]) {
            out.extend_from_slice(&axis.to_le_bytes());
        }

        for input in &self.vertex_inputs {
            out.extend_from_slice(&input.location.to_le_bytes());
            out.extend_from_slice(&input.format.code().to_le_bytes());
        }

        out
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`ReflectionError`] for anything that is not cooked reflection of this
    /// version, or is shorter than its header claims.
    pub fn read(bytes: &[u8]) -> Result<Self, ReflectionError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(ReflectionError::NotReflection {
                expected: "SLOPREFL",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(ReflectionError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let push_constant_bytes = read_u32(bytes, 12);
        let count = read_u32(bytes, 16) as usize;

        let thread_group = [
            read_u32(bytes, 20),
            read_u32(bytes, 24),
            read_u32(bytes, 28),
        ];
        // Any zero axis means no compute stage — see `write`. A partially zero
        // group would be a cooker bug rather than a shader, so it reads as
        // absent rather than being trusted.
        let thread_group = thread_group
            .iter()
            .all(|axis| *axis > 0)
            .then_some(thread_group);

        let expected = HEADER + count * INPUT;
        if bytes.len() < expected {
            return Err(ReflectionError::Truncated {
                expected,
                found: bytes.len(),
            });
        }

        let mut vertex_inputs = Vec::with_capacity(count);

        for index in 0..count {
            let at = HEADER + index * INPUT;
            let location = read_u32(bytes, at);
            let code = read_u32(bytes, at + 4);

            let format = VertexFormat::from_code(code)
                .ok_or(ReflectionError::UnknownFormat { location, code })?;

            vertex_inputs.push(VertexInput { location, format });
        }

        Ok(Self {
            push_constant_bytes,
            vertex_inputs,
            thread_group,
        })
    }

    /// How many workgroups cover `extent` items along `axis`.
    ///
    /// Integer ceiling division against the shader's own declared group size, so
    /// a dispatch cannot disagree with `[numthreads(..)]` — the disagreement
    /// this field exists to prevent.
    ///
    /// `None` when the shader has no compute stage, or `axis` is not 0, 1 or 2.
    #[must_use]
    pub fn workgroups(&self, axis: usize, extent: u32) -> Option<u32> {
        let group = *self.thread_group?.get(axis)?;

        // Written out rather than `(extent + group - 1) / group`, which overflows
        // for an extent near `u32::MAX`.
        Some(extent / group + u32::from(!extent.is_multiple_of(group)))
    }

    /// Place every input in one tightly packed, interleaved vertex.
    ///
    /// The convention the cooked mesh format uses: attributes in ascending
    /// location order, no padding between them, one buffer. Computed here rather
    /// than stored, because it is a decision about the *buffer* rather than a
    /// fact about the shader — a renderer feeding separate streams would place
    /// the same inputs differently and be equally correct.
    ///
    /// Returns the placed inputs and the stride.
    pub fn interleaved(&self) -> (Vec<PlacedInput>, u32) {
        let mut offset = 0;
        let mut placed = Vec::with_capacity(self.vertex_inputs.len());

        for input in &self.vertex_inputs {
            placed.push(PlacedInput {
                location: input.location,
                format: input.format,
                offset,
            });

            offset += input.format.size();
        }

        (placed, offset)
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Reflection {
        Reflection {
            push_constant_bytes: 136,
            vertex_inputs: vec![
                VertexInput {
                    location: 0,
                    format: VertexFormat::Float32x3,
                },
                VertexInput {
                    location: 1,
                    format: VertexFormat::Float32x3,
                },
                VertexInput {
                    location: 2,
                    format: VertexFormat::Float32x2,
                },
            ],
            // A graphics shader, so no compute stage.
            thread_group: None,
        }
    }

    /// A compute shader's reflection: no vertex inputs, a thread group.
    fn compute_sample() -> Reflection {
        Reflection {
            push_constant_bytes: 16,
            vertex_inputs: Vec::new(),
            thread_group: Some([8, 8, 1]),
        }
    }

    #[test]
    fn a_thread_group_round_trips() {
        let reflection = compute_sample();

        assert_eq!(Reflection::read(&reflection.write()), Ok(reflection));
    }

    /// Absence is spelled as zeroes and must come back as `None`, not as a group
    /// of zero — which would divide by zero, or dispatch nothing and look fine.
    #[test]
    fn no_compute_stage_round_trips_as_absent() {
        let bytes = sample().write();

        assert_eq!(
            Reflection::read(&bytes).expect("valid").thread_group,
            None,
            "a graphics shader must report no thread group rather than zeroes"
        );
    }

    #[test]
    fn workgroups_rounds_up_against_the_shaders_own_size() {
        let reflection = compute_sample();

        // Exact, and one texel over — the case rounding down drops.
        assert_eq!(reflection.workgroups(0, 16), Some(2));
        assert_eq!(reflection.workgroups(0, 17), Some(3));
        assert_eq!(reflection.workgroups(0, 0), Some(0));

        // The z axis is 1, so every extent needs that many groups.
        assert_eq!(reflection.workgroups(2, 5), Some(5));

        // No fourth axis, and no compute stage at all.
        assert_eq!(reflection.workgroups(3, 8), None);
        assert_eq!(sample().workgroups(0, 8), None);
    }

    /// The overflow the naive `(extent + group - 1) / group` has: it wraps and
    /// dispatches nothing for the largest possible extent.
    #[test]
    fn an_extent_near_the_maximum_does_not_overflow() {
        let reflection = compute_sample();

        assert_eq!(reflection.workgroups(2, u32::MAX), Some(u32::MAX));
        assert_eq!(reflection.workgroups(0, u32::MAX), Some(536_870_912));
    }

    #[test]
    fn reflection_round_trips() {
        let reflection = sample();

        assert_eq!(Reflection::read(&reflection.write()), Ok(reflection));
    }

    #[test]
    fn a_shader_with_no_inputs_round_trips() {
        // The triangle: positions come from SV_VertexID, so there is nothing to
        // bind and nothing to place.
        let reflection = Reflection::default();

        assert_eq!(Reflection::read(&reflection.write()), Ok(reflection));
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = sample().write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_u32(&bytes, 12), 136, "push constant bytes");
        assert_eq!(read_u32(&bytes, 16), 3, "input count");
        assert_eq!(bytes.len(), HEADER + 3 * INPUT);
    }

    #[test]
    fn interleaving_packs_in_location_order() {
        // The layout the cooked mesh format produces: position, normal, uv, with
        // nothing between them.
        let (placed, stride) = sample().interleaved();

        assert_eq!(stride, 32);
        assert_eq!(placed[0].offset, 0);
        assert_eq!(placed[1].offset, 12);
        assert_eq!(placed[2].offset, 24);
    }

    #[test]
    fn interleaving_nothing_gives_a_zero_stride() {
        let (placed, stride) = Reflection::default().interleaved();

        assert!(placed.is_empty());
        assert_eq!(stride, 0);
    }

    #[test]
    fn a_truncated_input_array_is_refused() {
        let mut bytes = sample().write();
        bytes.truncate(bytes.len() - 1);

        assert!(matches!(
            Reflection::read(&bytes),
            Err(ReflectionError::Truncated { .. })
        ));
    }

    #[test]
    fn something_that_is_not_reflection_is_refused() {
        assert!(matches!(
            Reflection::read(b"SLOPMESH\0\0\0\0"),
            Err(ReflectionError::NotReflection { .. })
        ));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Reflection::read(&[]).is_err());
    }

    #[test]
    fn a_different_version_says_so_rather_than_decoding_garbage() {
        let mut bytes = sample().write();
        bytes[8..12].copy_from_slice(&99_u32.to_le_bytes());

        assert_eq!(
            Reflection::read(&bytes),
            Err(ReflectionError::Version {
                expected: VERSION,
                found: 99,
            })
        );
    }

    #[test]
    fn an_unknown_vertex_format_is_refused_rather_than_assumed() {
        let mut bytes = sample().write();
        bytes[HEADER + 4..HEADER + 8].copy_from_slice(&77_u32.to_le_bytes());

        assert!(matches!(
            Reflection::read(&bytes),
            Err(ReflectionError::UnknownFormat { code: 77, .. })
        ));
    }

    #[test]
    fn the_encoding_is_little_endian_whatever_the_host_is() {
        let bytes = sample().write();

        // 136 = 0x88, so a big-endian writer would put the byte last.
        assert_eq!(&bytes[12..16], &[0x88, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn every_format_survives_its_discriminant() {
        for format in [
            VertexFormat::Float32,
            VertexFormat::Float32x2,
            VertexFormat::Float32x3,
            VertexFormat::Float32x4,
        ] {
            assert_eq!(VertexFormat::from_code(format.code()), Some(format));
            assert_eq!(
                VertexFormat::from_components(format.components()),
                Some(format)
            );
        }
    }
}
