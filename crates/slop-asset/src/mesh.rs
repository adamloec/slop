//! The cooked mesh format.
//!
//! What a glTF import produces and what the engine loads. Binary rather than
//! text, unlike the world format in `slop-ecs` — nobody reads a vertex buffer,
//! and a hundred thousand of them written as `1.0,` would be neither diffable
//! nor loadable at speed.
//!
//! # Why the format lives here
//!
//! Both sides have to agree on it: `slop-cli` writes it and the engine reads it.
//! `docs/DESIGN.md` §4 gives this crate the "source→cooked pipeline", and what
//! cooked *looks like* is the pipeline's data contract.
//!
//! This is not a contradiction of [`Vfs`](crate::Vfs) knowing nothing about
//! formats. The VFS deals in bytes and stays that way; the format is a separate
//! module that happens to share the crate, and nothing in the read path is aware
//! of it.
//!
//! # Layout
//!
//! ```text
//! magic      8 bytes  "SLOPMESH"
//! version    u32      VERSION
//! vertices   u32      count
//! indices    u32      count
//! material   u32      length of the material path, zero if there is none
//! vertex data         vertices × 48 bytes
//! index data          indices × 4 bytes
//! ```
//!
//! **Little-endian, decoded field by field.** Two reasons, and the first is
//! forced: `std::fs::read` returns a `Vec<u8>` aligned to 1, so casting it to a
//! `&[Vertex]` would be undefined behaviour on a buffer that happens to land
//! oddly. The second is that decoding explicitly makes the byte order a property
//! of the format rather than of whichever machine cooked it — cooked artifacts
//! live in a per-machine cache today and will live in a shipped archive later.
//!
//! Zero-copy loading is worth having and needs an aligned buffer to be sound, so
//! it arrives with the streaming loader that will own one
//! (`docs/PLAN.md` §6.1).

use thiserror::Error;

/// The first eight bytes of every cooked mesh.
const MAGIC: &[u8; 8] = b"SLOPMESH";

/// What this module knows how to read.
///
/// Bump when the layout changes. Every artifact then fails its stamp and
/// regenerates from source, which is the whole reason cooking is a build step.
pub const VERSION: u32 = 3;

/// Bytes before the vertex data.
const HEADER: usize = 24;

/// Bytes per vertex.
pub const VERTEX_SIZE: usize = 48;

/// Why a cooked mesh could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MeshError {
    /// The material path is not valid UTF-8.
    #[error("the material path in the mesh is not valid UTF-8")]
    MaterialNotUtf8,

    /// The bytes are not a cooked mesh.
    #[error("not a cooked mesh: expected magic {expected:?}, found {found:?}")]
    NotAMesh {
        /// What every cooked mesh starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// A cooked mesh from a different version of the format.
    ///
    /// Should not survive a cook, since the cooker's version keys the cache —
    /// but a stale artifact copied in by hand should say what it is rather than
    /// being decoded as garbage.
    #[error("cooked mesh is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// The file ends before it says it should.
    #[error("cooked mesh is truncated: {expected} bytes declared, {found} present")]
    Truncated {
        /// How many bytes the header implies.
        expected: usize,
        /// How many there are.
        found: usize,
    },

    /// An index names a vertex that does not exist.
    ///
    /// Checked on load rather than trusted. A cooked artifact is something we
    /// produced, but a corrupt cache entry that reaches the GPU is a hang or a
    /// garbage draw with nothing to point at — one linear scan at load time is
    /// cheap by comparison.
    #[error("index {index} names vertex {vertex}, but the mesh has {vertices}")]
    IndexOutOfRange {
        /// Position in the index buffer.
        index: usize,
        /// The vertex it named.
        vertex: u32,
        /// How many vertices there are.
        vertices: u32,
    },
}

/// One vertex.
///
/// **One layout, for every mesh.** Every shader that draws cooked geometry must
/// declare all of these, because the vertex layout is derived from shader
/// reflection and a shader that omits a field computes a stride shorter than the
/// buffer's — reading every vertex after the first from the middle of its
/// predecessor. The cube declares a tangent it never samples for exactly this
/// reason.
///
/// The alternative is per-mesh layouts and pipeline variants to match, which is
/// a real requirement for skinned and instanced geometry and is not one yet;
/// `docs/PLAN.md` §6.1 records why it waits and why it is cheap to add.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    /// Object-space position.
    pub position: [f32; 3],
    /// Object-space normal. Unit length.
    pub normal: [f32; 3],
    /// Texture coordinate, origin top-left.
    pub uv: [f32; 2],
    /// Object-space tangent, with handedness in `w`.
    ///
    /// A normal map stores directions in **tangent space** — a per-vertex frame
    /// aligned to the texture's axes — so sampling one means knowing that frame.
    /// The normal gives one axis; this gives a second; the third is
    /// `cross(normal, tangent) * w`.
    ///
    /// `w` is `+1` or `-1` and is not decoration. It records whether the texture
    /// is mirrored across this triangle, which is extremely common — an artist
    /// UV-maps half a symmetrical model and flips the other half to halve the
    /// texture budget. Dropping it inverts the bitangent on every mirrored
    /// surface, and a normal map applied with an inverted bitangent lights those
    /// surfaces as though from the opposite side.
    ///
    /// Zero when the source had no tangents and none could be derived, which
    /// means "no tangent frame" rather than "a degenerate one" — see
    /// [`Vertex::has_tangent`].
    pub tangent: [f32; 4],
}

impl Vertex {
    /// Whether this vertex carries a usable tangent frame.
    ///
    /// A shader must check rather than assume: a mesh whose source had no
    /// tangents and whose UVs are degenerate gets a zero tangent, and
    /// normalising that produces NaN, which propagates into the lit colour and
    /// shows as black or white pixels rather than as an obviously wrong normal.
    #[must_use]
    pub fn has_tangent(&self) -> bool {
        let [x, y, z, _] = self.tangent;

        x != 0.0 || y != 0.0 || z != 0.0
    }
}

/// A mesh, decoded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    /// Vertices, in the order the index buffer refers to them.
    pub vertices: Vec<Vertex>,
    /// Triangle indices, three per triangle.
    pub indices: Vec<u32>,
    /// Logical path of the material this primitive is drawn with.
    ///
    /// `None` for a primitive that names none, which glTF permits and which
    /// means "the default material" rather than "no material".
    ///
    /// Carried by the mesh because glTF puts it there: a primitive *has* a
    /// material, and separating them would mean inventing a third artifact to
    /// record a pairing the source already states. A scene that wants to
    /// override it still can — this is the default, not a binding.
    pub material: Option<String>,
}

impl Mesh {
    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEADER + self.vertices.len() * VERTEX_SIZE + self.indices.len() * 4);

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.vertices.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.indices.len() as u32).to_le_bytes());
        let material = self.material.as_deref().unwrap_or_default();
        out.extend_from_slice(&(material.len() as u32).to_le_bytes());

        for vertex in &self.vertices {
            for value in vertex
                .position
                .iter()
                .chain(&vertex.normal)
                .chain(&vertex.uv)
                .chain(&vertex.tangent)
            {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }

        for index in &self.indices {
            out.extend_from_slice(&index.to_le_bytes());
        }

        // After the geometry, so vertex data stays at a fixed offset — the
        // zero-copy read `docs/PLAN.md` §6.1 anticipates needs that.
        out.extend_from_slice(material.as_bytes());

        out
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`MeshError`] for anything that is not a cooked mesh of this version, is
    /// shorter than its header claims, or indexes a vertex it does not have.
    pub fn read(bytes: &[u8]) -> Result<Self, MeshError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(MeshError::NotAMesh {
                expected: "SLOPMESH",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(MeshError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let vertex_count = read_u32(bytes, 12) as usize;
        let index_count = read_u32(bytes, 16) as usize;
        let material_length = read_u32(bytes, 20) as usize;

        let expected = HEADER + vertex_count * VERTEX_SIZE + index_count * 4 + material_length;
        if bytes.len() < expected {
            return Err(MeshError::Truncated {
                expected,
                found: bytes.len(),
            });
        }

        let mut vertices = Vec::with_capacity(vertex_count);
        for index in 0..vertex_count {
            let at = HEADER + index * VERTEX_SIZE;

            vertices.push(Vertex {
                position: [
                    read_f32(bytes, at),
                    read_f32(bytes, at + 4),
                    read_f32(bytes, at + 8),
                ],
                normal: [
                    read_f32(bytes, at + 12),
                    read_f32(bytes, at + 16),
                    read_f32(bytes, at + 20),
                ],
                uv: [read_f32(bytes, at + 24), read_f32(bytes, at + 28)],
                tangent: [
                    read_f32(bytes, at + 32),
                    read_f32(bytes, at + 36),
                    read_f32(bytes, at + 40),
                    read_f32(bytes, at + 44),
                ],
            });
        }

        let index_base = HEADER + vertex_count * VERTEX_SIZE;
        let mut indices = Vec::with_capacity(index_count);
        for index in 0..index_count {
            let vertex = read_u32(bytes, index_base + index * 4);

            if vertex as usize >= vertex_count {
                return Err(MeshError::IndexOutOfRange {
                    index,
                    vertex,
                    vertices: vertex_count as u32,
                });
            }

            indices.push(vertex);
        }

        let at = HEADER + vertex_count * VERTEX_SIZE + index_count * 4;
        let material = match material_length {
            0 => None,
            length => Some(
                std::str::from_utf8(&bytes[at..at + length])
                    .map_err(|_| MeshError::MaterialNotUtf8)?
                    .to_owned(),
            ),
        };

        Ok(Self {
            vertices,
            indices,
            material,
        })
    }

    /// How many triangles the index buffer describes.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Whether the mesh has no geometry.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty() || self.indices.is_empty()
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

    fn triangle() -> Mesh {
        Mesh {
            vertices: vec![
                Vertex {
                    position: [0.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.0, 0.0],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                },
                Vertex {
                    position: [1.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [1.0, 0.0],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                },
                Vertex {
                    position: [0.0, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    uv: [0.0, 1.0],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                },
            ],
            indices: vec![0, 1, 2],
            material: None,
        }
    }

    #[test]
    fn a_mesh_round_trips() {
        let mesh = triangle();

        assert_eq!(Mesh::read(&mesh.write()), Ok(mesh));
    }

    #[test]
    fn an_empty_mesh_round_trips() {
        let mesh = Mesh::default();

        assert_eq!(Mesh::read(&mesh.write()), Ok(mesh));
    }

    #[test]
    fn awkward_float_values_survive_the_encoding() {
        // Written and read little-endian field by field, so the bit pattern is
        // what travels rather than a decimal rendering of it.
        let mesh = Mesh {
            vertices: vec![Vertex {
                position: [f32::MIN, -0.0, f32::MAX],
                normal: [f32::EPSILON, f32::MIN_POSITIVE, 1.0 / 3.0],
                uv: [f32::INFINITY, f32::NEG_INFINITY],
                tangent: [f32::NAN, -1.0, 0.5, -1.0],
            }],
            indices: vec![0, 0, 0],
            material: None,
        };

        let back = Mesh::read(&mesh.write()).expect("valid");
        let vertex = back.vertices[0];

        assert_eq!(vertex.position[1].to_bits(), (-0.0_f32).to_bits());
        assert_eq!(vertex.position, mesh.vertices[0].position);
        assert_eq!(vertex.normal, mesh.vertices[0].normal);
        assert_eq!(vertex.uv, mesh.vertices[0].uv);
    }

    #[test]
    fn the_encoding_is_little_endian_whatever_the_host_is() {
        // A cooked artifact lives in a per-machine cache today and in a shipped
        // archive later, so byte order is a property of the format.
        let mesh = Mesh {
            vertices: vec![Vertex {
                position: [1.0, 0.0, 0.0],
                normal: [0.0, 0.0, 0.0],
                uv: [0.0, 0.0],
                tangent: [0.0; 4],
            }],
            indices: vec![0],
            material: None,
        };

        let bytes = mesh.write();

        // `1.0f32` is 0x3F800000, little-endian 00 00 80 3F.
        assert_eq!(&bytes[HEADER..HEADER + 4], &[0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = triangle().write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_u32(&bytes, 12), 3, "vertices");
        assert_eq!(read_u32(&bytes, 16), 3, "indices");
        assert_eq!(bytes.len(), HEADER + 3 * VERTEX_SIZE + 3 * 4);
    }

    #[test]
    fn something_that_is_not_a_mesh_is_refused() {
        let error = Mesh::read(b"not a mesh at all").expect_err("wrong magic");

        assert!(matches!(error, MeshError::NotAMesh { .. }));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Mesh::read(&[]).is_err());
        assert!(Mesh::read(b"SLOP").is_err());
    }

    #[test]
    fn a_different_version_says_so_rather_than_decoding_garbage() {
        let mut bytes = triangle().write();
        bytes[8] = 99;

        let error = Mesh::read(&bytes).expect_err("wrong version");

        assert!(
            matches!(error, MeshError::Version { found: 99, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_truncated_mesh_is_refused() {
        let mut bytes = triangle().write();
        bytes.truncate(bytes.len() - 8);

        let error = Mesh::read(&bytes).expect_err("short");

        assert!(matches!(error, MeshError::Truncated { .. }), "{error}");
    }

    #[test]
    fn an_index_past_the_end_is_refused() {
        // Checked rather than trusted: a corrupt cache entry reaching the GPU is
        // a hang or a garbage draw with nothing to point at.
        let mut mesh = triangle();
        mesh.indices = vec![0, 1, 7];

        let error = Mesh::read(&mesh.write()).expect_err("index 7 of 3");

        assert!(
            matches!(error, MeshError::IndexOutOfRange { vertex: 7, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_mesh_counts_its_triangles() {
        assert_eq!(triangle().triangles(), 1);
        assert!(!triangle().is_empty());
        assert!(Mesh::default().is_empty());
    }
}
