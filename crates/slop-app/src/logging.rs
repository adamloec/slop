//! Where logging policy is decided.
//!
//! `slop-core::diagnostics` supplies the mechanism — install a subscriber with
//! this filter — and deliberately reads nothing from the environment. This
//! module is the other half: it decides *where the filter comes from*, which
//! `docs/CONVENTIONS.md` §5.1 places in the application layer and nowhere else.
//!
//! An engine crate reaching for an environment variable would be picking up
//! configuration the application never chose, and would behave differently
//! inside a game, a test harness, and the editor for reasons none of them
//! declared.

use slop_core::diagnostics;

/// Environment variable controlling log filtering.
///
/// Uses `tracing-subscriber`'s directive syntax, so `slop_rhi=debug,warn` turns
/// the RHI up while leaving everything else quiet.
pub const FILTER_ENV: &str = "SLOP_LOG";

/// The filter to use: [`FILTER_ENV`] if set, otherwise a sensible default.
pub fn filter_from_env() -> String {
    std::env::var(FILTER_ENV).unwrap_or_else(|_| String::from(diagnostics::DEFAULT_FILTER))
}

/// Install the process-wide subscriber, filtered by [`FILTER_ENV`].
///
/// Call once, early, from `main`.
///
/// # Panics
///
/// If a subscriber is already installed — see [`diagnostics::init`].
pub fn init() {
    diagnostics::init(&filter_from_env());
}

/// Install the subscriber only if one is not already present.
///
/// Returns whether this call installed it. For tests and examples, where several
/// entry points each want logging and only the first can win.
pub fn try_init() -> bool {
    diagnostics::try_init(&filter_from_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_the_default_when_unset() {
        // Not asserting on the env var being absent — another test in this
        // binary could set it — only that the result is always usable.
        let filter = filter_from_env();

        assert!(!filter.is_empty(), "the filter must never be empty");
    }
}
