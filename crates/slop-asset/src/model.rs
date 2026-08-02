//! The cooked model format: where each piece of geometry sits.
//!
//! A glTF file is a *tree* of nodes carrying transforms, some of which reference
//! meshes. Cooking flattens that into a list: every mesh primitive paired with
//! the world transform it should be drawn at. Without this, importing a building
//! produces several hundred meshes all sitting at the origin.
//!
//! # Why "model" and not "scene"
//!
//! glTF calls this a scene, and `slop-scene` is a planned crate
//! (`docs/DESIGN.md` §4) for something else entirely — the *runtime* spatial
//! structure: hierarchy, transform propagation, culling, LOD. One is a file, the
//! other is what the engine builds in memory and mutates every frame. Naming
//! both "scene" would make every sentence about either ambiguous, and this
//! project has already had one collision of that kind.
//!
//! # Flattened, deliberately
//!
//! The hierarchy is resolved at cook time and not preserved. That is right for
//! what this feeds — a static level is drawn, not articulated — and it is wrong
//! the moment something animates a parent joint. `docs/PLAN.md` §6.1 records it:
//! skinning and animation arrive at M5 and want the tree, and the tree is a
//! *runtime* structure `slop-scene` will own rather than something this format
//! should carry.
//!
//! # Layout
//!
//! ```text
//! magic      8 bytes  "SLOPMODL"
//! version    u32      VERSION
//! count      u32      instances that follow
//! instances           count × { transform 16 × f32, length u32, mesh path bytes }
//! ```
//!
//! Transforms are **column-major**, matching glTF, Vulkan and `glam`. Getting
//! this wrong transposes every rotation, which looks like objects placed at
//! plausible-but-wrong angles rather than like an error.

use thiserror::Error;

/// The first eight bytes of every cooked model.
const MAGIC: &[u8; 8] = b"SLOPMODL";

/// What this module knows how to read.
pub const VERSION: u32 = 1;

/// Bytes before the instance list.
const HEADER: usize = 16;

/// Bytes of transform per instance.
const TRANSFORM: usize = 64;

/// Why a cooked model could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelError {
    /// The bytes are not a cooked model.
    #[error("not a cooked model: expected magic {expected:?}, found {found:?}")]
    NotAModel {
        /// What every cooked model starts with.
        expected: &'static str,
        /// What these bytes start with.
        found: String,
    },

    /// A cooked model from a different version of the format.
    #[error("cooked model is version {found}, not {expected}; recook it")]
    Version {
        /// The version this understands.
        expected: u32,
        /// The version the file claims.
        found: u32,
    },

    /// A mesh path is not UTF-8.
    #[error("a mesh path in the model is not valid UTF-8")]
    NotUtf8,

    /// The file ends before it says it should.
    #[error("cooked model is truncated: needed {expected} bytes, {found} present")]
    Truncated {
        /// How many bytes were needed at that point.
        expected: usize,
        /// How many there are.
        found: usize,
    },
}

/// One mesh primitive, placed.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    /// Logical path of the cooked mesh to draw.
    ///
    /// The material comes from that mesh rather than from here: glTF binds a
    /// material to a primitive, and repeating it would give two places to
    /// disagree.
    pub mesh: String,
    /// World transform, column-major.
    pub transform: [f32; 16],
}

/// Everything one source file places in the world.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// Each primitive to draw, with where to draw it.
    pub instances: Vec<Instance>,
}

impl Model {
    /// Encode as cooked bytes.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.instances.len() * (TRANSFORM + 32));

        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.instances.len() as u32).to_le_bytes());

        for instance in &self.instances {
            for value in instance.transform {
                out.extend_from_slice(&value.to_le_bytes());
            }

            out.extend_from_slice(&(instance.mesh.len() as u32).to_le_bytes());
            out.extend_from_slice(instance.mesh.as_bytes());
        }

        out
    }

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// [`ModelError`] for anything that is not a cooked model of this version,
    /// or that ends before its header says it should.
    pub fn read(bytes: &[u8]) -> Result<Self, ModelError> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(ModelError::NotAModel {
                expected: "SLOPMODL",
                found: String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).into_owned(),
            });
        }

        let version = read_u32(bytes, 8);
        if version != VERSION {
            return Err(ModelError::Version {
                expected: VERSION,
                found: version,
            });
        }

        let count = read_u32(bytes, 12) as usize;
        let mut instances = Vec::with_capacity(count.min(4096));
        let mut at = HEADER;

        for _ in 0..count {
            let needed = at + TRANSFORM + 4;
            if bytes.len() < needed {
                return Err(ModelError::Truncated {
                    expected: needed,
                    found: bytes.len(),
                });
            }

            let mut transform = [0.0; 16];
            for (index, value) in transform.iter_mut().enumerate() {
                *value = read_f32(bytes, at + index * 4);
            }
            at += TRANSFORM;

            let length = read_u32(bytes, at) as usize;
            at += 4;

            if bytes.len() < at + length {
                return Err(ModelError::Truncated {
                    expected: at + length,
                    found: bytes.len(),
                });
            }

            let mesh = std::str::from_utf8(&bytes[at..at + length])
                .map_err(|_| ModelError::NotUtf8)?
                .to_owned();
            at += length;

            instances.push(Instance { mesh, transform });
        }

        Ok(Self { instances })
    }

    /// Every distinct mesh this model draws, in first-seen order.
    ///
    /// A model places the same mesh many times — that is what an instance *is* —
    /// so a loader wanting to upload each once needs the set rather than the
    /// list. First-seen rather than sorted, so the upload order matches the file
    /// and stays reproducible.
    pub fn meshes(&self) -> Vec<&str> {
        let mut seen = Vec::new();

        for instance in &self.instances {
            if !seen.contains(&instance.mesh.as_str()) {
                seen.push(instance.mesh.as_str());
            }
        }

        seen
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

    /// Column-major identity, with a translation in the last column.
    fn placed(x: f32, y: f32, z: f32) -> [f32; 16] {
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            x, y, z, 1.0,
        ]
    }

    fn sample() -> Model {
        Model {
            instances: vec![
                Instance {
                    mesh: String::from("meshes/hall.Wall.0.mesh"),
                    transform: placed(0.0, 0.0, 0.0),
                },
                Instance {
                    mesh: String::from("meshes/hall.Pillar.0.mesh"),
                    transform: placed(4.0, 0.0, -2.5),
                },
                Instance {
                    mesh: String::from("meshes/hall.Pillar.0.mesh"),
                    transform: placed(-4.0, 0.0, -2.5),
                },
            ],
        }
    }

    #[test]
    fn a_model_round_trips() {
        let model = sample();

        assert_eq!(Model::read(&model.write()), Ok(model));
    }

    #[test]
    fn an_empty_model_round_trips() {
        let model = Model::default();

        assert_eq!(Model::read(&model.write()), Ok(model));
    }

    #[test]
    fn the_header_is_where_the_layout_says() {
        let bytes = sample().write();

        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(read_u32(&bytes, 8), VERSION);
        assert_eq!(read_u32(&bytes, 12), 3, "instance count");
    }

    #[test]
    fn a_transform_survives_column_by_column() {
        // Transposing is the failure that does not error: every rotation comes
        // out mirrored and objects sit at plausible-but-wrong angles.
        let model = sample();
        let back = Model::read(&model.write()).expect("valid");

        assert_eq!(back.instances[1].transform, placed(4.0, 0.0, -2.5));
        assert_eq!(
            back.instances[1].transform[12..15],
            [4.0, 0.0, -2.5],
            "translation lives in the last column"
        );
    }

    #[test]
    fn one_mesh_placed_twice_is_two_instances_and_one_mesh() {
        // What instancing means, and what a loader needs: three placements of
        // two distinct meshes.
        let model = sample();

        assert_eq!(model.instances.len(), 3);
        assert_eq!(
            model.meshes(),
            vec!["meshes/hall.Wall.0.mesh", "meshes/hall.Pillar.0.mesh"]
        );
    }

    #[test]
    fn a_truncated_instance_list_is_refused() {
        let mut bytes = sample().write();
        bytes.truncate(bytes.len() - 5);

        assert!(matches!(
            Model::read(&bytes),
            Err(ModelError::Truncated { .. })
        ));
    }

    #[test]
    fn a_count_that_lies_is_refused_rather_than_panicking() {
        // The shape of a fuzz crash, and the reason `with_capacity` is clamped:
        // a header claiming four billion instances must not try to allocate for
        // them before reading a single one.
        let mut bytes = sample().write();
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(
            Model::read(&bytes),
            Err(ModelError::Truncated { .. })
        ));
    }

    #[test]
    fn something_that_is_not_a_model_is_refused() {
        assert!(matches!(
            Model::read(b"SLOPMESH\0\0\0\0\0\0\0\0"),
            Err(ModelError::NotAModel { .. })
        ));
    }

    #[test]
    fn an_empty_buffer_is_refused_rather_than_panicking() {
        assert!(Model::read(&[]).is_err());
    }

    #[test]
    fn a_different_version_says_so_rather_than_decoding_garbage() {
        let mut bytes = sample().write();
        bytes[8..12].copy_from_slice(&99_u32.to_le_bytes());

        assert_eq!(
            Model::read(&bytes),
            Err(ModelError::Version {
                expected: VERSION,
                found: 99,
            })
        );
    }

    #[test]
    fn the_encoding_is_little_endian_whatever_the_host_is() {
        let bytes = sample().write();

        // The first transform element is 1.0 = 0x3F800000.
        assert_eq!(&bytes[16..20], &[0x00, 0x00, 0x80, 0x3F]);
    }
}
