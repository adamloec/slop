//! The cache and the VFS against a real directory.
//!
//! The unit tests check path arithmetic without touching a disk. These check the
//! thing that actually matters — that a cook, a recook and a load agree — which
//! only shows up when files exist and change.
//!
//! The failure this is aimed at is the one that has already happened once in
//! this project: a cache that reports up to date when an input has changed. It
//! is worse than a slow cache, because the stale artifact is silently wrong and
//! the symptom appears somewhere unrelated.

use std::path::{Path, PathBuf};

use slop_asset::{Cache, CacheKey, Vfs};

/// A directory that deletes itself, so tests leave nothing behind.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("slop-asset-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("creating scratch directory");

        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Pretend to cook `source` into `logical`, returning whether work was done.
fn cook(cache: &Cache, logical: &str, source: &[u8]) -> bool {
    let key = CacheKey::builder()
        .input("cooker", &1_u32.to_le_bytes())
        .input("source", source)
        .finish();
    let artifact = cache.artifact(logical);

    if cache.is_current(&artifact, &key) {
        return false;
    }

    cache.prepare(&artifact).expect("preparing");
    std::fs::write(&artifact, source).expect("writing artifact");
    cache.record(&artifact, &key).expect("recording");

    true
}

#[test]
fn a_cooked_artifact_reads_back_through_the_vfs() {
    let scratch = Scratch::new("round-trip");
    let cache = Cache::for_project(scratch.path());
    let vfs = Vfs::for_project(scratch.path());

    assert!(cook(&cache, "shaders/passes/triangle.spv", b"spirv bytes"));

    assert!(vfs.exists("shaders/passes/triangle.spv"));
    assert_eq!(
        vfs.read("shaders/passes/triangle.spv").expect("cooked"),
        b"spirv bytes"
    );
}

#[test]
fn the_writer_and_the_reader_agree_on_layout() {
    // One path shape for both sides, so a cook and a load cannot disagree about
    // where a thing is. This is what lets call sites stop hard-coding
    // `.slop/cache/...`.
    let scratch = Scratch::new("layout");
    let cache = Cache::for_project(scratch.path());
    let vfs = Vfs::for_project(scratch.path());

    let logical = "meshes/props/crate.mesh";
    cook(&cache, logical, b"vertices");

    assert_eq!(
        cache.artifact(logical),
        vfs.resolve(logical).expect("valid")
    );
}

#[test]
fn cooking_twice_does_no_work_the_second_time() {
    let scratch = Scratch::new("incremental");
    let cache = Cache::for_project(scratch.path());

    assert!(cook(&cache, "shaders/one.spv", b"source"));
    assert!(
        !cook(&cache, "shaders/one.spv", b"source"),
        "an unchanged source must not recook"
    );
}

#[test]
fn changing_the_source_recooks() {
    let scratch = Scratch::new("changed-source");
    let cache = Cache::for_project(scratch.path());
    let vfs = Vfs::for_project(scratch.path());

    cook(&cache, "shaders/one.spv", b"first");
    assert!(cook(&cache, "shaders/one.spv", b"second"), "source changed");
    assert_eq!(vfs.read("shaders/one.spv").expect("cooked"), b"second");
}

#[test]
fn changing_a_dependency_recooks_even_though_the_source_did_not() {
    // The bug that has already happened here once: editing a shared include
    // changed what every dependent compiled to, while every source and so every
    // stamp still matched. The cache was wrong, not stale.
    let scratch = Scratch::new("changed-dependency");
    let cache = Cache::for_project(scratch.path());
    let artifact = cache.artifact("shaders/one.spv");

    let cook_with = |includes: &[u8]| {
        let key = CacheKey::builder()
            .input("source", b"unchanged")
            .input("includes", includes)
            .finish();

        if cache.is_current(&artifact, &key) {
            return false;
        }

        cache.prepare(&artifact).expect("preparing");
        std::fs::write(&artifact, includes).expect("writing");
        cache.record(&artifact, &key).expect("recording");

        true
    };

    assert!(cook_with(b"digest-one"));
    assert!(!cook_with(b"digest-one"), "nothing changed");
    assert!(cook_with(b"digest-two"), "the include changed");
}

#[test]
fn deleting_the_artifact_recooks_even_with_a_matching_stamp() {
    // A stamp promises an artifact. If the artifact is gone the promise is
    // false, and trusting the stamp alone would report a build as complete with
    // nothing to load.
    let scratch = Scratch::new("deleted-artifact");
    let cache = Cache::for_project(scratch.path());

    cook(&cache, "shaders/one.spv", b"source");
    std::fs::remove_file(cache.artifact("shaders/one.spv")).expect("removing");

    assert!(cook(&cache, "shaders/one.spv", b"source"));
}

#[test]
fn an_interrupted_cook_leaves_no_stamp_vouching_for_it() {
    // The stamp is written after the artifact, so a crash between the two leaves
    // a missing stamp rather than one promising a half-written file. Simulated
    // by doing the first half and stopping.
    let scratch = Scratch::new("interrupted");
    let cache = Cache::for_project(scratch.path());
    let artifact = cache.artifact("shaders/one.spv");
    let key = CacheKey::builder().input("source", b"source").finish();

    cache.prepare(&artifact).expect("preparing");
    std::fs::write(&artifact, b"half written").expect("writing");
    // ... and the process dies here, before `record`.

    assert!(
        !cache.is_current(&artifact, &key),
        "an artifact with no stamp must never be trusted"
    );
}

#[test]
fn a_stale_stamp_from_an_older_cooker_recooks() {
    // The cooker's own version is an input, so changing how cooking works
    // invalidates everything. Forgetting this is the classic way a cache starts
    // lying after a compiler upgrade.
    let scratch = Scratch::new("cooker-version");
    let cache = Cache::for_project(scratch.path());
    let artifact = cache.artifact("shaders/one.spv");

    let cook_at_version = |version: u32| {
        let key = CacheKey::builder()
            .input("cooker", &version.to_le_bytes())
            .input("source", b"unchanged")
            .finish();

        if cache.is_current(&artifact, &key) {
            return false;
        }

        cache.prepare(&artifact).expect("preparing");
        std::fs::write(&artifact, b"artifact").expect("writing");
        cache.record(&artifact, &key).expect("recording");

        true
    };

    assert!(cook_at_version(1));
    assert!(!cook_at_version(1));
    assert!(cook_at_version(2), "a new cooker invalidates everything");
}

#[test]
fn nested_paths_create_their_directories() {
    let scratch = Scratch::new("nested");
    let cache = Cache::for_project(scratch.path());
    let vfs = Vfs::for_project(scratch.path());

    cook(&cache, "textures/props/wood/oak.ktx2", b"pixels");

    assert!(vfs.exists("textures/props/wood/oak.ktx2"));
}

#[test]
fn reading_something_never_cooked_says_so_usefully() {
    let scratch = Scratch::new("missing");
    let vfs = Vfs::for_project(scratch.path());

    let error = vfs.read("shaders/absent.spv").expect_err("not cooked");
    let message = error.to_string();

    assert!(message.contains("shaders/absent.spv"), "{message}");
    assert!(message.contains("cooked"), "{message}");
}

#[test]
fn two_artifacts_do_not_share_a_stamp() {
    let scratch = Scratch::new("distinct");
    let cache = Cache::for_project(scratch.path());
    let vfs = Vfs::for_project(scratch.path());

    cook(&cache, "shaders/one.spv", b"first");
    cook(&cache, "shaders/two.spv", b"second");

    assert_eq!(vfs.read("shaders/one.spv").expect("cooked"), b"first");
    assert_eq!(vfs.read("shaders/two.spv").expect("cooked"), b"second");
}
