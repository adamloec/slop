//! The approve-and-compare loop around a reference image.

use std::path::{Path, PathBuf};

use crate::compare::{compare, diff_image};
use crate::encode::{decode_png, encode_png};
use crate::{Difference, Rgba8, Tolerance, VerifyError};

/// The environment variable that switches [`Mode`] to [`Update`](Mode::Update).
///
/// Named once here so it cannot drift between test crates, and referenced by
/// [`VerifyError::NoReference`]'s message so a missing golden explains its own
/// fix.
pub(crate) const UPDATE_VARIABLE: &str = "SLOP_UPDATE_GOLDEN";

/// Whether a run checks against references or replaces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Compare against the approved reference and fail on a difference.
    #[default]
    Check,
    /// Overwrite the reference with what was rendered.
    ///
    /// Approving a new image is a **human** decision: this writes whatever it
    /// was given, correct or not. A run in this mode proves nothing, which is
    /// why it is not the default and why nothing infers it from a missing file.
    Update,
}

impl Mode {
    /// Read the mode from the environment.
    ///
    /// `docs/CONVENTIONS.md` §5.1 keeps environment reads out of library
    /// crates, and this is the documented exception: a test binary is an
    /// application in that rule's terms — it owns its own entry point — and the
    /// alternative is every test crate spelling the same variable name and
    /// eventually spelling it differently.
    ///
    /// Any value other than an unset or empty variable means
    /// [`Update`](Self::Update).
    pub fn from_env() -> Self {
        if update_requested() {
            Self::Update
        } else {
            Self::Check
        }
    }
}

/// Whether the environment asks for references to be regenerated.
///
/// Exposed separately from [`Mode::from_env`] so a test harness can report what
/// it is about to do before doing it.
pub fn update_requested() -> bool {
    std::env::var_os(UPDATE_VARIABLE).is_some_and(|value| !value.is_empty())
}

/// One golden-image comparison.
///
/// Every input is a field rather than derived from a name, so a test says
/// exactly which file it compares against and exactly where failures land.
/// Deriving paths from a test name reads well until two crates pick the same
/// name.
#[derive(Debug, Clone)]
pub struct Golden<'a> {
    /// The approved reference PNG. Committed to the repository.
    pub reference: &'a Path,
    /// Directory for the rendered image and the highlighted difference when a
    /// comparison fails. Should be inside `target/`, since its contents are
    /// build output rather than source.
    pub failures: &'a Path,
    /// How much difference is allowed — see [`Tolerance`].
    pub tolerance: Tolerance,
    /// Compare, or replace the reference.
    pub mode: Mode,
}

impl Golden<'_> {
    /// Compare `actual` against the reference, or replace it in
    /// [`Mode::Update`].
    ///
    /// On a failed comparison the rendered image and a highlighted difference
    /// are written under [`failures`](Self::failures) before returning, so the
    /// evidence exists whether or not the test runner captured stdout.
    ///
    /// Returns the measured [`Difference`] on success, so a test can log how
    /// much of its budget it is using rather than only whether it passed.
    ///
    /// # Errors
    ///
    /// Fails if the reference is missing ([`VerifyError::NoReference`]), is a
    /// different size ([`VerifyError::SizeMismatch`]), differs by more than
    /// [`tolerance`](Self::tolerance) ([`VerifyError::Mismatch`]), or cannot be
    /// read or written.
    pub fn check(&self, actual: &Rgba8) -> Result<Difference, VerifyError> {
        if self.mode == Mode::Update {
            encode_png(self.reference, actual)?;

            return Ok(Difference {
                differing: 0,
                total: actual.pixel_count(),
                max_channel: 0,
                worst: None,
            });
        }

        if !self.reference.exists() {
            return Err(VerifyError::NoReference {
                path: self.reference.to_path_buf(),
            });
        }

        let reference = decode_png(self.reference)?;

        if reference.width() != actual.width() || reference.height() != actual.height() {
            return Err(VerifyError::SizeMismatch {
                reference_width: reference.width(),
                reference_height: reference.height(),
                actual_width: actual.width(),
                actual_height: actual.height(),
            });
        }

        let difference = compare(&reference, actual, self.tolerance);

        if difference.is_within(self.tolerance) {
            return Ok(difference);
        }

        let stem = self
            .reference
            .file_stem()
            .map_or_else(|| PathBuf::from("golden"), PathBuf::from);

        let actual_path = self.failures.join(stem.with_extension("actual.png"));
        let diff_path = self.failures.join(stem.with_extension("diff.png"));

        encode_png(&actual_path, actual)?;
        encode_png(&diff_path, &diff_image(&reference, actual, self.tolerance))?;

        Err(VerifyError::Mismatch {
            difference,
            actual: actual_path,
            diff: diff_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("slop-verify-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp directory");

            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn solid(colour: [u8; 4]) -> Rgba8 {
        let pixels = colour.iter().copied().cycle().take(4 * 4 * 4).collect();

        Rgba8::new(4, 4, pixels).expect("well-formed")
    }

    fn golden<'a>(scratch: &'a Scratch, reference: &'a Path, mode: Mode) -> Golden<'a> {
        Golden {
            reference,
            failures: &scratch.0,
            tolerance: Tolerance::EXACT,
            mode,
        }
    }

    #[test]
    fn a_missing_reference_is_an_error_and_not_an_automatic_approval() {
        // The important half of this crate's behaviour: a new test must not
        // pass by writing whatever it happened to render the first time.
        let scratch = Scratch::new("missing-reference");
        let reference = scratch.0.join("absent.png");

        assert!(matches!(
            golden(&scratch, &reference, Mode::Check).check(&solid([1, 2, 3, 4])),
            Err(VerifyError::NoReference { .. })
        ));
        assert!(!reference.exists(), "checking must not create a reference");
    }

    #[test]
    fn update_writes_the_reference_and_check_then_passes() {
        let scratch = Scratch::new("update-then-check");
        let reference = scratch.0.join("solid.png");
        let image = solid([10, 20, 30, 255]);

        golden(&scratch, &reference, Mode::Update)
            .check(&image)
            .expect("update must write");
        assert!(reference.exists());

        golden(&scratch, &reference, Mode::Check)
            .check(&image)
            .expect("the image it just wrote must match");
    }

    #[test]
    fn a_mismatch_writes_both_artifacts_and_names_them() {
        let scratch = Scratch::new("mismatch-artifacts");
        let reference = scratch.0.join("solid.png");

        golden(&scratch, &reference, Mode::Update)
            .check(&solid([0, 0, 0, 255]))
            .expect("update must write");

        match golden(&scratch, &reference, Mode::Check).check(&solid([255, 255, 255, 255])) {
            Err(VerifyError::Mismatch {
                difference,
                actual,
                diff,
            }) => {
                assert_eq!(difference.differing, 16);
                assert!(actual.exists(), "the rendered image must be written");
                assert!(diff.exists(), "the difference must be written");
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_size_change_is_distinguished_from_a_pixel_difference() {
        // Different failures need different responses: a size change means
        // regenerate, a pixel change means investigate.
        let scratch = Scratch::new("size-change");
        let reference = scratch.0.join("solid.png");

        golden(&scratch, &reference, Mode::Update)
            .check(&solid([0, 0, 0, 255]))
            .expect("update must write");

        let taller = Rgba8::new(4, 8, vec![0; 4 * 8 * 4]).expect("well-formed");

        assert!(matches!(
            golden(&scratch, &reference, Mode::Check).check(&taller),
            Err(VerifyError::SizeMismatch {
                reference_height: 4,
                actual_height: 8,
                ..
            })
        ));
    }
}
