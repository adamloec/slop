//! Loaded assets, named by handle rather than by path.
//!
//! The [`Vfs`] answers "what bytes are at this name". This answers "what is
//! loaded, and what do I call it once it is" — the first thing in this crate
//! that *holds* an asset rather than handing its bytes to whoever asked.
//!
//! ```ignore
//! let mut meshes = Assets::<Mesh>::for_project(Path::new("."));
//! let cube = meshes.load("meshes/cube.Cube.0.mesh")?;   // decodes
//! let same = meshes.load("meshes/cube.Cube.0.mesh")?;   // does not
//! assert_eq!(cube, same);
//! ```
//!
//! # Why a handle and not the asset itself
//!
//! Three things a `Mesh` by value cannot do, and all three are load-bearing:
//!
//! 1. **Be shared without being copied.** Two hundred crates in a level are one
//!    mesh. Handing out `Mesh` clones decodes and stores it two hundred times.
//! 2. **Survive being replaced.** Hot reload swaps what is behind a name while
//!    the things referring to it keep referring to it. A value handed out
//!    already is unreachable, so nothing can be told it changed.
//! 3. **Cross the guest ABI.** `docs/DESIGN.md` §2.3 makes a handle an opaque
//!    integer a WASM module can hold. An `Rc<Mesh>` cannot cross that boundary,
//!    and a raw pointer must not.
//!
//! The third is why this is [`Handle<T>`] from `slop-core` rather than a
//! reference-counted pointer. Refcounts are the conventional answer and are
//! genuinely simpler *until* an untrusted guest holds one; then they are a
//! pointer the guest can forge. A generational handle is checkable — a stale one
//! fails a lookup instead of reading freed memory (§2.6).
//!
//! # Why a trait here, when the cook side has none
//!
//! This crate deliberately has no `Cooker` trait, and the reasoning is in
//! `docs/PLAN.md` §6.1: a shader is one source to one artifact, a glTF is one
//! source to *many*, so a trait shaped by the first breaks on the second.
//!
//! The load side is the opposite shape, which is why [`Asset`] exists. One
//! cooked artifact decodes to exactly one asset, always — that is what cooking
//! *is*. [`Mesh::read`] and [`Texture::read`] already had identical signatures
//! before this trait was written; it names an agreement that was there, rather
//! than imposing one on two things that disagree.

use std::path::Path;

use slop_core::{FxHashMap, Handle, SlotMap};
use thiserror::Error;

use crate::vfs::Version;
use crate::{Mesh, Texture, Vfs, VfsError};

/// Something that can be loaded from cooked bytes.
///
/// The bounds are wider than today's callers need, on purpose. `Send + Sync`
/// is what lets async streaming (`docs/PLAN.md` §6.1) decode on a worker thread
/// and hand the result back, and both implementors are plain data that satisfy
/// it for free. Relaxing a bound later breaks nobody; adding one breaks every
/// implementor, so the cheap direction to err in is this one.
pub trait Asset: Sized + Send + Sync + 'static {
    /// What to call this kind in a diagnostic.
    ///
    /// "decoding mesh 'meshes/cube.mesh'" reads better than a type name, and a
    /// type name is not available without `std::any::type_name` being stable
    /// enough to put in a message.
    const KIND: &'static str;

    /// Decode cooked bytes.
    ///
    /// # Errors
    ///
    /// Whatever the format's own reader rejects. Boxed rather than an associated
    /// type so that [`AssetError`] stays one concrete type — a caller loading
    /// meshes and textures should not need two error paths to say "that asset
    /// did not decode".
    fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>;
}

impl Asset for Mesh {
    const KIND: &'static str = "mesh";

    fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::read(bytes)?)
    }
}

impl Asset for Texture {
    const KIND: &'static str = "texture";

    fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::read(bytes)?)
    }
}

impl Asset for crate::Material {
    const KIND: &'static str = "material";

    fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::read(bytes)?)
    }
}

impl Asset for crate::Model {
    const KIND: &'static str = "model";

    fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::read(bytes)?)
    }
}

/// Why an asset could not be loaded.
#[derive(Debug, Error)]
pub enum AssetError {
    /// The cooked bytes could not be read.
    #[error("reading {kind} '{logical}'")]
    Read {
        /// Which kind was being loaded.
        kind: &'static str,
        /// The logical path asked for.
        logical: String,
        /// The underlying failure — usually "not cooked yet".
        #[source]
        source: VfsError,
    },

    /// The bytes were read but are not a valid artifact of that kind.
    ///
    /// Distinct from [`AssetError::Read`] because the fix is different: this
    /// means the cache holds something wrong, so the answer is to recook rather
    /// than to cook.
    #[error("decoding {kind} '{logical}'")]
    Decode {
        /// Which kind was being loaded.
        kind: &'static str,
        /// The logical path asked for.
        logical: String,
        /// What the format's reader objected to.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// What is known about a loaded asset besides the asset.
struct Record {
    /// The name it was loaded under, so a handle can be traced back to a file.
    logical: Box<str>,
    /// Bumped every time [`Assets::reload`] replaces the value.
    revision: u32,
    /// What the bytes looked like when they were last read, for
    /// [`Assets::reload_changed`]. `None` when the filesystem would not say.
    version: Option<Version>,
}

/// Every loaded asset of one kind.
///
/// One registry per type rather than one holding everything: `get` then returns
/// a `&Mesh` with no downcast and no way to ask for the wrong type. A single
/// heterogeneous store would need `Any`, and every lookup would gain a failure
/// case that the type system already rules out.
pub struct Assets<T: Asset> {
    vfs: Vfs,
    /// Owns the assets and issues the handles. Generational, so a handle held
    /// across an unload fails its lookup rather than reading a reused slot.
    values: SlotMap<T>,
    /// Handle to what it was loaded as. Keyed by the full handle, generation
    /// included, so a stale handle misses instead of hitting its successor.
    records: FxHashMap<Handle<T>, Record>,
    /// Name to handle, which is what makes [`Assets::load`] idempotent.
    by_path: FxHashMap<Box<str>, Handle<T>>,
}

impl<T: Asset> Assets<T> {
    /// An empty registry reading through `vfs`.
    pub fn new(vfs: Vfs) -> Self {
        Self {
            vfs,
            values: SlotMap::new(),
            records: FxHashMap::default(),
            by_path: FxHashMap::default(),
        }
    }

    /// An empty registry over the project at `project`.
    pub fn for_project(project: &Path) -> Self {
        Self::new(Vfs::for_project(project))
    }

    /// Where this reads cooked bytes from.
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// Load the asset at `logical`, or return the handle it already has.
    ///
    /// Idempotent by name: asking twice decodes once and yields the same handle
    /// both times. That is the whole reason a registry earns its place — without
    /// it, a level referencing one mesh from two hundred places holds two
    /// hundred copies.
    ///
    /// # Errors
    ///
    /// [`AssetError::Read`] if nothing is cooked there, [`AssetError::Decode`]
    /// if what is there is not a valid artifact of this kind.
    pub fn load(&mut self, logical: &str) -> Result<Handle<T>, AssetError> {
        if let Some(handle) = self.by_path.get(logical) {
            return Ok(*handle);
        }

        // Sampled before the read, not after. A write landing between the two
        // then leaves a version that does not describe what was decoded, so the
        // next poll reloads — which is the harmless direction. Sampling after
        // would record the *new* bytes against the *old* value and never notice.
        let version = self.vfs.version(logical);
        let value = self.read_and_decode(logical)?;
        let handle = self.values.insert(value);
        let logical: Box<str> = logical.into();

        self.records.insert(
            handle,
            Record {
                logical: logical.clone(),
                revision: 0,
                version,
            },
        );
        self.by_path.insert(logical, handle);

        Ok(handle)
    }

    /// Re-read an already-loaded asset, replacing the value behind its handle.
    ///
    /// Returns the handle, or `None` if that name is not loaded — reloading
    /// something nobody asked for is a no-op rather than a load, so that a file
    /// watcher can fire on every change in the tree without pulling in assets
    /// the game never wanted.
    ///
    /// **The new value is decoded before the old one is dropped.** A reload that
    /// fails leaves the previous asset in place, so saving a broken mesh mid-
    /// session gets an error in the log rather than a hole where the model was.
    /// This is the same check-then-commit shape `slop-ecs`'s serializer uses,
    /// and for the same reason: the failure is expected, so it must not be
    /// destructive.
    ///
    /// # Errors
    ///
    /// As [`Assets::load`]. The registry is unchanged when this fails.
    pub fn reload(&mut self, logical: &str) -> Result<Option<Handle<T>>, AssetError> {
        let Some(handle) = self.by_path.get(logical).copied() else {
            return Ok(None);
        };

        // Both before the replacement, and for the same reason as in `load`.
        let version = self.vfs.version(logical);
        let value = self.read_and_decode(logical)?;

        *self
            .values
            .get_mut(handle)
            .expect("a handle in by_path always has a value") = value;

        let record = self
            .records
            .get_mut(&handle)
            .expect("a handle in by_path always has a record");
        record.revision += 1;
        record.version = version;

        Ok(Some(handle))
    }

    /// Reload every loaded asset whose bytes have changed on disk.
    ///
    /// This is the runtime half of hot reload. The other half is a cooker
    /// rewriting the artifact — `slop-cli cook --watch` — because
    /// `docs/DESIGN.md` §2.8 keeps source parsing out of anything that ships.
    /// This side watches *cooked* bytes, so the engine still knows nothing about
    /// compilers or source formats (invariant 7), and the same code works
    /// whether the cook was triggered by a watcher, by hand, or by a build
    /// server.
    ///
    /// Returns one entry per asset that was reloaded or tried to be. An empty
    /// result is the overwhelmingly common case and costs one `stat` per loaded
    /// asset, which is what makes it callable every frame.
    ///
    /// A failure leaves that asset as it was — see [`Assets::reload`] — and is
    /// reported rather than returned, because one broken texture must not stop
    /// the other nineteen assets from reloading.
    pub fn reload_changed(&mut self) -> Vec<(Handle<T>, Result<(), AssetError>)> {
        // Collected first: reloading borrows `self` mutably, and one poll should
        // see one consistent set of paths rather than a set that shifts as it
        // goes. The version travels with the path so a failure can still be
        // recorded against the bytes that caused it.
        let stale: Vec<(Box<str>, Option<Version>)> = self
            .records
            .values()
            .filter_map(|record| {
                // Absent is deliberately *not* a change. Editors save by writing
                // a temporary file and renaming over the target, so a healthy
                // asset spends a few milliseconds not existing; treating that as
                // a change would try to reload a file that is not there yet, and
                // do it again on every poll until it appeared.
                let version = self.vfs.version(&record.logical)?;

                (Some(version) != record.version).then(|| (record.logical.clone(), Some(version)))
            })
            .collect();

        let mut outcomes = Vec::new();

        for (logical, version) in stale {
            match self.reload(&logical) {
                Ok(Some(handle)) => outcomes.push((handle, Ok(()))),
                Ok(None) => {}
                Err(error) => {
                    let Some(handle) = self.by_path.get(&logical).copied() else {
                        continue;
                    };

                    // Stamp the failure. `reload` returns before it records
                    // anything, so without this the same broken file is retried
                    // and reported on every single poll — hundreds of identical
                    // errors a second, and no way to tell a new failure from the
                    // old one. Recorded here, it is reported once and stays
                    // quiet until the file changes again, which is exactly when
                    // someone has tried to fix it.
                    if let Some(record) = self.records.get_mut(&handle) {
                        record.version = version;
                    }

                    outcomes.push((handle, Err(error)));
                }
            }
        }

        outcomes
    }

    /// The asset behind a handle, or `None` if it is stale or foreign.
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        self.values.get(handle)
    }

    /// Whether the handle still refers to something loaded.
    pub fn contains(&self, handle: Handle<T>) -> bool {
        self.values.contains(handle)
    }

    /// The handle `logical` is loaded under, if it is loaded.
    pub fn handle(&self, logical: &str) -> Option<Handle<T>> {
        self.by_path.get(logical).copied()
    }

    /// What a handle was loaded from.
    ///
    /// For diagnostics. An error naming a handle rather than a file is an error
    /// nobody can act on.
    pub fn path(&self, handle: Handle<T>) -> Option<&str> {
        self.records.get(&handle).map(|record| &*record.logical)
    }

    /// How many times this asset has been reloaded, or `None` if it is not
    /// loaded.
    ///
    /// The signal a consumer needs and cannot otherwise get. Something that has
    /// uploaded a mesh to the GPU holds a handle whose *contents* changed
    /// underneath it; comparing this against what it last saw is how it knows to
    /// upload again. Without it, hot reload updates the CPU-side asset and
    /// nothing on screen moves.
    pub fn revision(&self, handle: Handle<T>) -> Option<u32> {
        self.records.get(&handle).map(|record| record.revision)
    }

    /// Drop an asset, returning it. Its handle goes stale.
    ///
    /// The name is freed too, so a later [`Assets::load`] of the same path reads
    /// from disk and issues a *new* handle rather than resurrecting the old one.
    /// Leaving the name mapped is the obvious bug here: the registry would hand
    /// back a handle whose slot no longer holds anything.
    pub fn unload(&mut self, handle: Handle<T>) -> Option<T> {
        let record = self.records.remove(&handle)?;
        self.by_path.remove(&record.logical);

        self.values.remove(handle)
    }

    /// How many assets are loaded.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether nothing is loaded.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every loaded asset with its handle, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        self.values.iter()
    }

    /// Read and decode without touching any of the bookkeeping.
    ///
    /// Separate so that [`Assets::reload`] can do the fallible half before it
    /// commits to anything.
    fn read_and_decode(&self, logical: &str) -> Result<T, AssetError> {
        let bytes = self.vfs.read(logical).map_err(|source| AssetError::Read {
            kind: T::KIND,
            logical: logical.to_owned(),
            source,
        })?;

        T::decode(&bytes).map_err(|source| AssetError::Decode {
            kind: T::KIND,
            logical: logical.to_owned(),
            source,
        })
    }
}

// Written out rather than derived: `derive(Debug)` would bound the whole type on
// `T: Debug`, so a registry of a non-`Debug` asset would silently stop being
// printable. The same reason `SlotMap` and `HandleAllocator` write theirs.
impl<T: Asset> std::fmt::Debug for Assets<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assets")
            .field("kind", &T::KIND)
            .field("loaded", &self.values.len())
            .field("root", &self.vfs.root())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An asset kind with no format behind it, so these test the registry rather
    /// than a decoder. `"bad"` fails to decode, which is how the error paths and
    /// the check-then-commit reload are exercised.
    #[derive(Debug, PartialEq)]
    struct Fake(String);

    impl Asset for Fake {
        const KIND: &'static str = "fake";

        fn decode(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
            let text = std::str::from_utf8(bytes)?;

            if text == "bad" {
                return Err("not a fake".into());
            }

            Ok(Self(text.to_owned()))
        }
    }

    /// A registry over a scratch directory, with `files` already cooked into it.
    fn registry(name: &str, files: &[(&str, &str)]) -> (Assets<Fake>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("slop-asset-registry-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creating scratch");

        for (logical, contents) in files {
            write(&root, logical, contents);
        }

        (Assets::new(Vfs::new(root.clone())), root)
    }

    fn write(root: &Path, logical: &str, contents: &str) {
        let path = root.join(logical);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating directory");
        }

        std::fs::write(path, contents).expect("writing");
    }

    #[test]
    fn loading_the_same_path_twice_decodes_once() {
        // The reason the registry exists. Two hundred references to one mesh
        // must be one mesh.
        let (mut assets, _root) = registry("dedup", &[("a.fake", "one")]);

        let first = assets.load("a.fake").expect("cooked");
        let second = assets.load("a.fake").expect("cooked");

        assert_eq!(first, second);
        assert_eq!(assets.len(), 1);
    }

    #[test]
    fn two_paths_are_two_assets() {
        let (mut assets, _root) = registry("distinct", &[("a.fake", "one"), ("b.fake", "two")]);

        let a = assets.load("a.fake").expect("cooked");
        let b = assets.load("b.fake").expect("cooked");

        assert_ne!(a, b);
        assert_eq!(assets.get(a).expect("loaded").0, "one");
        assert_eq!(assets.get(b).expect("loaded").0, "two");
    }

    #[test]
    fn a_handle_knows_what_it_was_loaded_from() {
        let (mut assets, _root) = registry("path", &[("meshes/a.fake", "one")]);

        let handle = assets.load("meshes/a.fake").expect("cooked");

        assert_eq!(assets.path(handle), Some("meshes/a.fake"));
        assert_eq!(assets.handle("meshes/a.fake"), Some(handle));
    }

    #[test]
    fn an_unloaded_handle_goes_stale_rather_than_dangling() {
        // The property generational handles exist for. Reading through a handle
        // whose slot was reused is the bug §2.6 is aimed at.
        let (mut assets, _root) = registry("stale", &[("a.fake", "one")]);

        let handle = assets.load("a.fake").expect("cooked");
        assert_eq!(assets.unload(handle), Some(Fake(String::from("one"))));

        assert!(!assets.contains(handle));
        assert_eq!(assets.get(handle), None);
        assert_eq!(assets.path(handle), None);
    }

    #[test]
    fn unloading_frees_the_name_as_well_as_the_slot() {
        // Leaving the name mapped would make the next `load` hand back a handle
        // pointing at nothing — the registry vouching for a slot it emptied.
        let (mut assets, _root) = registry("reuse-name", &[("a.fake", "one")]);

        let first = assets.load("a.fake").expect("cooked");
        assets.unload(first);

        assert_eq!(assets.handle("a.fake"), None);

        let second = assets.load("a.fake").expect("cooked");
        assert_ne!(first, second, "a fresh load is a fresh handle");
        assert!(assets.contains(second));
        assert!(!assets.contains(first));
    }

    #[test]
    fn a_reload_changes_the_value_behind_a_stable_handle() {
        // The property hot reload is built on: the handle a consumer already
        // holds keeps working and starts pointing at the new bytes.
        let (mut assets, root) = registry("reload", &[("a.fake", "before")]);

        let handle = assets.load("a.fake").expect("cooked");
        assert_eq!(assets.revision(handle), Some(0));

        write(&root, "a.fake", "after");
        assert_eq!(assets.reload("a.fake").expect("recooked"), Some(handle));

        assert_eq!(assets.get(handle).expect("loaded").0, "after");
        assert_eq!(assets.revision(handle), Some(1), "the change is observable");
        assert_eq!(assets.len(), 1, "a reload is not a second asset");
    }

    #[test]
    fn a_failed_reload_keeps_the_old_asset() {
        // Saving a broken file mid-session must log an error, not blank the
        // screen. Decode happens before the replacement for exactly this.
        let (mut assets, root) = registry("reload-broken", &[("a.fake", "good")]);

        let handle = assets.load("a.fake").expect("cooked");
        write(&root, "a.fake", "bad");

        assert!(assets.reload("a.fake").is_err());
        assert_eq!(assets.get(handle).expect("still loaded").0, "good");
        assert_eq!(assets.revision(handle), Some(0), "nothing was replaced");
    }

    #[test]
    fn reloading_something_never_loaded_does_nothing() {
        // A file watcher fires on every change in the tree. Pulling in assets
        // the game never asked for would make touching a file load it.
        let (mut assets, _root) = registry("reload-absent", &[("a.fake", "one")]);

        assert_eq!(assets.reload("a.fake").expect("no-op"), None);
        assert!(assets.is_empty());
    }

    #[test]
    fn a_missing_asset_says_what_kind_and_where() {
        let (mut assets, _root) = registry("missing", &[]);

        let error = assets.load("a.fake").expect_err("not cooked");
        let message = format!("{error}");

        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("a.fake"), "{message}");
        assert!(matches!(error, AssetError::Read { .. }));
    }

    #[test]
    fn bytes_that_do_not_decode_are_a_different_error_from_bytes_that_are_absent() {
        // The fix differs: absent means cook, undecodable means recook. An error
        // that conflated them would send someone the wrong way.
        let (mut assets, _root) = registry("undecodable", &[("a.fake", "bad")]);

        let error = assets.load("a.fake").expect_err("bad bytes");

        assert!(matches!(error, AssetError::Decode { .. }));
        assert!(assets.is_empty(), "a failed load stores nothing");
    }

    #[test]
    fn a_failed_load_can_be_retried_after_the_file_is_fixed() {
        // A failed load must not poison the name. Caching the failure would mean
        // fixing the asset required restarting the game.
        let (mut assets, root) = registry("retry", &[("a.fake", "bad")]);

        assert!(assets.load("a.fake").is_err());

        write(&root, "a.fake", "good");
        let handle = assets.load("a.fake").expect("fixed");

        assert_eq!(assets.get(handle).expect("loaded").0, "good");
    }

    /// Write `contents` and make sure the version moves.
    ///
    /// A test can rewrite a file well inside the filesystem's timestamp
    /// granularity, which is precisely the case `Version` documents itself as
    /// unable to see. Changing the length too is what keeps these tests about
    /// the registry's logic rather than about clock resolution.
    fn rewrite(root: &Path, logical: &str, contents: &str) {
        write(root, logical, contents);
        assert_ne!(
            contents.len(),
            0,
            "an empty rewrite would not move the length"
        );
    }

    #[test]
    fn nothing_reloads_when_nothing_changed() {
        // The common case, and the one that has to be cheap: this runs every
        // frame and must cost a stat per asset and nothing else.
        let (mut assets, _root) = registry("unchanged", &[("a.fake", "one")]);

        assets.load("a.fake").expect("cooked");

        assert!(assets.reload_changed().is_empty());
        assert!(assets.reload_changed().is_empty(), "and again");
    }

    #[test]
    fn a_changed_file_reloads_itself() {
        let (mut assets, root) = registry("poll", &[("a.fake", "one")]);

        let handle = assets.load("a.fake").expect("cooked");
        rewrite(&root, "a.fake", "one plus more");

        let changed = assets.reload_changed();

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, handle);
        assert!(changed[0].1.is_ok());
        assert_eq!(assets.get(handle).expect("loaded").0, "one plus more");
        assert_eq!(assets.revision(handle), Some(1));
    }

    #[test]
    fn a_reloaded_file_does_not_reload_again() {
        // Without recording the new version, every poll after the first change
        // would reload forever — a re-upload to the GPU every frame.
        let (mut assets, root) = registry("poll-once", &[("a.fake", "one")]);

        let handle = assets.load("a.fake").expect("cooked");
        rewrite(&root, "a.fake", "one plus more");

        assert_eq!(assets.reload_changed().len(), 1);
        assert!(assets.reload_changed().is_empty(), "already up to date");
        assert_eq!(assets.revision(handle), Some(1), "reloaded exactly once");
    }

    #[test]
    fn only_the_asset_that_changed_reloads() {
        let (mut assets, root) =
            registry("poll-one-of-two", &[("a.fake", "one"), ("b.fake", "two")]);

        let a = assets.load("a.fake").expect("cooked");
        let b = assets.load("b.fake").expect("cooked");
        rewrite(&root, "b.fake", "two plus more");

        let changed = assets.reload_changed();

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, b);
        assert_eq!(assets.revision(a), Some(0), "a was not touched");
    }

    #[test]
    fn a_file_that_vanishes_is_not_treated_as_a_change() {
        // Editors save by writing a temporary file and renaming over the target,
        // so an asset spends a few milliseconds not existing. Reloading then
        // would fail against a file that is about to be fine.
        let (mut assets, root) = registry("poll-vanished", &[("a.fake", "one")]);

        let handle = assets.load("a.fake").expect("cooked");
        std::fs::remove_file(root.join("a.fake")).expect("removing");

        assert!(assets.reload_changed().is_empty());
        assert_eq!(
            assets.get(handle).expect("still loaded").0,
            "one",
            "the last good value survives"
        );
    }

    #[test]
    fn a_broken_file_is_reported_once_rather_than_every_poll() {
        // `reload` returns before recording anything, so a failure has to be
        // stamped by the poller. Without that, one bad save produces an
        // identical error every frame and no way to tell a new failure from the
        // old one.
        let (mut assets, root) = registry("poll-broken", &[("a.fake", "good")]);

        let handle = assets.load("a.fake").expect("cooked");
        write(&root, "a.fake", "bad");

        let changed = assets.reload_changed();
        assert_eq!(changed.len(), 1);
        assert!(changed[0].1.is_err());

        assert!(assets.reload_changed().is_empty(), "reported once");
        assert_eq!(
            assets.get(handle).expect("still loaded").0,
            "good",
            "and the old asset is still there"
        );
    }

    #[test]
    fn fixing_a_broken_file_reloads_it() {
        // The other half of reporting once: the poller must go quiet, but not
        // deaf. A fix has to be picked up without a restart.
        let (mut assets, root) = registry("poll-fixed", &[("a.fake", "good")]);

        let handle = assets.load("a.fake").expect("cooked");

        write(&root, "a.fake", "bad");
        assert!(assets.reload_changed()[0].1.is_err());

        rewrite(&root, "a.fake", "good again");
        let changed = assets.reload_changed();

        assert_eq!(changed.len(), 1);
        assert!(changed[0].1.is_ok());
        assert_eq!(assets.get(handle).expect("loaded").0, "good again");
    }

    #[test]
    fn iteration_yields_every_loaded_asset() {
        let (mut assets, _root) = registry("iter", &[("a.fake", "one"), ("b.fake", "two")]);

        assets.load("a.fake").expect("cooked");
        assets.load("b.fake").expect("cooked");

        let mut names: Vec<&str> = assets.iter().map(|(_, value)| value.0.as_str()).collect();
        names.sort_unstable();

        assert_eq!(names, vec!["one", "two"]);
    }
}
