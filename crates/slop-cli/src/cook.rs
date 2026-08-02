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
//! # The cache itself lives in `slop-asset`
//!
//! Keying, stamping, staleness and layout are not shader-specific, and every
//! later asset type inherits them — so they moved to `slop_asset::Cache` and
//! this file drives it. What stays here is the part that *is* about shaders:
//! finding sources, the include convention, and invoking the compiler.
//!
//! Deliberately not a `Cooker` trait. A shader is one source to one artifact and
//! a glTF is one source to *many*, so a trait shaped by the first would break on
//! the second — see `docs/PLAN.md` §6.1. The cache is what is genuinely shared,
//! and it is what was factored out.
//!
//! # Why keying matters more than it looks
//!
//! A cook cache that misses a change silently ships a stale artifact, and the
//! symptom appears somewhere unrelated. Four things therefore feed the key: the
//! source bytes, [`COOKER_VERSION`], the compiler's own version string, and a
//! digest of every shared include. A compiler upgrade that changes codegen must
//! invalidate everything, and forgetting that is the classic way a cook cache
//! becomes untrustworthy.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use slop_asset::{Cache, CacheKey};
use slop_core::diagnostics::tracing::{debug, info, warn};

/// Bump to invalidate every cooked artifact.
///
/// Any change to how cooking works — different compiler flags, a different
/// output layout, a bug fix in this file — must bump this, or existing caches
/// keep serving artifacts produced by the old rules.
const COOKER_VERSION: u32 = 2;

/// Extension of the shader sources this cooks.
const SHADER_SOURCE_EXTENSION: &str = "slang";

/// Directory of shared includes, relative to the shader root.
///
/// Files here are `#include`d by other shaders and never compiled on their own:
/// a file with no `[shader(...)]` entry point produces nothing useful and, more
/// to the point, an error. Excluding by directory rather than by inspecting
/// contents keeps the rule something an author can see in the file tree.
const SHADER_INCLUDE_DIRECTORY: &str = "lib";

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
    let cache = Cache::for_project(root);

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

    let includes = include_digest(&source_root)?;

    let mut sources = Vec::new();
    collect_sources(&source_root, &mut sources)?;
    sources.sort();

    let mut summary = Summary::default();

    for source in &sources {
        let relative = source
            .strip_prefix(&source_root)
            .expect("collected paths are under the source root");
        let output = cache.artifact(&logical_path(relative));

        if cook_one(
            &compiler,
            &cache,
            &source_root,
            source,
            &output,
            &includes,
            force,
        )? {
            summary.cooked += 1;
        } else {
            summary.skipped += 1;
        }
    }

    Ok(summary)
}

/// The logical path a cooked shader is addressed by.
///
/// `passes/triangle.slang` becomes `shaders/passes/triangle.spv`, which is what
/// both [`Cache::artifact`] and [`Vfs::read`](slop_asset::Vfs::read) take —
/// writer and reader cannot disagree about layout because they are given the
/// same string.
fn logical_path(relative: &Path) -> String {
    let cooked = relative.with_extension("spv");
    let segments: Vec<String> = cooked
        .components()
        .map(|segment| segment.as_os_str().to_string_lossy().into_owned())
        .collect();

    format!("shaders/{}", segments.join("/"))
}

/// Cook one shader. Returns whether it was recompiled.
fn cook_one(
    compiler: &Compiler,
    cache: &Cache,
    source_root: &Path,
    source: &Path,
    output: &Path,
    includes: &str,
    force: bool,
) -> Result<bool> {
    let bytes = std::fs::read(source)
        .with_context(|| format!("reading shader source {}", source.display()))?;
    let key = cache_key(&bytes, &compiler.version, includes);

    if !force && cache.is_current(output, &key) {
        debug!(source = %source.display(), "up to date");
        return Ok(false);
    }

    cache.prepare(output)?;
    compile(compiler, source_root, source, output)?;

    // Recorded *after* the artifact, so an interrupted cook leaves a missing
    // stamp rather than one promising a file that was never finished.
    cache.record(output, &key)?;

    info!(source = %source.display(), output = %output.display(), "cooked");

    Ok(true)
}

/// Invoke the shader compiler.
///
/// The one provisional part of this module — see the module docs. Compiling the
/// whole file emits a single SPIR-V module containing every `[shader(...)]`
/// entry point, so a vertex and fragment pair travels as one artifact.
fn compile(compiler: &Compiler, source_root: &Path, source: &Path, output: &Path) -> Result<()> {
    let result = Command::new(&compiler.path)
        .arg(source)
        .args(["-target", "spirv"])
        // The shader root on the include path, so a shader writes
        // `#include "lib/bindless.slang"` rather than counting `../`s to reach
        // it. Paths in source then stay stable if a shader moves between
        // directories.
        .arg("-I")
        .arg(source_root)
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

/// Hex-encoded key covering the source, its includes, this cooker, and the
/// compiler.
fn cache_key(source: &[u8], compiler_version: &str, includes: &str) -> CacheKey {
    // Every input is labelled and length-prefixed by the builder, so two
    // different sets cannot hash alike by running together at the boundary.
    // Naming each one is also what makes an omission visible: the include digest
    // is here because leaving it out made the cache *wrong* rather than stale.
    CacheKey::builder()
        .input("cooker", &COOKER_VERSION.to_le_bytes())
        .input("compiler", compiler_version.as_bytes())
        .input("includes", includes.as_bytes())
        .input("source", source)
        .finish()
}

/// One hash covering every shared include.
///
/// Without this the cache is **wrong**, not merely stale: editing
/// `lib/bindless.slang` changes what every shader including it compiles to,
/// while none of their sources change, so every stamp still matches and the
/// cook reports everything up to date. That is the failure mode content-hash
/// caching exists to prevent, arriving through the back door.
///
/// Deliberately coarse — *any* include changing recooks *every* shader, whether
/// or not it included the file. The precise answer is a per-shader dependency
/// list, which `slangc` can emit with `-depfile`, and which belongs with the
/// library integration at M2 (`docs/DESIGN.md` §2.11) rather than with a
/// provisional CLI wrapper. Recooking a handful of shaders unnecessarily costs
/// a second; getting this wrong costs a debugging session.
fn include_digest(source_root: &Path) -> Result<String> {
    let directory = source_root.join(SHADER_INCLUDE_DIRECTORY);

    if !directory.is_dir() {
        return Ok(String::from("no-includes"));
    }

    let mut includes = Vec::new();
    collect_all(&directory, &mut includes)?;
    // Sorted, because directory iteration order is not defined and a digest
    // that depends on it would differ between machines — which would defeat
    // artifact reuse across the CI matrix (`docs/DESIGN.md` §2.13).
    includes.sort();

    let mut digest = CacheKey::builder();

    for include in &includes {
        let bytes = std::fs::read(include)
            .with_context(|| format!("reading shader include {}", include.display()))?;

        // The path is an input too, so renaming a file changes the digest even
        // when its contents do not — a rename can change which `#include`
        // resolves, and that has to invalidate.
        digest = digest
            .input("path", include.to_string_lossy().as_bytes())
            .input("contents", &bytes);
    }

    Ok(digest.finish().as_str().to_owned())
}

/// Recursively gather shader sources, in no particular order.
///
/// Skips [`SHADER_INCLUDE_DIRECTORY`]: those files are `#include`d by others
/// and have no entry points of their own, so compiling one standalone fails.
/// They still affect the cache key, through [`include_digest`].
fn collect_sources(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry in {}", directory.display()))?
            .path();

        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == SHADER_INCLUDE_DIRECTORY)
            {
                continue;
            }

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

/// Recursively gather every file, regardless of extension.
///
/// Every file, not only `.slang`: an include directory may hold a `.h` of
/// shared constants or a generated table, and any of them changing changes what
/// the shaders compile to.
fn collect_all(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry in {}", directory.display()))?
            .path();

        if path.is_dir() {
            collect_all(&path, found)?;
        } else {
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

    /// The include digest of a tree with nothing shared in it.
    const NO_INCLUDES: &str = "no-includes";

    #[test]
    fn the_key_changes_when_the_source_changes() {
        let a = cache_key(b"shader one", "2026.8", NO_INCLUDES);
        let b = cache_key(b"shader two", "2026.8", NO_INCLUDES);

        assert_ne!(a, b);
    }

    #[test]
    fn the_key_changes_when_the_compiler_changes() {
        // A compiler upgrade can change codegen. Not keying on it is how a cook
        // cache silently starts serving artifacts built by different rules.
        let a = cache_key(b"shader", "2026.8", NO_INCLUDES);
        let b = cache_key(b"shader", "2026.9", NO_INCLUDES);

        assert_ne!(a, b);
    }

    #[test]
    fn the_key_changes_when_an_include_changes() {
        // The bug this exists to prevent: editing a shared include changes what
        // every shader including it compiles to, while none of their own
        // sources change. Without this the stamps all still match and the cook
        // reports everything up to date — a cache that is wrong rather than
        // merely stale.
        let a = cache_key(b"shader", "2026.8", "digest-before");
        let b = cache_key(b"shader", "2026.8", "digest-after");

        assert_ne!(a, b);
    }

    #[test]
    fn the_key_is_stable_for_identical_inputs() {
        assert_eq!(
            cache_key(b"shader", "2026.8", NO_INCLUDES),
            cache_key(b"shader", "2026.8", NO_INCLUDES)
        );
    }

    #[test]
    fn version_and_source_cannot_be_confused_at_their_boundary() {
        // Without a length prefix, ("ab", "c") and ("a", "bc") would hash the
        // same, so a compiler upgrade could collide with a source edit.
        assert_ne!(
            cache_key(b"bc", "a", NO_INCLUDES),
            cache_key(b"c", "ab", NO_INCLUDES)
        );
    }

    #[test]
    fn a_tree_with_no_include_directory_still_produces_a_digest() {
        let digest = include_digest(Path::new("does/not/exist"))
            .expect("a missing include directory is not an error");

        assert_eq!(digest, NO_INCLUDES);
    }

    #[test]
    fn the_include_digest_covers_contents_and_names() {
        let root = std::env::temp_dir().join("slop-cook-include-digest");
        let includes = root.join(SHADER_INCLUDE_DIRECTORY);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&includes).expect("temp directory");

        let path = includes.join("bindless.slang");
        std::fs::write(&path, b"original").expect("write");
        let before = include_digest(&root).expect("digest");

        std::fs::write(&path, b"modified").expect("write");
        let after_edit = include_digest(&root).expect("digest");
        assert_ne!(before, after_edit, "editing an include must change it");

        // A rename changes which `#include` resolves, so it must invalidate
        // even though no file's contents changed.
        std::fs::rename(&path, includes.join("renamed.slang")).expect("rename");
        let after_rename = include_digest(&root).expect("digest");
        assert_ne!(after_edit, after_rename, "renaming must change it too");

        let _ = std::fs::remove_dir_all(&root);
    }

    // Stamping and staleness moved to `slop_asset::Cache` and are tested there.
    // What stays here is the shader-specific part.

    #[test]
    fn a_source_becomes_a_logical_path_under_shaders() {
        // The one string both the cache and the VFS are handed, which is why
        // they cannot disagree about where an artifact lives.
        assert_eq!(
            logical_path(Path::new("passes").join("triangle.slang").as_path()),
            "shaders/passes/triangle.spv"
        );
        assert_eq!(logical_path(Path::new("cube.slang")), "shaders/cube.spv");
    }

    #[test]
    fn a_logical_path_uses_forward_slashes_on_every_platform() {
        // It is a name rather than something the OS sees, so a backslash here
        // would give one shader two names depending on where it was cooked.
        let logical = logical_path(Path::new("a").join("b").join("c.slang").as_path());

        assert!(!logical.contains('\\'), "{logical}");
        assert_eq!(logical, "shaders/a/b/c.spv");
    }

    #[test]
    fn the_include_digest_is_an_input_to_the_key() {
        // The bug that made this cache wrong rather than stale: without the
        // digest, editing a shared include changed what every dependent compiled
        // to while every source, and so every stamp, still matched.
        let source = b"float4 main() { return 0; }";

        assert_ne!(
            cache_key(source, "slangc 1", "digest-one"),
            cache_key(source, "slangc 1", "digest-two")
        );
    }

    #[test]
    fn the_compiler_version_is_an_input_to_the_key() {
        let source = b"float4 main() { return 0; }";

        assert_ne!(
            cache_key(source, "slangc 1", "digest"),
            cache_key(source, "slangc 2", "digest")
        );
    }
}
