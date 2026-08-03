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
    /// No ancestor of the starting directory holds a cooked cache.
    ///
    /// In a fresh clone this means nothing has been cooked yet, which is why the
    /// message says so rather than only naming the directory searched from.
    ///
    /// It says so *without naming a command*. This message used to end with
    /// ``run `cargo run -p slop-cli -- cook` first``, which is a library crate
    /// naming a binary and an invocation — the `CONVENTIONS.md` §5.1 line, that
    /// only the application layer knows how it was launched, crossed inside an
    /// error message. It also becomes wrong the moment `DESIGN.md` §2.12's
    /// editor cooks assets itself rather than telling a person to run
    /// something, which is the whole reason `slop-cook` was extracted from the
    /// CLI. Naming the condition is this crate's job; prescribing the remedy is
    /// the caller's, and every caller in the tree already does it.
    #[error("no cooked assets found in {from} or any parent; nothing has been cooked yet")]
    NoProject {
        /// Where the search started.
        from: PathBuf,
    },

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

/// A token that changes when the bytes at a logical path change.
///
/// Opaque on purpose. Today it is the file's modification time and length,
/// because that is what a directory on disk can answer cheaply; a packed archive
/// would answer from its index, and a network mount from an etag. Comparing two
/// of these is the only supported operation, which is what lets the answer come
/// from somewhere else later without a caller noticing.
///
/// **Not a content hash.** Two writes within the filesystem's timestamp
/// granularity that leave the length unchanged compare equal, so a change can be
/// missed. Hashing instead would mean reading every byte of every asset on every
/// poll — the cost this exists to avoid — and the failure mode is one more save
/// rather than anything incorrect. Where exactness matters, the cook cache
/// already hashes content (`Cache`, §2.8); this is the cheap runtime check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// `None` when the filesystem will not report one, which some network and
    /// container mounts do. Length alone then carries the signal.
    modified: Option<std::time::SystemTime>,
    len: u64,
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

    /// Find the project containing `start`, by walking up for a cache directory.
    ///
    /// `start` is usually the current directory or the directory holding the
    /// executable. **The caller chooses**, because which one is right depends on
    /// how the program was launched, and `docs/CONVENTIONS.md` §5.1 puts that
    /// decision in the application layer rather than in a library reading the
    /// environment behind everyone's back.
    ///
    /// # Why this exists
    ///
    /// Every example used to compute its root from `CARGO_MANIFEST_DIR`, which
    /// is baked in at compile time and points at a source tree — so it is
    /// correct only when the binary is run from the build that produced it, and
    /// wrong the moment anything is installed or copied. Four copies of that
    /// expression is what made it worth fixing rather than deduplicating.
    ///
    /// Walking up is what every project-scoped tool does — `git`, `cargo`,
    /// `node` — and it works the same in a source tree and beside a shipped
    /// binary.
    ///
    /// # Errors
    ///
    /// [`VfsError::NoProject`] if no ancestor of `start` holds a cache
    /// directory, which in a fresh clone means nothing has been cooked yet.
    pub fn discover(start: &Path) -> Result<Self, VfsError> {
        for directory in start.ancestors() {
            let cache = crate::cache_root(directory);

            if cache.is_dir() {
                return Ok(Self::new(cache));
            }
        }

        Err(VfsError::NoProject {
            from: start.to_path_buf(),
        })
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

    /// A token for the current bytes at `logical`, for spotting a change without
    /// reading them.
    ///
    /// `None` if the path is malformed or nothing is there. A caller polling for
    /// changes should read that as *unchanged* rather than as removed: an editor
    /// saving over a file often deletes and renames, so "absent" is a state a
    /// perfectly healthy asset passes through for a few milliseconds.
    pub fn version(&self, logical: &str) -> Option<Version> {
        let metadata = std::fs::metadata(self.resolve(logical).ok()?).ok()?;

        if !metadata.is_file() {
            return None;
        }

        Some(Version {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
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
mod discovery_tests {
    use super::*;

    /// A scratch project with a cache directory and a nested subdirectory.
    ///
    /// Named per test and cleared first, matching `registry.rs` — tests share
    /// one temp directory and a fixed name would make them collide when run in
    /// parallel.
    fn project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("slop-asset-discover-{name}"));
        let _ = std::fs::remove_dir_all(&root);

        std::fs::create_dir_all(crate::cache_root(&root)).expect("creating the cache");
        std::fs::create_dir_all(root.join("src").join("deep")).expect("creating a subdirectory");

        root
    }

    #[test]
    fn discovery_walks_up_to_the_project() {
        // The property that makes this work from anywhere inside a project,
        // which is what a tool run from a subdirectory needs.
        let root = project("walks-up");

        let vfs = Vfs::discover(&root.join("src").join("deep")).expect("the cache is above `deep`");

        assert_eq!(vfs.root(), crate::cache_root(&root));
    }

    #[test]
    fn discovery_finds_a_cache_in_the_starting_directory() {
        let root = project("right-here");

        let vfs = Vfs::discover(&root).expect("the cache is right here");

        assert_eq!(vfs.root(), crate::cache_root(&root));
    }

    #[test]
    fn no_project_says_nothing_is_cooked_without_naming_a_command() {
        // The first error a fresh clone hits. Naming only the directory would
        // send someone looking for a path problem rather than a missing build
        // step — so the condition has to be in the message.
        //
        // The *remedy* must not be. This message used to end with
        // ``run `cargo run -p slop-cli -- cook` first``, which is a library
        // naming a binary: the `CONVENTIONS.md` §5.1 line crossed inside an
        // error, and wrong as soon as `DESIGN.md` §2.12's editor cooks assets
        // itself instead of telling a person to run something. Every caller
        // already appends its own remedy, which is the layer that knows one.
        //
        // Under the temp directory rather than at the filesystem root, and that
        // matters: `ancestors` walks all the way up, so a scratch directory
        // inside a *real* project would find that project's cache and pass for
        // the wrong reason.
        let empty = std::env::temp_dir().join("slop-asset-discover-empty");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).expect("creating scratch");

        let failure = Vfs::discover(&empty).expect_err("nothing is cooked here");

        assert!(matches!(failure, VfsError::NoProject { .. }));

        let message = failure.to_string();
        assert!(
            message.contains("cooked"),
            "the condition must be named: {message}"
        );
        assert!(
            !message.contains("cargo run") && !message.contains("slop-cli"),
            "a library must not prescribe a command: {message}"
        );
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
