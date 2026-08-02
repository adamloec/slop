//! The content pipeline — `docs/DESIGN.md` §2.8.
//!
//! **A shipping build never parses a source asset.** Cooking turns a `.slang`,
//! a `.gltf` or a `.png` into bytes the engine loads directly, keyed by the
//! content that produced them. That split is the whole point: the runtime read
//! path knows nothing about compilers, importers, or source formats.
//!
//! ```text
//!  source tree            cache                       runtime
//!  shaders/tri.slang  →   .slop/cache/shaders/tri.spv  →  Vfs::read
//!         (cook, offline)                 (load, every run)
//! ```
//!
//! # What this crate is today
//!
//! The two halves of that diagram, and nothing else:
//!
//! - [`Cache`] — where a cooked artifact lives, and what decides whether it is
//!   still current. The write side, used by `slop-cli`.
//! - [`Vfs`] — reading cooked bytes at runtime. The read side, which ships.
//!
//! What is deliberately absent, with reasoning in `docs/PLAN.md` §6.1: async
//! streaming, asset handles, a registry of loaded assets, hot reload, and any
//! notion of an asset *kind*. Each waits for a consumer rather than being
//! designed against an imagined one — the mistake §4.1-C avoided for the job
//! system's access declaration, which shipped its API only once the ECS existed
//! to say what it needed.
//!
//! The sync read in particular is **not** a placeholder. A blocking load stays
//! correct for startup, for tools, and for the cooker itself; §2.8's streaming
//! is an additional entry point rather than a replacement for this one.
//!
//! # Why keying is the part worth getting right
//!
//! A cook cache that misses a change ships a stale artifact, and the symptom
//! surfaces somewhere unrelated. That failure has already happened once here: an
//! early version keyed a shader on its own bytes alone, so editing a shared
//! `#include` changed what every dependent compiled to while every stamp still
//! matched. The cache was *wrong*, not merely stale.
//!
//! So [`CacheKey`] takes every input as a labelled, length-prefixed chunk, and
//! there is no way to add one without saying what it is. The cooker's own
//! version is an input too: a change to how cooking works must invalidate
//! everything, and forgetting that is the classic way a cache becomes
//! untrustworthy.

mod cache;
pub mod mesh;
mod vfs;

pub use cache::{Cache, CacheError, CacheKey, KeyBuilder};
pub use mesh::{Mesh, MeshError, Vertex};
pub use vfs::{Vfs, VfsError};

use std::path::{Path, PathBuf};

/// Where cooked artifacts live, relative to a project root.
///
/// Inside the project rather than in a user-wide location, so that deleting it
/// is obviously safe and two checkouts cannot share a cache keyed on one's
/// toolchain.
pub const CACHE_DIRECTORY: &str = ".slop/cache";

/// The cache directory for a project rooted at `project`.
pub fn cache_root(project: &Path) -> PathBuf {
    let mut path = project.to_path_buf();
    for segment in CACHE_DIRECTORY.split('/') {
        path.push(segment);
    }

    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_lives_inside_the_project() {
        let root = cache_root(Path::new("/games/demo"));

        assert!(root.ends_with("cache"));
        assert!(root.starts_with("/games/demo"));
    }

    #[test]
    fn the_cache_directory_is_split_into_real_path_segments() {
        // Pushing `".slop/cache"` whole would make one directory with a slash in
        // its name on platforms that allow it, and a path that does not compare
        // equal to the built-up form everywhere else.
        let root = cache_root(Path::new("project"));
        let segments: Vec<_> = root.components().collect();

        assert_eq!(segments.len(), 3, "project, .slop, cache");
    }
}
