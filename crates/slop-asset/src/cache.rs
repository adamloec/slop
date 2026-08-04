//! Where a cooked artifact lives, and what says whether it is still current.
//!
//! The write side of the pipeline. `slop-cli` drives it; nothing that ships
//! needs it.
//!
//! # The stamp discipline
//!
//! Beside every artifact is a `.stamp` file holding the [`CacheKey`] that
//! produced it. An artifact is current when **both** the artifact exists and its
//! stamp matches — never the stamp alone, because a stamp promises an artifact
//! and a deleted or half-written one would make that promise a lie.
//!
//! The stamp is written *after* the artifact, so an interrupted cook leaves a
//! missing stamp rather than one vouching for a file that was never finished.
//! That ordering is the entire crash-safety argument, and reversing it turns a
//! rerun-and-recover into a corrupt cache.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why the cache could not be read or written.
#[derive(Debug, Error)]
pub enum CacheError {
    /// A directory could not be created, or a stamp could not be written.
    #[error("{action} {path}")]
    Io {
        /// What was being attempted.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// An artifact path has no parent directory to create.
    #[error("cooked path {path} has no parent directory")]
    Rootless {
        /// The offending path.
        path: PathBuf,
    },
}

/// A content hash covering everything that decides an artifact's bytes.
///
/// Compared as a whole and never inspected. Two artifacts share a key exactly
/// when every input that produced them was identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// Start building a key.
    pub fn builder() -> KeyBuilder {
        KeyBuilder {
            hasher: blake3::Hasher::new(),
        }
    }

    /// The key as it is written into a stamp.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Accumulates the inputs a [`CacheKey`] covers.
///
/// Every input is **labelled and length-prefixed**, so no two different sets of
/// inputs can hash alike by running together at the boundary — a source ending
/// `"abc"` followed by a version `"1"` must not collide with a source ending
/// `"ab"` followed by `"c1"`.
///
/// Requiring a label is what makes the omission that caused the include bug
/// visible: adding an input means naming it, and reading the cooker back shows
/// what it does and does not depend on.
#[derive(Debug)]
pub struct KeyBuilder {
    hasher: blake3::Hasher,
}

impl KeyBuilder {
    /// Add an input the artifact depends on.
    ///
    /// ```
    /// use slop_asset::CacheKey;
    ///
    /// let key = CacheKey::builder()
    ///     .input("cooker", &2_u32.to_le_bytes())
    ///     .input("tool", b"slangc 2026.8")
    ///     .input("source", b"float4 main() { ... }")
    ///     .finish();
    ///
    /// assert_eq!(key.as_str().len(), 64);
    /// ```
    #[must_use]
    pub fn input(mut self, label: &str, bytes: &[u8]) -> Self {
        self.hasher.update(&(label.len() as u64).to_le_bytes());
        self.hasher.update(label.as_bytes());
        self.hasher.update(&(bytes.len() as u64).to_le_bytes());
        self.hasher.update(bytes);

        self
    }

    /// Finish the key.
    pub fn finish(self) -> CacheKey {
        CacheKey(self.hasher.finalize().to_hex().to_string())
    }
}

/// A directory of cooked artifacts.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// A cache rooted at `root`, which is the cache directory itself.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache belonging to the project at `project`.
    pub fn for_project(project: &Path) -> Self {
        Self::new(crate::cache_root(project))
    }

    /// The cache directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the artifact for `relative` lives.
    ///
    /// `relative` is the cooked path — `shaders/examples/triangle.spv` — which is
    /// also what [`Vfs::read`](crate::Vfs::read) asks for. One path shape for
    /// both sides means the writer and the reader cannot disagree about layout.
    pub fn artifact(&self, relative: &str) -> PathBuf {
        let mut path = self.root.clone();
        for segment in relative.split('/') {
            path.push(segment);
        }

        path
    }

    /// Whether `artifact` exists and its stamp records `key`.
    ///
    /// Both, never the stamp alone — see the module documentation.
    pub fn is_current(&self, artifact: &Path, key: &CacheKey) -> bool {
        if !artifact.is_file() {
            return false;
        }

        std::fs::read_to_string(stamp_path(artifact)).is_ok_and(|recorded| recorded == key.as_str())
    }

    /// Create the directory `artifact` will be written into.
    ///
    /// # Errors
    ///
    /// If the path has no parent, or the directory cannot be created.
    pub fn prepare(&self, artifact: &Path) -> Result<(), CacheError> {
        let parent = artifact.parent().ok_or_else(|| CacheError::Rootless {
            path: artifact.to_path_buf(),
        })?;

        std::fs::create_dir_all(parent).map_err(|source| CacheError::Io {
            action: "creating cache directory",
            path: parent.to_path_buf(),
            source,
        })
    }

    /// Record that `artifact` was produced for `key`.
    ///
    /// Call this **after** the artifact is written, so an interrupted cook
    /// leaves no stamp vouching for an unfinished file.
    ///
    /// # Errors
    ///
    /// If the stamp cannot be written.
    pub fn record(&self, artifact: &Path, key: &CacheKey) -> Result<(), CacheError> {
        let stamp = stamp_path(artifact);

        std::fs::write(&stamp, key.as_str()).map_err(|source| CacheError::Io {
            action: "writing stamp",
            path: stamp,
            source,
        })
    }
}

/// The stamp that sits beside an artifact.
fn stamp_path(artifact: &Path) -> PathBuf {
    let mut name = artifact.as_os_str().to_owned();
    name.push(".stamp");

    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_covers_every_input() {
        let base = CacheKey::builder().input("source", b"abc").finish();
        let more = CacheKey::builder()
            .input("source", b"abc")
            .input("tool", b"v2")
            .finish();

        assert_ne!(base, more);
    }

    #[test]
    fn inputs_cannot_run_together_at_the_boundary() {
        // The reason every input is length-prefixed. Without it these two hash
        // identically, and a source change would be cancelled out by a version
        // change.
        let left = CacheKey::builder()
            .input("a", b"abc")
            .input("b", b"1")
            .finish();
        let right = CacheKey::builder()
            .input("a", b"ab")
            .input("b", b"c1")
            .finish();

        assert_ne!(left, right);
    }

    #[test]
    fn labels_distinguish_otherwise_identical_inputs() {
        let left = CacheKey::builder().input("source", b"x").finish();
        let right = CacheKey::builder().input("tool", b"x").finish();

        assert_ne!(left, right);
    }

    #[test]
    fn the_same_inputs_always_give_the_same_key() {
        let build = || {
            CacheKey::builder()
                .input("cooker", &3_u32.to_le_bytes())
                .input("source", b"contents")
                .finish()
        };

        assert_eq!(build(), build());
    }

    #[test]
    fn input_order_matters() {
        // Two inputs swapped is a different cooker, not the same one.
        let left = CacheKey::builder()
            .input("a", b"1")
            .input("b", b"2")
            .finish();
        let right = CacheKey::builder()
            .input("b", b"2")
            .input("a", b"1")
            .finish();

        assert_ne!(left, right);
    }

    #[test]
    fn a_key_is_a_full_length_hex_digest() {
        let key = CacheKey::builder().input("source", b"").finish();

        assert_eq!(key.as_str().len(), 64);
        assert!(key.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn an_artifact_path_follows_the_cooked_relative_path() {
        let cache = Cache::new("cache");

        assert_eq!(
            cache.artifact("shaders/examples/triangle.spv"),
            Path::new("cache")
                .join("shaders")
                .join("examples")
                .join("triangle.spv")
        );
    }

    #[test]
    fn the_stamp_sits_beside_its_artifact() {
        assert_eq!(
            stamp_path(Path::new("cache/shaders/triangle.spv")),
            PathBuf::from("cache/shaders/triangle.spv.stamp")
        );
    }

    #[test]
    fn a_missing_artifact_is_never_current() {
        // Even with a matching stamp: the stamp promises an artifact, and if the
        // artifact is gone the promise is false. Deleting a cooked file must
        // recook it.
        let cache = Cache::new("cache");
        let key = CacheKey::builder().input("source", b"x").finish();

        assert!(!cache.is_current(Path::new("does/not/exist.spv"), &key));
    }
}
