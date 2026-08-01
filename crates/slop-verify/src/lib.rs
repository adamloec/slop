//! Verification infrastructure: golden images and the harness around them.
//!
//! `docs/DESIGN.md` §5 treats verification as a subsystem rather than as
//! something tests each reinvent, for a specific reason: when code is produced
//! faster than it can be reviewed line by line, automated truth is the only
//! thing preventing large volumes of subtly wrong architecture.
//!
//! # Why this is a crate and not a test helper
//!
//! A `tests/support` module cannot be shared across crates, and golden images
//! are wanted by `slop-rhi` now and by `slop-render` at M3. More importantly the
//! *policy* — what tolerance means, where failures are written, how a reference
//! is approved — should have one answer, not one per test binary.
//!
//! It is a normal library rather than something behind `cfg(test)` so that
//! `slop-cli` can eventually drive the same comparison outside `cargo test`.
//! Consumers depend on it under `[dev-dependencies]`, so nothing it pulls in
//! reaches a shipped game.
//!
//! # Two tiers, one reference format
//!
//! `docs/PLAN.md` §4.1-G splits golden images in two: hosted CI renders through
//! lavapipe, a CPU rasterizer that is bit-deterministic and vendor-independent,
//! and compares by [`Tolerance::EXACT`]; a separate opt-in lane renders on real
//! hardware, where driver differences in interpolation and rounding make a small
//! tolerance necessary.
//!
//! Both tiers use the same comparison and the same file format. The difference
//! is which reference is loaded and how much slack it is given, which is why
//! [`Tolerance`] is a parameter rather than a constant.
//!
//! # Configuration
//!
//! Per `docs/CONVENTIONS.md` §5.1, library crates take parameters and do not
//! read the environment. [`Golden`] follows that: every input is a field. The
//! single exception is [`update_requested`], which is documented as the test
//! harness's application boundary and exists so the variable's name is written
//! down once rather than in every test crate.
//!
//! ```no_run
//! use std::path::Path;
//! use slop_verify::{Golden, Mode, Rgba8, Tolerance};
//!
//! # fn render() -> Rgba8 { Rgba8::new(1, 1, vec![0; 4]).unwrap() }
//! let actual = render();
//!
//! Golden {
//!     reference: Path::new("tests/golden/triangle.png"),
//!     failures: Path::new("target/golden-failures"),
//!     tolerance: Tolerance::EXACT,
//!     mode: Mode::from_env(),
//! }
//! .check(&actual)
//! .expect("the render must match its approved reference");
//! ```

mod compare;
mod encode;
mod golden;
mod image;

pub use compare::{Difference, Tolerance};
pub use encode::{decode_png, encode_png};
pub use golden::{Golden, Mode, update_requested};
pub use image::Rgba8;

use thiserror::Error;

/// Anything verification can fail at.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The approved reference does not exist.
    ///
    /// Carries the command that would create it, because "file not found" for a
    /// golden image is nearly always a new test rather than a broken one.
    #[error(
        "no approved reference at {path}; run the test with {}=1 to create one, \
         then inspect it before committing",
        golden::UPDATE_VARIABLE
    )]
    NoReference {
        /// Where the reference was expected.
        path: std::path::PathBuf,
    },

    /// The rendered image is a different size from the reference.
    ///
    /// Separated from a pixel mismatch because it is never a rendering
    /// regression — it means the test changed its resolution, and the reference
    /// needs regenerating rather than investigating.
    #[error(
        "size changed: reference is {reference_width}x{reference_height}, \
         render is {actual_width}x{actual_height}"
    )]
    SizeMismatch {
        /// Width of the approved reference.
        reference_width: u32,
        /// Height of the approved reference.
        reference_height: u32,
        /// Width of the render under test.
        actual_width: u32,
        /// Height of the render under test.
        actual_height: u32,
    },

    /// The render differs from the reference by more than the tolerance allows.
    #[error("{difference}; wrote {actual} and {diff}")]
    Mismatch {
        /// How they differ.
        difference: Difference,
        /// Where the rendered image was written for inspection.
        actual: std::path::PathBuf,
        /// Where the highlighted difference was written.
        diff: std::path::PathBuf,
    },

    /// Pixel data does not match the stated dimensions.
    #[error("{width}x{height} needs {expected} bytes of RGBA8, got {found}")]
    MalformedImage {
        /// Stated width.
        width: u32,
        /// Stated height.
        height: u32,
        /// Bytes the dimensions imply.
        expected: usize,
        /// Bytes actually supplied.
        found: usize,
    },

    /// A PNG could not be read or written.
    #[error("PNG at {path} could not be processed")]
    Png {
        /// The file involved.
        path: std::path::PathBuf,
        /// What the codec reported.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A file could not be read or written.
    #[error("{path} could not be accessed")]
    Io {
        /// The file involved.
        path: std::path::PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}
