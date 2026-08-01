//! Scalar transcendental functions with identical results on every platform —
//! `docs/DESIGN.md` §2.14.
//!
//! # Why not `f32::sin`
//!
//! `f32::sin` and its neighbours call the platform's C math library. IEEE-754
//! pins `+`, `-`, `*`, `/` and `sqrt` to be correctly rounded, so those agree
//! everywhere — but it says nothing about `sin`, `exp`, or `powf`. The Windows
//! CRT and glibc are both correct and disagree in the last bit.
//!
//! One bit, once, is nothing. One bit inside a simulation that feeds itself for
//! ten thousand ticks is a replay that diverges and a golden image that fails on
//! one platform, for a reason that looks like a rendering bug and is not.
//!
//! These forward to the `libm` crate — the same Rust source on every target —
//! which is also what `glam` is configured to use, so vector and scalar maths
//! agree with each other as well as across platforms.
//!
//! `clippy.toml` disallows the `std` equivalents in engine code and points here.
//!
//! # What is not here, and why
//!
//! No `sqrt`, `abs`, `floor`, `ceil`, `round`, `trunc`, or `mul_add`. Every one
//! of those is exactly specified by IEEE-754 and compiles to a single
//! instruction; the `std` versions are already identical everywhere and are
//! faster. Wrapping them would cost performance and buy nothing.
//!
//! # Cost
//!
//! `libm` is scalar and portable, so it is slower than a vendor-tuned CRT — on
//! the order of tens of nanoseconds per call. That is irrelevant at the call
//! counts a simulation makes and would matter in a per-pixel loop, which is on
//! the GPU and not subject to any of this.

/// Sine, in radians.
#[inline]
pub fn sin(x: f32) -> f32 {
    libm::sinf(x)
}

/// Cosine, in radians.
#[inline]
pub fn cos(x: f32) -> f32 {
    libm::cosf(x)
}

/// Sine and cosine together.
///
/// Cheaper than two calls, and the pair every rotation needs.
#[inline]
pub fn sin_cos(x: f32) -> (f32, f32) {
    let (sin, cos) = libm::sincosf(x);

    (sin, cos)
}

/// Tangent, in radians.
#[inline]
pub fn tan(x: f32) -> f32 {
    libm::tanf(x)
}

/// Arcsine, in radians.
#[inline]
pub fn asin(x: f32) -> f32 {
    libm::asinf(x)
}

/// Arccosine, in radians.
#[inline]
pub fn acos(x: f32) -> f32 {
    libm::acosf(x)
}

/// Arctangent, in radians.
#[inline]
pub fn atan(x: f32) -> f32 {
    libm::atanf(x)
}

/// Arctangent of `y / x`, using the signs of both to pick the quadrant.
#[inline]
pub fn atan2(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

/// `e` raised to `x`.
#[inline]
pub fn exp(x: f32) -> f32 {
    libm::expf(x)
}

/// Natural logarithm.
#[inline]
pub fn ln(x: f32) -> f32 {
    libm::logf(x)
}

/// Base-2 logarithm.
#[inline]
pub fn log2(x: f32) -> f32 {
    libm::log2f(x)
}

/// Base-10 logarithm.
#[inline]
pub fn log10(x: f32) -> f32 {
    libm::log10f(x)
}

/// `base` raised to `exponent`.
#[inline]
pub fn powf(base: f32, exponent: f32) -> f32 {
    libm::powf(base, exponent)
}

/// The length of the hypotenuse, without the intermediate overflow that
/// `sqrt(x * x + y * y)` suffers for large inputs.
#[inline]
pub fn hypot(x: f32, y: f32) -> f32 {
    libm::hypotf(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tight enough to catch a wrong function, loose enough to say nothing
    /// about the last bit — which is the platform's business and not this
    /// test's.
    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-5
    }

    #[test]
    fn the_functions_are_the_ones_they_claim_to_be() {
        // Guards against a transcription slip — `log2f` where `log10f` was
        // meant compiles, runs, and is wrong.
        assert!(close(sin(0.0), 0.0));
        assert!(close(cos(0.0), 1.0));
        assert!(close(tan(0.0), 0.0));
        assert!(close(asin(1.0), std::f32::consts::FRAC_PI_2));
        assert!(close(acos(1.0), 0.0));
        assert!(close(atan(1.0), std::f32::consts::FRAC_PI_4));
        assert!(close(atan2(1.0, 1.0), std::f32::consts::FRAC_PI_4));
        assert!(close(exp(0.0), 1.0));
        assert!(close(ln(std::f32::consts::E), 1.0));
        assert!(close(log2(8.0), 3.0));
        assert!(close(log10(1000.0), 3.0));
        assert!(close(powf(2.0, 10.0), 1024.0));
        assert!(close(hypot(3.0, 4.0), 5.0));
    }

    #[test]
    fn atan2_takes_y_first() {
        // The argument order every maths library gets asked about. Swapping it
        // is a mirror-image rotation bug that looks like a coordinate
        // convention problem.
        assert!(close(atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2));
        assert!(close(atan2(0.0, 1.0), 0.0));
    }

    #[test]
    fn sin_cos_agrees_with_the_separate_calls() {
        for step in -100..=100 {
            let angle = step as f32 * 0.1;
            let (sin_value, cos_value) = sin_cos(angle);

            assert!(close(sin_value, sin(angle)), "sin at {angle}");
            assert!(close(cos_value, cos(angle)), "cos at {angle}");
        }
    }

    #[test]
    fn results_are_reproducible_within_a_run() {
        // Weak on its own — the cross-platform half cannot be tested from one
        // platform. What this catches is a future change to a function that
        // dispatches on CPU features at runtime, which would break the promise
        // without breaking any other test here.
        for step in 0..1000 {
            let x = step as f32 * 0.01;

            assert_eq!(sin(x), sin(x));
            assert_eq!(exp(x), exp(x));
            assert_eq!(powf(x, 1.5), powf(x, 1.5));
        }
    }
}
