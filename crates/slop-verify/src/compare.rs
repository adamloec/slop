//! Comparing a render against an approved reference.

use crate::Rgba8;

/// How much a render may differ from its reference and still pass.
///
/// Two thresholds rather than one, because the two failure modes look nothing
/// alike. A driver rounding differently perturbs *every* pixel by one or two
/// levels; a geometry or state regression moves a *few* pixels a long way. A
/// single "average difference" number passes both, which is why there isn't one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tolerance {
    /// Largest absolute per-channel difference before a pixel counts as
    /// differing.
    pub channel: u8,
    /// Largest fraction of pixels that may differ and still pass, in `0.0..=1.0`.
    pub pixels: f32,
}

impl Tolerance {
    /// Every byte identical.
    ///
    /// The correct setting for lavapipe, which is a software rasterizer and so
    /// produces the same bytes on every machine. Using anything looser there
    /// would discard the reason `docs/PLAN.md` §4.1-G chose it.
    pub const EXACT: Self = Self {
        channel: 0,
        pixels: 0.0,
    };

    /// Room for one vendor's rounding, on real hardware.
    ///
    /// Two levels out of 255 covers interpolation and blending differences
    /// between drivers. One percent of pixels covers the edges of triangles,
    /// where coverage rules are specified precisely enough to agree but
    /// rasterizer precision still differs in the last bit.
    ///
    /// Deliberately not the default: a tolerance is a statement about which
    /// renderer produced the reference, so the call site has to say.
    pub const HARDWARE: Self = Self {
        channel: 2,
        pixels: 0.01,
    };
}

/// How two images differ.
///
/// Reported even on success, so a test that passes at 0.9% of the 1% budget can
/// be seen creeping toward the threshold rather than discovered at the moment it
/// crosses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Difference {
    /// Pixels differing by more than [`Tolerance::channel`].
    pub differing: usize,
    /// Pixels compared.
    pub total: usize,
    /// The largest per-channel difference anywhere, tolerance ignored.
    pub max_channel: u8,
    /// Where [`max_channel`](Self::max_channel) was found, as `(x, y)`.
    ///
    /// `None` only for two identical images.
    pub worst: Option<(u32, u32)>,
}

impl Difference {
    /// The fraction of pixels that differ, in `0.0..=1.0`.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }

        self.differing as f32 / self.total as f32
    }

    /// Whether this difference is within `tolerance`.
    pub fn is_within(&self, tolerance: Tolerance) -> bool {
        // `<=` on both, so `Tolerance::EXACT` — zero and zero — passes for two
        // identical images rather than requiring "fewer than zero" pixels.
        self.max_channel <= tolerance.channel && self.fraction() <= tolerance.pixels
    }
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} of {} pixels differ ({:.3}%), largest channel difference {}",
            self.differing,
            self.total,
            self.fraction() * 100.0,
            self.max_channel,
        )?;

        if let Some((x, y)) = self.worst {
            write!(f, " at ({x}, {y})")?;
        }

        Ok(())
    }
}

/// Compare two images of identical dimensions.
///
/// The caller is responsible for the sizes matching; mismatched dimensions are
/// reported separately by [`Golden::check`](crate::Golden::check) because they
/// mean something different from a pixel difference.
pub(crate) fn compare(reference: &Rgba8, actual: &Rgba8, tolerance: Tolerance) -> Difference {
    let mut differing = 0;
    let mut max_channel = 0_u8;
    let mut worst = None;

    for y in 0..reference.height().min(actual.height()) {
        for x in 0..reference.width().min(actual.width()) {
            let (Some(left), Some(right)) = (reference.pixel(x, y), actual.pixel(x, y)) else {
                continue;
            };

            // The pixel's difference is its *worst* channel, not the average of
            // four. A pure red-to-green swap leaves alpha and blue identical, so
            // an average would report half the difference actually present.
            let delta = (0..4)
                .map(|channel| left[channel].abs_diff(right[channel]))
                .max()
                .unwrap_or(0);

            if delta > tolerance.channel {
                differing += 1;
            }

            if delta > max_channel {
                max_channel = delta;
                worst = Some((x, y));
            }
        }
    }

    Difference {
        differing,
        total: reference.pixel_count(),
        max_channel,
        worst,
    }
}

/// Render the difference between two images as something a human can look at.
///
/// Differing pixels become magenta — a colour essentially no renderer produces
/// by accident — over a dimmed greyscale copy of the reference, so *what*
/// changed is visible against *where* in the image it happened. A plain
/// per-channel subtraction produces a near-black image whose interesting pixels
/// are invisible, which is why this does not do that.
pub(crate) fn diff_image(reference: &Rgba8, actual: &Rgba8, tolerance: Tolerance) -> Rgba8 {
    let width = reference.width();
    let height = reference.height();
    let mut pixels = Vec::with_capacity(width as usize * height as usize * Rgba8::CHANNELS);

    for y in 0..height {
        for x in 0..width {
            let left = reference.pixel(x, y).unwrap_or([0, 0, 0, 255]);
            let right = actual.pixel(x, y).unwrap_or([0, 0, 0, 255]);

            let delta = (0..4)
                .map(|channel| left[channel].abs_diff(right[channel]))
                .max()
                .unwrap_or(0);

            if delta > tolerance.channel {
                pixels.extend_from_slice(&[255, 0, 255, 255]);
            } else {
                // Rec. 601 luma, quartered. Integer arithmetic on u32 so the
                // weighted sum cannot overflow a u8 mid-calculation.
                let luma = (u32::from(left[0]) * 299
                    + u32::from(left[1]) * 587
                    + u32::from(left[2]) * 114)
                    / 4000;
                let grey = u8::try_from(luma).unwrap_or(u8::MAX);

                pixels.extend_from_slice(&[grey, grey, grey, 255]);
            }
        }
    }

    Rgba8::new(width, height, pixels).expect("built from the reference's own dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, colour: [u8; 4]) -> Rgba8 {
        let pixels = colour
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * Rgba8::CHANNELS)
            .collect();

        Rgba8::new(width, height, pixels).expect("well-formed")
    }

    #[test]
    fn identical_images_pass_exact() {
        let image = solid(4, 4, [10, 20, 30, 255]);
        let difference = compare(&image, &image, Tolerance::EXACT);

        assert_eq!(difference.differing, 0);
        assert_eq!(difference.max_channel, 0);
        assert_eq!(difference.worst, None);
        assert!(difference.is_within(Tolerance::EXACT));
    }

    #[test]
    fn one_level_of_drift_fails_exact_and_passes_hardware() {
        // The lavapipe-versus-real-GPU case, in miniature: every pixel off by
        // one. Exact must reject it, hardware must not.
        let reference = solid(4, 4, [10, 20, 30, 255]);
        let actual = solid(4, 4, [11, 20, 30, 255]);

        assert!(!compare(&reference, &actual, Tolerance::EXACT).is_within(Tolerance::EXACT));
        assert!(compare(&reference, &actual, Tolerance::HARDWARE).is_within(Tolerance::HARDWARE));
    }

    #[test]
    fn a_few_pixels_moving_a_long_way_fails_hardware() {
        // The regression case: 1 pixel in 16 is inside the 1% budget by count
        // alone, but the channel difference is enormous. This is why there are
        // two thresholds and not an average.
        let reference = solid(4, 4, [0, 0, 0, 255]);
        let mut pixels = reference.pixels().to_vec();
        pixels[0..4].copy_from_slice(&[255, 255, 255, 255]);
        let actual = Rgba8::new(4, 4, pixels).expect("well-formed");

        let difference = compare(&reference, &actual, Tolerance::HARDWARE);

        assert_eq!(difference.differing, 1);
        assert_eq!(difference.max_channel, 255);
        assert_eq!(difference.worst, Some((0, 0)));
        assert!(!difference.is_within(Tolerance::HARDWARE));
    }

    #[test]
    fn the_worst_channel_decides_a_pixel_not_the_average() {
        // Red swapped for green: two channels differ by 255, two are identical.
        // An average would report 127 and pass tolerances this must fail.
        let reference = solid(1, 1, [255, 0, 0, 255]);
        let actual = solid(1, 1, [0, 255, 0, 255]);

        assert_eq!(
            compare(&reference, &actual, Tolerance::EXACT).max_channel,
            255
        );
    }

    #[test]
    fn the_diff_image_marks_only_pixels_outside_tolerance() {
        let reference = solid(2, 1, [0, 0, 0, 255]);
        let mut pixels = reference.pixels().to_vec();
        pixels[4..8].copy_from_slice(&[255, 255, 255, 255]);
        let actual = Rgba8::new(2, 1, pixels).expect("well-formed");

        let diff = diff_image(&reference, &actual, Tolerance::EXACT);

        assert_eq!(
            diff.pixel(0, 0),
            Some([0, 0, 0, 255]),
            "matching stays grey"
        );
        assert_eq!(
            diff.pixel(1, 0),
            Some([255, 0, 255, 255]),
            "differing is magenta"
        );
    }
}
