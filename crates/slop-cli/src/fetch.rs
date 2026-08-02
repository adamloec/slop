//! Downloading third-party test assets that are too large to commit.
//!
//! Sponza is 51 MB across 71 files. Committing it would put those bytes in git
//! history permanently — every future clone pays for them forever, and history
//! is the one thing a repository cannot take back. So `assets/vendor/` is
//! ignored and this records *how to get* the asset instead of the asset.
//!
//! That trade has a real cost, and it is worth naming: a fresh clone cannot
//! render Sponza until someone runs this, and any test that needs Sponza must
//! skip when it is absent rather than fail. Skipping is a hazard — the golden
//! suite once reported green while the demo refused to start, because every
//! setup failure was treated as a skip. The rule that came out of that applies
//! here too: **a missing vendored asset is the one legitimate skip, checked for
//! by name, and everything else is a failure.**
//!
//! # Why `git` rather than an HTTP client
//!
//! The upstream repository holds every Khronos sample — gigabytes — and offers
//! no per-directory archive. Fetching one model over HTTP means 71 individual
//! URLs, which is 71 chances for a partial download to look like success.
//! A blobless sparse clone asks git for exactly one directory, and git already
//! verifies what it receives. It is also a dependency this project has by
//! definition: the source lives in a git repository.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use slop_core::diagnostics::tracing::info;

/// A third-party asset that can be fetched.
struct Vendored {
    /// What the user types.
    name: &'static str,
    /// Upstream repository.
    repository: &'static str,
    /// The directory within it to check out, and to copy from.
    path: &'static str,
    /// Where it lands under `assets/vendor/`.
    destination: &'static str,
    /// A file that must exist afterwards, so a partial fetch is detectable.
    sentinel: &'static str,
    /// Shown when listing, and worth stating because these carry licences.
    about: &'static str,
}

/// Everything fetchable, which is deliberately a short list.
///
/// Each entry is a third-party asset under its own licence, which is why the
/// upstream `LICENSE.md` is copied alongside the files rather than left behind.
const CATALOGUE: &[Vendored] = &[Vendored {
    name: "sponza",
    repository: "https://github.com/KhronosGroup/glTF-Sample-Assets.git",
    path: "Models/Sponza",
    destination: "sponza",
    sentinel: "Sponza.gltf",
    about: "Crytek Sponza as glTF 2.0 — 103 primitives, 25 materials, 71 files, 51 MB",
}];

/// Print what can be fetched.
pub(crate) fn list() {
    println!("fetchable assets:");
    for asset in CATALOGUE {
        println!("  {:<10} {}", asset.name, asset.about);
    }
    println!("\nfetched into assets/vendor/, which is gitignored.");
}

/// Fetch `name` into `root/assets/vendor/`.
///
/// # Errors
///
/// Fails if the name is unknown, if `git` is unavailable or the clone fails, or
/// if the fetched tree does not contain what the catalogue says it should.
pub(crate) fn fetch(root: &Path, name: &str, force: bool) -> Result<()> {
    let Some(asset) = CATALOGUE.iter().find(|entry| entry.name == name) else {
        let known: Vec<&str> = CATALOGUE.iter().map(|entry| entry.name).collect();
        bail!("unknown asset '{name}'. Known: {}", known.join(", "));
    };

    let destination = root.join("assets").join("vendor").join(asset.destination);

    // Checked by sentinel rather than by directory existence: an interrupted
    // fetch leaves a directory behind, and treating that as "already done"
    // produces a cook failure far from its cause.
    if destination.join(asset.sentinel).exists() && !force {
        info!(asset = asset.name, path = %destination.display(), "already fetched");
        println!("{} is already present. Use --force to refetch.", asset.name);
        return Ok(());
    }

    // Into a sibling of the destination rather than the system temp directory,
    // so the final move is a rename within one filesystem rather than a copy
    // across two — and so an interrupted fetch leaves its debris somewhere the
    // user is already ignoring.
    let staging = root
        .join("assets")
        .join("vendor")
        .join(format!(".{}-staging", asset.name));

    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clearing {}", staging.display()))?;
    }
    std::fs::create_dir_all(staging.parent().expect("staging has a parent"))
        .with_context(|| format!("creating {}", staging.display()))?;

    println!("fetching {} — {}", asset.name, asset.about);

    // `--filter=blob:none` fetches commit and tree objects but no file contents,
    // then `--sparse` narrows the working tree to nothing. Together they turn a
    // multi-gigabyte repository into a metadata-only clone, and the checkout
    // below is what pulls the blobs for one directory.
    git(
        root,
        &[
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            asset.repository,
            &staging.to_string_lossy(),
        ],
    )
    .context("the blobless clone failed — is `git` on PATH, and is the network reachable?")?;

    git(&staging, &["sparse-checkout", "set", asset.path])
        .with_context(|| format!("narrowing the checkout to {}", asset.path))?;

    let source = staging.join(asset.path);
    if !source.is_dir() {
        bail!(
            "upstream no longer has '{}' — the catalogue entry for {} is stale",
            asset.path,
            asset.name
        );
    }

    if destination.exists() {
        std::fs::remove_dir_all(&destination)
            .with_context(|| format!("replacing {}", destination.display()))?;
    }
    std::fs::create_dir_all(&destination)
        .with_context(|| format!("creating {}", destination.display()))?;

    // The model's files sit in a `glTF` subdirectory and its licence sits beside
    // that subdirectory, so both are copied and the nesting is flattened away.
    // A licence left behind upstream is the kind of omission that is invisible
    // until it matters.
    let mut copied = copy_into(&source.join("glTF"), &destination)?;
    for licence in ["LICENSE.md", "README.md"] {
        let from = source.join(licence);
        if from.is_file() {
            std::fs::copy(&from, destination.join(licence))
                .with_context(|| format!("copying {licence}"))?;
            copied += 1;
        }
    }

    std::fs::remove_dir_all(&staging).with_context(|| format!("clearing {}", staging.display()))?;

    if !destination.join(asset.sentinel).exists() {
        bail!(
            "fetched {} files but '{}' is not among them — the catalogue's sentinel is wrong",
            copied,
            asset.sentinel
        );
    }

    info!(asset = asset.name, files = copied, path = %destination.display(), "fetched");
    println!(
        "fetched {copied} files into {}\n\nNow run: cargo run -p slop-cli -- cook",
        destination.display()
    );

    Ok(())
}

/// Copy every file in `from` into `to`, one level deep.
///
/// One level because that is the shape every catalogue entry has, and a
/// recursive copy would silently pull in whatever upstream adds later.
fn copy_into(from: &Path, to: &Path) -> Result<usize> {
    let mut copied = 0;

    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.context("reading a directory entry")?;

        if entry.file_type().context("inspecting an entry")?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))
                .with_context(|| format!("copying {}", entry.path().display()))?;
            copied += 1;
        }
    }

    Ok(copied)
}

/// Run `git` in `directory`, failing with its own diagnostics attached.
///
/// Git reports the interesting part of a failure on stderr and says nothing
/// useful in its exit code, so discarding stderr would turn "no network" and
/// "no such branch" into the same message.
fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .context("could not run `git`")?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalogue_entry_is_addressable() {
        for asset in CATALOGUE {
            assert!(!asset.name.is_empty());
            assert!(asset.repository.starts_with("https://"));
            assert!(
                !asset.sentinel.is_empty(),
                "{} needs a sentinel",
                asset.name
            );
        }
    }

    #[test]
    fn catalogue_names_are_unique() {
        // A duplicate would make `find` silently pick the first, and the second
        // entry would be unreachable rather than an error.
        let mut names: Vec<&str> = CATALOGUE.iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();

        assert_eq!(names.len(), count, "catalogue has duplicate names");
    }

    #[test]
    fn an_unknown_name_is_rejected_by_name() {
        let failure = fetch(Path::new("."), "not-an-asset", false)
            .expect_err("an unknown asset must not be fetched");

        // The message must name what *is* available: a bare "unknown asset" is
        // the kind of error that sends someone to read the source.
        let message = failure.to_string();
        assert!(message.contains("not-an-asset"), "{message}");
        assert!(message.contains("sponza"), "{message}");
    }
}
