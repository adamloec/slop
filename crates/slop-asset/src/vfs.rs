//! Reading cooked bytes at runtime.
//!
//! The half that ships. It knows nothing about compilers, importers or source
//! formats — a cooked artifact is bytes at a logical path, and that is the whole
//! interface `docs/DESIGN.md` §2.8 asks the engine to have.
//!
//! ```ignore
//! let vfs = Vfs::for_project(Path::new("."));
//! let spirv = vfs.read("shaders/passes/triangle.spv")?;
//! ```
//!
//! # Logical paths, not filesystem paths
//!
//! A caller says `shaders/passes/triangle.spv` and never `.slop/cache/...`. The
//! layout is this crate's business, which is what lets it change — to a packed
//! archive for a shipped build, to an override directory for a mod — without a
//! single call site moving. Examples and tests previously hard-coded the cache
//! path, which `docs/PLAN.md` §6.1 recorded as waiting for exactly this.
//!
//! Separators are always `/`, on every platform. A logical path is a name, not
//! something the OS ever sees, so letting it vary by platform would mean the
//! same asset had two names.
//!
//! # Synchronous, and that is not a placeholder
//!
//! §2.8 also calls for async streaming, and this blocks. The two are not
//! alternatives: a blocking read stays correct for startup, for tools, and for
//! the cooker itself, and streaming is an additional entry point that will sit
//! beside this one. Recorded in `docs/PLAN.md` §6.1 so that "the VFS is
//! synchronous" is not mistaken for a shortcut.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why an asset could not be read.
#[derive(Debug, Error)]
pub enum VfsError {
    /// Nothing is cooked at that path.
    ///
    /// Separate from a read failure because the fix is different: this almost
    /// always means the cook step has not been run, not that the disk is
    /// unhappy.
    #[error("no cooked asset at '{logical}' (looked in {path}); has it been cooked?")]
    Missing {
        /// The logical path asked for.
        logical: String,
        /// Where that resolved to.
        path: PathBuf,
    },

    /// The file exists but could not be read.
    #[error("reading '{logical}' from {path}")]
    Io {
        /// The logical path asked for.
        logical: String,
        /// Where that resolved to.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The logical path is not one this can resolve.
    ///
    /// Absolute paths and `..` are refused rather than normalised: a logical
    /// path naming something outside the cache is a bug in the caller, and
    /// quietly resolving it would let an asset name reach arbitrary files.
    #[error("'{logical}' is not a valid logical path: {reason}")]
    Malformed {
        /// The logical path asked for.
        logical: String,
        /// What is wrong with it.
        reason: &'static str,
    },
}

/// Reads cooked assets for one project.
#[derive(Debug, Clone)]
pub struct Vfs {
    root: PathBuf,
}

impl Vfs {
    /// A reader over the cache directory `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// A reader over the project at `project`.
    pub fn for_project(project: &Path) -> Self {
        Self::new(crate::cache_root(project))
    }

    /// The directory being read from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a logical path resolves to.
    ///
    /// Exposed for diagnostics — an error saying only "not found" for a path the
    /// caller cannot see the resolution of is a bad afternoon.
    ///
    /// # Errors
    ///
    /// [`VfsError::Malformed`] if the path is absolute, empty, or climbs out of
    /// the cache.
    pub fn resolve(&self, logical: &str) -> Result<PathBuf, VfsError> {
        let reason = if logical.is_empty() {
            Some("it is empty")
        } else if logical.starts_with('/') || logical.contains(':') {
            Some("it is absolute")
        } else if logical.split('/').any(|segment| segment == "..") {
            Some("it climbs above the cache")
        } else if logical.contains('\\') {
            Some("separators are always `/`")
        } else {
            None
        };

        if let Some(reason) = reason {
            return Err(VfsError::Malformed {
                logical: logical.to_owned(),
                reason,
            });
        }

        let mut path = self.root.clone();
        for segment in logical.split('/').filter(|segment| !segment.is_empty()) {
            path.push(segment);
        }

        Ok(path)
    }

    /// Whether something is cooked at `logical`.
    pub fn exists(&self, logical: &str) -> bool {
        self.resolve(logical).is_ok_and(|path| path.is_file())
    }

    /// Read the cooked bytes at `logical`.
    ///
    /// # Errors
    ///
    /// [`VfsError::Missing`] if nothing is cooked there — which is usually a
    /// missing cook step rather than a broken disk, hence its own variant.
    pub fn read(&self, logical: &str) -> Result<Vec<u8>, VfsError> {
        let path = self.resolve(logical)?;

        if !path.is_file() {
            return Err(VfsError::Missing {
                logical: logical.to_owned(),
                path,
            });
        }

        std::fs::read(&path).map_err(|source| VfsError::Io {
            logical: logical.to_owned(),
            path,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vfs() -> Vfs {
        Vfs::new("cache")
    }

    #[test]
    fn a_logical_path_resolves_under_the_root() {
        let resolved = vfs().resolve("shaders/passes/triangle.spv").expect("valid");

        assert_eq!(
            resolved,
            Path::new("cache")
                .join("shaders")
                .join("passes")
                .join("triangle.spv")
        );
    }

    #[test]
    fn separators_are_always_forward_slashes() {
        // A logical path is a name rather than something the OS sees, so letting
        // it vary by platform would give one asset two names.
        let error = vfs()
            .resolve("shaders\\triangle.spv")
            .expect_err("backslash");

        assert!(matches!(error, VfsError::Malformed { .. }));
    }

    #[test]
    fn an_absolute_path_is_refused() {
        assert!(vfs().resolve("/etc/passwd").is_err());
        assert!(vfs().resolve("C:/windows").is_err());
    }

    #[test]
    fn climbing_out_of_the_cache_is_refused() {
        // Refused rather than normalised: an asset name reaching arbitrary files
        // is the shape of a real vulnerability once names come from content.
        assert!(vfs().resolve("../../secrets").is_err());
        assert!(vfs().resolve("shaders/../../secrets").is_err());
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert!(vfs().resolve("").is_err());
    }

    #[test]
    fn a_missing_asset_says_so_and_says_where_it_looked() {
        let error = vfs().read("shaders/absent.spv").expect_err("not cooked");

        match error {
            VfsError::Missing { logical, path } => {
                assert_eq!(logical, "shaders/absent.spv");
                assert!(path.ends_with("absent.spv"));
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn nothing_exists_in_an_empty_cache() {
        assert!(!vfs().exists("shaders/absent.spv"));
        assert!(!vfs().exists("../escape"));
    }
}
