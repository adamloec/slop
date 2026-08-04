//! Finding the source assets under a directory.
//!
//! Every importer starts by walking a tree and keeping the files it recognises,
//! and each had written its own copy of that walk: `.gltf` and `.glb` in
//! [`gltf_import`](crate::gltf_import), `.png` in
//! [`texture_import`](crate::texture_import), `.slang` in
//! [`shader_import`](crate::shader_import), and every file for the include
//! digest. Four copies of one loop, differing in the line that decides what to
//! keep.
//!
//! `docs/PLAN.md` §6.1's rule is that a third copy is the signal to extract, so
//! this is overdue rather than early — the environment importer would have been
//! the fifth.
//!
//! The differences that were real are the two fields of [`Sources`]: which
//! extensions count, and whether a directory is skipped whole. Everything else
//! was the same loop written four times, including the two spellings of the same
//! error context.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What a walk is looking for.
///
/// A struct rather than two arguments, for `docs/CONVENTIONS.md` §5.1's reason:
/// the next importer with a different rule adds a field here instead of a
/// parameter to every call.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Sources<'a> {
    /// Extensions to keep, without the dot. Empty keeps **every** file.
    ///
    /// Empty is not a degenerate case: the shader cooker digests every file
    /// under its include directory, because an include may be a `.h` of shared
    /// constants or a generated table and any of them changing changes what the
    /// shaders compile to.
    pub extensions: &'a [&'a str],

    /// A directory name to skip entirely, wherever it appears.
    ///
    /// The shader cooker's includes are the only user: those files are
    /// `#include`d by others and declare no entry points, so compiling one
    /// standalone fails.
    pub skip: Option<&'a str>,
}

/// Every matching file under `directory`, recursively.
///
/// Appends rather than returning, so a caller collecting from several roots
/// accumulates into one list — and so the recursion has nothing to merge.
///
/// **The order is the filesystem's**, which is not stable across platforms.
/// Every caller sorts, and that is not incidental: a cook that visits sources in
/// a different order on Linux and Windows would report its summary differently
/// and, where one artifact's cook reads another's output, could produce different
/// bytes.
///
/// # Errors
///
/// Fails if a directory cannot be read.
pub(crate) fn collect(
    directory: &Path,
    wanted: &Sources<'_>,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry in {}", directory.display()))?
            .path();

        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| wanted.skip.is_some_and(|skipped| name == skipped))
            {
                continue;
            }

            collect(&path, wanted, found)?;
            continue;
        }

        if wanted.extensions.is_empty() {
            found.push(path);
            continue;
        }

        let matches = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| wanted.extensions.contains(&extension));

        if matches {
            found.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small tree under a fresh directory, removed when the test ends.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("slop-sources-{name}"));

            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("nested")).expect("creating the tree");
            std::fs::create_dir_all(root.join("lib")).expect("creating the tree");

            for (path, contents) in [
                ("one.png", "a"),
                ("two.gltf", "b"),
                ("notes.txt", "c"),
                ("nested/three.png", "d"),
                ("lib/shared.slang", "e"),
            ] {
                std::fs::write(root.join(path), contents).expect("writing a file");
            }

            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The collected file names, sorted.
    ///
    /// Sorted **after** dropping the directories, not before: `nested/three.png`
    /// sorts ahead of `one.png` as a path and behind it as a name, and the
    /// assertions below are about which files were found rather than about the
    /// walk's order — which this module's documentation says is the
    /// filesystem's and not to be relied on.
    fn names(found: Vec<PathBuf>) -> Vec<String> {
        let mut names: Vec<String> = found
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("a collected path is a file")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        names.sort();
        names
    }

    #[test]
    fn only_the_named_extensions_are_kept() {
        let tree = Tree::new("extensions");
        let mut found = Vec::new();

        collect(
            &tree.root,
            &Sources {
                extensions: &["png"],
                skip: None,
            },
            &mut found,
        )
        .expect("walking the tree");

        assert_eq!(names(found), vec!["one.png", "three.png"]);
    }

    #[test]
    fn several_extensions_are_all_kept() {
        // The glTF importer's case: two extensions naming one kind of source.
        let tree = Tree::new("several");
        let mut found = Vec::new();

        collect(
            &tree.root,
            &Sources {
                extensions: &["png", "gltf"],
                skip: None,
            },
            &mut found,
        )
        .expect("walking the tree");

        assert_eq!(names(found), vec!["one.png", "three.png", "two.gltf"]);
    }

    #[test]
    fn no_extensions_means_every_file() {
        // Not a degenerate case — it is what the shader cooker digests its
        // include directory with, and a walk that quietly kept nothing would
        // make every shader's cache key blind to its includes. That exact bug is
        // why the key builder demands a label; this is the other half of it.
        let tree = Tree::new("every");
        let mut found = Vec::new();

        collect(&tree.root, &Sources::default(), &mut found).expect("walking the tree");

        assert_eq!(
            names(found),
            vec![
                "notes.txt",
                "one.png",
                "shared.slang",
                "three.png",
                "two.gltf"
            ]
        );
    }

    #[test]
    fn a_skipped_directory_is_not_descended_into() {
        let tree = Tree::new("skip");
        let mut found = Vec::new();

        collect(
            &tree.root,
            &Sources {
                extensions: &["slang"],
                skip: Some("lib"),
            },
            &mut found,
        )
        .expect("walking the tree");

        assert!(found.is_empty(), "found {found:?}");
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_it() {
        let failure = collect(
            Path::new("does/not/exist"),
            &Sources::default(),
            &mut Vec::new(),
        )
        .expect_err("a missing directory cannot be walked");

        assert!(failure.to_string().contains("does"), "{failure}");
    }
}
