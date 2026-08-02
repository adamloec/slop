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

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod cook;
mod gltf_import;
mod texture_import;

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
    },
}

fn main() -> Result<()> {
    // This binary is an application, so it reads the environment and installs
    // the subscriber — `docs/CONVENTIONS.md` §5.1.
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    match Cli::parse().command {
        Command::Cook { root, force } => {
            let context = || format!("cooking assets under {}", root.display());

            let shaders = cook::shaders(&root, force).with_context(context)?;
            let meshes = gltf_import::meshes(&root, force).with_context(context)?;
            let textures = texture_import::textures(&root, force).with_context(context)?;

            println!(
                "cooked {}, up to date {}",
                shaders.cooked + meshes.cooked + textures.cooked,
                shaders.skipped + meshes.skipped + textures.skipped
            );
        }
    }

    Ok(())
}
