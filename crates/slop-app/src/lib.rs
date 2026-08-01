//! Application layer — main loop, module and plugin wiring, configuration.
//!
//! This is a library, not a framework entry point. `docs/DESIGN.md` §1.2
//! principle 4: the game owns `main()` and can drive the loop itself. Runnable
//! targets live in `examples/`, and the editor (§2.12) embeds this crate exactly
//! as a shipping game does.
//!
//! # This is the only layer that reads configuration
//!
//! `docs/CONVENTIONS.md` §5.1: engine crates take parameters, and this crate is
//! what turns files, environment variables, and command-line arguments into
//! those parameters. Nothing below here knows that a config file exists.
//!
//! Keeping that line is what allows a game, the editor, a test harness, and
//! headless CI to each configure the same engine differently without any of them
//! fighting over ambient state.

pub mod logging;
