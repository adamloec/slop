//! The Slop engine's command-line tool.
//!
//! `docs/DESIGN.md` §4 scopes this as "build, cook, run, inspect, test", and §7
//! counts tooling among the things that make this a platform rather than an
//! engine. Today it cooks shaders; the other verbs arrive with the subsystems
//! they serve.
//!
//! This is an application, so `anyhow` is used rather than typed errors — the
//! distinction `docs/CONVENTIONS.md` §6 draws. Nothing here is a library
//! surface, and a person reading a failure wants the chain of context, not a
//! variant to match on.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

mod fetch;

/// Slop engine tooling.
#[derive(Debug, Parser)]
#[command(name = "slop", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile source assets into the runtime cache.
    ///
    /// Reads `shaders/` and `assets/`, writing into `.slop/cache/`.
    /// Incremental: work whose inputs have not changed is skipped.
    Cook {
        /// Project root holding `shaders/` and `assets/`. Defaults to the
        /// current directory.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Recook everything, ignoring the cache.
        #[arg(long)]
        force: bool,

        /// Keep running, recooking whatever changes.
        ///
        /// The authoring half of hot reload: this rewrites cooked artifacts, and
        /// a running game notices through `Assets::reload_changed`. The two
        /// halves are separate processes on purpose — `docs/DESIGN.md` §2.8
        /// keeps source parsing out of anything that ships, so the engine never
        /// links a shader compiler or a glTF parser.
        #[arg(long)]
        watch: bool,
    },

    /// Download a third-party test asset into `assets/vendor/`.
    ///
    /// That directory is gitignored: Sponza alone is 51 MB, and a binary that
    /// large is permanent once it is in git history. The repository records how
    /// to get these rather than the bytes themselves — see `fetch::CATALOGUE`.
    Fetch {
        /// Which asset. Omit to list what is available.
        name: Option<String>,

        /// Project root holding `assets/`. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Refetch even if the asset is already present.
        #[arg(long)]
        force: bool,
    },
}

/// How often `--watch` looks for changed sources.
///
/// Short enough that saving a file and alt-tabbing feels immediate, long enough
/// that the tree walk is not constant. See §6.1 on why this polls at all.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

fn main() -> Result<()> {
    // This binary is an application, so it reads the environment and installs
    // the subscriber — `docs/CONVENTIONS.md` §5.1.
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    match Cli::parse().command {
        Command::Cook { root, force, watch } => {
            let summary = slop_cook::all(&root, force)?;
            println!("cooked {}, up to date {}", summary.cooked, summary.skipped);

            if watch {
                return watch_and_cook(&root);
            }
        }

        Command::Fetch { name, root, force } => match name {
            Some(name) => fetch::fetch(&root, &name, force)?,
            None => fetch::list(),
        },
    }

    Ok(())
}

/// Recook on a timer until interrupted.
///
/// **This polls rather than subscribing to filesystem events**, and that is a
/// deliberate first cut recorded in `docs/PLAN.md` §6.1. It costs a tree walk
/// and a hash of every source four times a second, which is nothing for this
/// project and wrong for a large one. The reason to start here is that the cache
/// already decides what needs doing: a poll that finds nothing changed does no
/// work beyond the walk, so correctness comes from the same code path as a
/// one-shot cook rather than from the watcher being right about what changed. An
/// event-driven watcher replaces the loop and touches nothing else.
///
/// A cook that fails is reported and the loop continues. Exiting on a syntax
/// error would mean restarting the watcher every time a shader is mid-edit,
/// which is most of the time it is being watched.
fn watch_and_cook(root: &Path) -> Result<()> {
    println!("watching {} — ctrl-c to stop", root.display());

    loop {
        std::thread::sleep(WATCH_INTERVAL);

        match slop_cook::all(root, false) {
            Ok(summary) if summary.cooked == 0 => {}
            Ok(summary) => println!("recooked {}", summary.cooked),
            // `{error:#}` renders the whole `anyhow` context chain on one line,
            // which for a shader error is the file, the stage and the message.
            Err(error) => eprintln!("cook failed: {error:#}"),
        }
    }
}
