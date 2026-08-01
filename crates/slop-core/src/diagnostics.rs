//! Structured logging and profiling markers — `docs/CONVENTIONS.md` §13.
//!
//! The engine emits through the [`tracing`] facade. Engine crates only ever
//! *emit*; installing a subscriber is the application's decision, which is why
//! `init` sits behind the optional `subscriber` feature rather than being
//! always available.
//!
//! Note the deliberate absence of intra-doc links to `init` from anything not
//! itself feature-gated: a link from ungated documentation into a gated item
//! dangles in the default configuration, which `-D warnings` correctly rejects.
//!
//! `tracing` is re-exported here so dependent crates get the macros without
//! adding their own dependency edge, and so the whole engine is guaranteed to
//! be on one version of it.
//!
//! # Emit fields, not sentences
//!
//! ```ignore
//! // Not this — the path cannot be filtered on or aggregated.
//! info!("loaded {} in {}ms", path.display(), ms);
//!
//! // This.
//! info!(asset = %path.display(), duration_ms = ms, "asset loaded");
//! ```
//!
//! # Log the decision, not just the outcome
//!
//! The first bug reports arrive as log files from machines that cannot be
//! inspected. `"selected RTX 5090 (discrete) over Intel UHD 770 (integrated)"`
//! is diagnosable by a stranger; `"device initialized"` is not.
//!
//! # Nothing above `debug` fires per frame
//!
//! A log line in the frame loop is a performance bug that also drowns the
//! signal. Use a `trace`-level span instead, which costs nothing when no
//! subscriber is listening and becomes a profiler region when one is.

/// Re-exported so dependent crates need no `tracing` dependency of their own,
/// and so the engine cannot end up split across two versions of it.
pub use tracing;

/// Environment variable read by `init` to set log filtering, using the standard
/// `tracing-subscriber` syntax — `slop_rhi=debug,warn`.
pub const FILTER_ENV: &str = "SLOP_LOG";

/// Default filter when [`FILTER_ENV`] is unset.
///
/// `info` rather than `warn`: lifecycle events — device selected, module
/// loaded — are what make an unfamiliar machine's log useful, and they are rare
/// enough to cost nothing.
pub const DEFAULT_FILTER: &str = "info";

/// Install the process-wide subscriber.
///
/// Call once, early, from an application — a binary, an example, or a test
/// harness. Never from a library.
///
/// # Panics
///
/// If a global subscriber is already installed. That is a programmer error
/// rather than a runtime condition: it means two places are each claiming to
/// own process-wide configuration, and silently letting the first win would
/// hide the mistake. Tests and other repeat callers want
/// [`init_for_tests`] instead.
#[cfg(feature = "subscriber")]
pub fn init() {
    try_init().expect("a tracing subscriber is already installed for this process");
}

/// Install the subscriber if one is not already present.
///
/// Returns whether this call installed it. Intended for tests, where many cases
/// each want logging available and only the first can win.
#[cfg(feature = "subscriber")]
pub fn init_for_tests() -> bool {
    try_init().is_ok()
}

#[cfg(feature = "subscriber")]
fn try_init() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Targets identify the emitting module, which is what makes per-crate
        // filtering usable.
        .with_target(true)
        .try_init()
}

#[cfg(all(test, feature = "subscriber"))]
mod tests {
    use super::*;

    #[test]
    fn repeated_initialization_is_reported_rather_than_silently_ignored() {
        // Whichever call wins, the second must report that it did not install.
        // A test binary shares one process, so this is the only honest thing to
        // assert about ordering here.
        let first = init_for_tests();
        let second = init_for_tests();

        assert!(
            !(first && second),
            "both calls claimed to install a subscriber"
        );
        assert!(
            !second,
            "the second call must report that one already existed"
        );
    }

    #[test]
    fn default_filter_parses() {
        // Guards against a typo in DEFAULT_FILTER, which would otherwise only
        // surface as silently missing logs on a machine with SLOP_LOG unset.
        assert!(
            tracing_subscriber::EnvFilter::try_new(DEFAULT_FILTER).is_ok(),
            "DEFAULT_FILTER must be a valid filter directive"
        );
    }
}
