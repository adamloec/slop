//! Where the light in a frame comes from, and which fragments each source
//! reaches.
//!
//! Four modules and one subject. They were at the crate root through E4 and E5,
//! and `docs/CONVENTIONS.md` promotes to a directory once three or more share a
//! subject that is already a name in the crate — "lighting" is `docs/PLAN.md`
//! §9.4's own word for half the frame, so this is overdue rather than early.
//!
//! | | |
//! |---|---|
//! | [`environment`] | The sun, and the sky as nine spherical-harmonic coefficients — the two things every fragment gets regardless of where it is |
//! | [`light`] | Point lights, and the falloff that makes a radius mean something |
//! | [`cluster`] | Which cell of the view frustum each light reaches, so a fragment shades against its own cell rather than the whole scene |
//! | [`shadow`] | Where the four cascades sit, and what each one sees |
//!
//! # Why these four and not the rest
//!
//! The split is by *what the code is about*, not by what it touches. `mesh` also
//! writes a GPU buffer the shader reads, and `graph` also names passes that
//! shade — neither is about where light comes from. The test is whether removing
//! a module would leave a hole in the description above, and for all four it
//! does.
//!
//! # What is deliberately *not* grouped here
//!
//! The `*Gpu` structs. Each is one half of an ABI whose other half is a Slang
//! struct, and the reason each lives beside the code that fills it — `CascadeGpu`
//! next to the cascade fitting, `PointLightGpu` next to the falloff — is that the
//! two halves must be read together to be kept in agreement. Collecting them into
//! a module of "things the shader reads" would be a split by kind, which is the
//! thing `docs/CONVENTIONS.md` bans `types.rs` for, and it would put every ABI's
//! two halves in different files.

pub(crate) mod cluster;
pub(crate) mod environment;
pub(crate) mod light;
pub(crate) mod shadow;
pub(crate) mod sky;
