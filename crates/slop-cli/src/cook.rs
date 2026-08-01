//! Turning source assets into runtime-ready artifacts — `docs/DESIGN.md` §2.8.
//!
//! Shipping builds never parse a source asset. Cooking is a build step for
//! content, and this is where it lives: `docs/DESIGN.md` §4 names `slop-cli` as
//! "build, cook, run, inspect, test".
//!
//! # What is final here, and what is not
//!
//! **Final:** the cache layout, the content-hash keying, and the invariant that
//! the engine loads cooked bytes and never compiles anything. Those are the
//! parts every later asset type inherits.
//!
//! **Provisional:** shaders are compiled by invoking the `slangc` binary.
//! `docs/DESIGN.md` §2.11 requires Slang as a *library*, because reflection is
//! only reachable through the compilation API — a command-line invocation cannot
//! produce it. `docs/PLAN.md` §4.1-F permits the CLI for M0 specifically, and
//! replacing it changes only [`compile`], not the cache around it.
//!
//! # Why keying matters more than it looks
//!
//! A cook cache that misses a change silently ships a stale artifact, and the
//! symptom appears somewhere unrelated. Three things therefore feed the key:
//! the source bytes, [`COOKER_VERSION`], and the compiler's own version string.
//! A compiler upgrade that changes codegen must invalidate everything, and
//! forgetting that is the classic way a cook cache becomes untrustworthy.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use slop_core::diagnostics::tracing::{debug, info, warn};

/// Bump to invalidate every cooked artifact.
///
/// Any change to how cooking works — different compiler flags, a different
/// output layout, a bug fix in this file — must bump this, or existing caches
/// keep serving artifacts produced by the old rules.
const COOKER_VERSION: u32 = 1;

/// Extension of the shader sources this cooks.
const SHADER_SOURCE_EXTENSION: &str = "slang";

/// What a cook run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    /// Artifacts compiled this run.
    pub(crate) cooked: usize,
    /// Artifacts already up to date.
    pub(crate) skipped: usize,
}

/// Cook every shader under `root/shaders` into `root/.slop/cache/shaders`.
///
/// Incremental: an artifact whose stamp still matches is left alone. `force`
/// ignores stamps and recompiles everything, which is the escape hatch for when
/// the cache is suspected of lying.
///
/// # Errors
///
/// Fails if the compiler cannot be found, a shader fails to compile, or the
/// cache cannot be written.
pub(crate) fn shaders(root: &Path, force: bool) -> Result<Summary> {
    let source_root = root.join("shaders");
    let cache_root = root.join(".slop").join("cache").join("shaders");

    if !source_root.is_dir() {
        warn!(path = %source_root.display(), "no shaders directory; nothing to cook");
        return Ok(Summary::default());
    }

    let compiler = Compiler::discover()?;
    info!(
        path = %compiler.path.display(),
        version = %compiler.version,
        "using shader compiler"
    );

    let mut sources = Vec::new();
    collect_sources(&source_root, &mut sources)?;
    sources.sort();

    let mut summary = Summary::default();

    for source in &sources {
        let relative = source
            .strip_prefix(&source_root)
            .expect("collected paths are under the source root");
        let output = cache_root.join(relative).with_extension("spv");

        if cook_one(&compiler, source, &output, force)? {
            summary.cooked += 1;
        } else {
            summary.skipped += 1;
        }
    }

    Ok(summary)
}

/// Cook one shader. Returns whether it was recompiled.
fn cook_one(compiler: &Compiler, source: &Path, output: &Path, force: bool) -> Result<bool> {
    let bytes = std::fs::read(source)
        .with_context(|| format!("reading shader source {}", source.display()))?;
    let key = cache_key(&bytes, &compiler.version);
    let stamp = stamp_path(output);

    if !force && is_up_to_date(&stamp, &key, output) {
        debug!(source = %source.display(), "up to date");
        return Ok(false);
    }

    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("cooked path {} has no parent", output.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating cache directory {}", parent.display()))?;

    compile(compiler, source, output)?;

    // The stamp is written *after* the artifact, so an interrupted cook leaves a
    // missing or stale stamp rather than a stamp promising an artifact that was
    // never finished.
    std::fs::write(&stamp, &key).with_context(|| format!("writing stamp {}", stamp.display()))?;

    info!(source = %source.display(), output = %output.display(), "cooked");

    Ok(true)
}

/// Invoke the shader compiler.
///
/// The one provisional part of this module — see the module docs. Compiling the
/// whole file emits a single SPIR-V module containing every `[shader(...)]`
/// entry point, so a vertex and fragment pair travels as one artifact.
fn compile(compiler: &Compiler, source: &Path, output: &Path) -> Result<()> {
    let result = Command::new(&compiler.path)
        .arg(source)
        .args(["-target", "spirv"])
        .arg("-o")
        .arg(output)
        .output()
        .with_context(|| format!("running {}", compiler.path.display()))?;

    if !result.status.success() {
        // Compiler diagnostics are the whole value of a failed compile, so they
        // are surfaced rather than replaced by an exit code.
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);

        bail!(
            "compiling {} failed:\n{}{}",
            source.display(),
            stdout.trim(),
            stderr.trim()
        );
    }

    Ok(())
}

/// Hex-encoded key covering the source, this cooker, and the compiler.
fn cache_key(source: &[u8], compiler_version: &str) -> String {
    let mut hasher = blake3::Hasher::new();

    hasher.update(&COOKER_VERSION.to_le_bytes());
    hasher.update(compiler_version.as_bytes());
    // A length prefix keeps the version and the source from being ambiguous
    // where they meet, so two different pairs cannot hash identically.
    hasher.update(&(source.len() as u64).to_le_bytes());
    hasher.update(source);

    hasher.finalize().to_hex().to_string()
}

/// Whether the artifact exists and its stamp matches.
fn is_up_to_date(stamp: &Path, key: &str, output: &Path) -> bool {
    if !output.is_file() {
        return false;
    }

    std::fs::read_to_string(stamp).is_ok_and(|recorded| recorded == key)
}

fn stamp_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_owned();
    name.push(".stamp");

    PathBuf::from(name)
}

/// Recursively gather shader sources, in no particular order.
fn collect_sources(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry in {}", directory.display()))?
            .path();

        if path.is_dir() {
            collect_sources(&path, found)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext == SHADER_SOURCE_EXTENSION)
        {
            found.push(path);
        }
    }

    Ok(())
}

/// The located shader compiler and its version.
struct Compiler {
    path: PathBuf,
    version: String,
}

impl Compiler {
    /// Find `slangc` and ask what version it is.
    ///
    /// Prefers the Vulkan SDK over `PATH`, because a machine with several SDKs
    /// installed should cook with the one the engine is otherwise using rather
    /// than whichever happens to be first on the path.
    fn discover() -> Result<Self> {
        let executable = format!("slangc{}", std::env::consts::EXE_SUFFIX);

        let path = std::env::var_os("VULKAN_SDK")
            .map(|sdk| PathBuf::from(sdk).join("Bin").join(&executable))
            .filter(|candidate| candidate.is_file())
            .unwrap_or_else(|| PathBuf::from(&executable));

        let version = Self::query_version(&path).with_context(|| {
            format!(
                "could not run the Slang compiler at {}. Install the Vulkan SDK, \
                 or put slangc on PATH",
                path.display()
            )
        })?;

        Ok(Self { path, version })
    }

    fn query_version(path: &Path) -> Result<String> {
        let result = Command::new(path).arg("-v").output()?;

        // slangc reports its version on stderr, and returns a nonzero status
        // while doing so on some builds — so the status is deliberately not
        // checked here. Failing to *run* it is what matters, and that surfaces
        // as an Err from `output()` above.
        let version = String::from_utf8_lossy(&result.stderr).trim().to_owned();

        if version.is_empty() {
            bail!("the Slang compiler reported no version");
        }

        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_changes_when_the_source_changes() {
        let a = cache_key(b"shader one", "2026.8");
        let b = cache_key(b"shader two", "2026.8");

        assert_ne!(a, b);
    }

    #[test]
    fn the_key_changes_when_the_compiler_changes() {
        // A compiler upgrade can change codegen. Not keying on it is how a cook
        // cache silently starts serving artifacts built by different rules.
        let a = cache_key(b"shader", "2026.8");
        let b = cache_key(b"shader", "2026.9");

        assert_ne!(a, b);
    }

    #[test]
    fn the_key_is_stable_for_identical_inputs() {
        assert_eq!(
            cache_key(b"shader", "2026.8"),
            cache_key(b"shader", "2026.8")
        );
    }

    #[test]
    fn version_and_source_cannot_be_confused_at_their_boundary() {
        // Without a length prefix, ("ab", "c") and ("a", "bc") would hash the
        // same, so a compiler upgrade could collide with a source edit.
        assert_ne!(cache_key(b"bc", "a"), cache_key(b"c", "ab"));
    }

    #[test]
    fn the_stamp_sits_beside_its_artifact() {
        let stamp = stamp_path(Path::new("cache/shaders/passes/triangle.spv"));

        assert_eq!(
            stamp,
            PathBuf::from("cache/shaders/passes/triangle.spv.stamp")
        );
    }

    #[test]
    fn a_missing_artifact_is_never_up_to_date() {
        // Even with a matching stamp: the stamp promises an artifact, and if the
        // artifact is gone the promise is void.
        assert!(!is_up_to_date(
            Path::new("does/not/exist.stamp"),
            "anything",
            Path::new("does/not/exist.spv")
        ));
    }
}
