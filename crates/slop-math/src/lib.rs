//! Linear algebra and geometry for the Slop engine.
//!
//! Re-exports `glam` rather than wrapping it, and adds only the engine-specific
//! types `glam` does not provide. See `docs/DESIGN.md` §3.2 for why the vector
//! and matrix types are taken rather than written: vocabulary types are
//! contagious, and inventing our own would cost a conversion at every boundary
//! — glTF import, GPU buffer layout, `egui`, and anything third parties write
//! against `slop-abi` — permanently.
//!
//! # Coordinate conventions
//!
//! **Stated once, here, and never assumed anywhere else.** This is where the
//! bugs live in every engine that left it implicit.
//!
//! | | |
//! |---|---|
//! | World space | Right-handed, **Y up**, **−Z forward**, +X right |
//! | Rotation | Quaternions; counter-clockwise about the axis, viewed from the positive end |
//! | Matrices | Column-major storage, column-vector convention — `M * v`, and `parent * child` composes |
//! | Depth range | `[0, 1]`, not OpenGL's `[-1, 1]` |
//! | Depth direction | **Reversed** — near maps to 1.0, far to 0.0 |
//! | Framebuffer origin | Vulkan's, Y down; the projection matrix absorbs the flip |
//!
//! **Why right-handed Y-up:** it is glTF's convention, and glTF is the import
//! format (`docs/DESIGN.md` §2.8). Matching it means mesh import applies no
//! basis change, which is an entire class of mirrored-model and inside-out
//! normal bug that simply never occurs. It is also `glam`'s `_rh` default.
//!
//! **Why reversed depth:** floating-point depth has most of its precision near
//! zero, while a conventional projection spends most of its range near the far
//! plane — the two are exactly mismatched, which is what produces z-fighting on
//! distant geometry. Mapping near to 1.0 and far to 0.0 aligns them and buys
//! several orders of magnitude of precision for free.
//!
//! This is decided at M0 rather than at M3 deliberately. Reverse-Z is not a
//! renderer tweak: it changes every projection matrix, the depth compare
//! operation, the depth clear value, and the sense of every depth test in the
//! engine. Retrofitting it means auditing all of them at once, which is
//! `docs/DESIGN.md` §1.2 principle 6's "rewrite, not refactor" case.
//!
//! Nothing consumes the depth conventions yet — they bind when projection
//! matrices land with the camera in M0 task F.

mod transform;

pub use transform::Transform;

/// Re-exported so dependent crates need no `glam` dependency of their own, and
/// so the engine cannot end up split across two versions of it.
pub use glam;

// The types that appear in nearly every signature, flattened so callers write
// `slop_math::Vec3` rather than `slop_math::glam::Vec3`. Anything rarer is
// reached through the `glam` re-export above.
pub use glam::{Mat3, Mat4, Quat, Vec2, Vec3, Vec3A, Vec4};

/// The world-space up axis. See the conventions table in the crate docs.
pub const UP: Vec3 = Vec3::Y;

/// The world-space forward axis. Right-handed Y-up means forward is **−Z**,
/// which is the sign error worth stating as a constant rather than typing out.
pub const FORWARD: Vec3 = Vec3::NEG_Z;

/// The world-space right axis.
pub const RIGHT: Vec3 = Vec3::X;
