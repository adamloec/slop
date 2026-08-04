//! Turning source assets into cooked artifacts — `docs/DESIGN.md` §2.8.
//!
//! ```text
//! source asset  →  import  →  cook  →  runtime format
//!  (.gltf/.png/.slang)        (BC7 textures, mip chains,
//!                              vertex buffers, SPIR-V, reflection)
//! ```
//!
//! # Why this is a library and not just the CLI
//!
//! It was the CLI: 3,167 of `slop-cli`'s 3,319 lines were this, `pub(crate)`
//! inside a binary, so nothing else could ever call it. The editor is what
//! forces the split — dropping a `.gltf` into a project has to cook it, and the
//! alternatives are shelling out to the `slop` binary (no structured progress,
//! no typed failures, a process launch per asset, and the binary has to be
//! findable) or linking this.
//!
//! `slop-cli` is now a thin command-line front end over this crate, which is the
//! same library-plus-binary shape everything else here has.
//!
//! # Nothing that links this crate ships
//!
//! `gltf`, `png`, `intel_tex_2` and the Slang compiler live here and nowhere
//! else. A game links [`slop-asset`](slop_asset) for the *read* path and never
//! links this, which is what makes §2.8's "shipping builds never parse a PNG or
//! a glTF at runtime" a property of the dependency graph rather than a habit.
//!
//! That was previously true by accident — the cooker happened to be a different
//! binary, and nothing stopped anything depending on `slop-cli`. It is now
//! true by construction, which is what `slop-asset`'s invariant 7 asks for.
//!
//! # On `anyhow` in a library
//!
//! `docs/CONVENTIONS.md` §6 says `thiserror` in libraries, `anyhow` only at
//! application boundaries — and this is a library using `anyhow`. Deliberate,
//! and argued from the rule's own reason rather than claimed as an exemption.
//!
//! The reason the rule gives is "**a caller must be able to match and respond**".
//! Nothing does, here: every caller — the CLI, and the editor when it arrives —
//! reports the failure and marks the asset as not cooked. What is actually
//! wanted from a cook failure is the *context chain*, because "reading primitive
//! 3 of mesh 'Body' in sponza.gltf: index 5 names a vertex the primitive does
//! not have" is the whole diagnosis, and that is precisely what `anyhow` carries
//! and a flat enum discards.
//!
//! **The trigger to type these is a caller that branches on the kind** — an
//! editor that shows a missing-texture failure differently from a malformed-file
//! one. `docs/PLAN.md` §6.1 carries the row.

mod cube;
mod geometry;
mod import;
mod panorama;
mod reflection;
mod sources;
mod specular;

use std::path::Path;

use anyhow::{Context, Result};

pub use import::Summary;

/// Cook every shader, model, texture and environment under `root`.
///
/// Incremental: an artifact whose stamp still matches its source is left alone.
/// `force` ignores stamps, which is the escape hatch for when the cache is
/// suspected of lying.
///
/// **Order matters.** Models are cooked after shaders and before nothing in
/// particular, but within the model importer a primitive names the material it
/// uses, so materials are written first. Across the three
/// kinds there is no dependency, and this order is simply the cheapest first.
///
/// # Errors
///
/// Fails if a source cannot be read or parsed, a shader fails to compile, or the
/// cache cannot be written. The error carries the chain of what was being cooked.
pub fn all(root: &Path, force: bool) -> Result<Summary> {
    let context = || format!("cooking assets under {}", root.display());

    let shaders = import::shader::shaders(root, force).with_context(context)?;
    let meshes = import::gltf::meshes(root, force).with_context(context)?;
    let textures = import::texture::textures(root, force).with_context(context)?;
    let environments = import::environment::environments(root, force).with_context(context)?;

    Ok(Summary {
        cooked: shaders.cooked + meshes.cooked + textures.cooked + environments.cooked,
        skipped: shaders.skipped + meshes.skipped + textures.skipped + environments.skipped,
    })
}

/// Cook only the shaders under `root/shaders`.
///
/// # Errors
///
/// As [`all`], for shaders.
pub fn shaders(root: &Path, force: bool) -> Result<Summary> {
    import::shader::shaders(root, force)
}

/// Cook only the glTF files under `root/assets`, and the materials, images and
/// models they name.
///
/// # Errors
///
/// As [`all`], for models.
pub fn models(root: &Path, force: bool) -> Result<Summary> {
    import::gltf::meshes(root, force)
}

/// Cook only the PNGs under `root/assets`.
///
/// # Errors
///
/// As [`all`], for textures.
pub fn textures(root: &Path, force: bool) -> Result<Summary> {
    import::texture::textures(root, force)
}

/// Cook only the HDR panoramas under `root/assets`.
///
/// # Errors
///
/// As [`all`], for environments.
pub fn environments(root: &Path, force: bool) -> Result<Summary> {
    import::environment::environments(root, force)
}
