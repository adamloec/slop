//! Application layer — main loop, module and plugin wiring, configuration.
//!
//! This is a library, not a framework entry point. `docs/DESIGN.md` §1.2 principle 4:
//! the game owns `main()` and can drive the loop itself. Runnable targets live
//! in `examples/`, and the editor (§2.12) embeds this crate exactly as a
//! shipping game does.
