//! The source environment image — an equirectangular panorama of radiance.
//!
//! `docs/PLAN.md` §9.7 E6a. What an HDR environment arrives as, and the only
//! source format the environment cooker reads.
//!
//! # Why Radiance `.hdr` and not OpenEXR
//!
//! `docs/DESIGN.md` §3's write/take line, applied honestly in both directions.
//! BC7 was **taken** because a compressor is a serious project — eight modes,
//! partition tables, endpoint fitting — with no bearing on the engine's
//! architecture. An RGBE decoder is a text header, a scanline loop and a
//! run-length case, and every part of it is checkable against known bytes. It is
//! written.
//!
//! OpenEXR is the opposite of both: half and float variants, scanline and tiled
//! layouts, multi-part files, and four compression codecs. Nothing here is worth
//! owning, and it arrives as a dependency when a source asset demands one rather
//! than in advance.
//!
//! # RGBE, and what it costs
//!
//! Each pixel is four bytes: a mantissa per channel and one **shared** exponent.
//! That buys a dynamic range no eight-bit format has, at the cost of the three
//! channels being quantised against the largest of them — so a deeply saturated
//! colour loses precision in its small channels. For an environment map, whose
//! whole job is a smooth field of radiance, that is invisible; for a texture it
//! would not be, which is part of why this decodes to `f32` here and is never
//! offered as a texture format.
//!
//! # The mapping is a decision, not a convention
//!
//! [`Panorama::direction_at`] and [`uv_of`] are inverses, and which way they face
//! is stated rather than left to be inferred. Getting it wrong rotates or mirrors
//! the whole environment, which reads as "the HDR is odd" rather than as a bug —
//! so `a_direction_survives_the_round_trip` is what holds them together.

use anyhow::{Context, Result, bail};
use slop_math::{Vec3, scalar};

/// Radians in a full turn.
const TAU: f32 = std::f32::consts::TAU;

/// An equirectangular image of linear radiance.
///
/// Row zero is the **top** — the `+Y` pole — matching the `-Y` in a Radiance
/// resolution line, which is what every `.hdr` in circulation writes.
pub(crate) struct Panorama {
    /// Width in pixels. Twice the height, for a full sphere.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Linear radiance, row-major from the top.
    pub texels: Vec<Vec3>,
}

impl std::fmt::Debug for Panorama {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panorama")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Where a direction lands in the panorama, as `(u, v)` in 0..1.
///
/// `u` runs from the `-Z` axis — `slop_math::FORWARD`, the way a camera looks by
/// default — eastward through `+X`. `v` runs from the `+Y` pole down, so `v = 0`
/// is the top row, which is where the file's first scanline is.
#[must_use]
pub(crate) fn uv_of(direction: Vec3) -> (f32, f32) {
    let direction = direction.normalize_or_zero();

    // `atan2(x, -z)` rather than `atan2(z, x)`: the zero of the longitude is the
    // forward axis, so an environment's "front" is what a default camera faces.
    let longitude = scalar::atan2(direction.x, -direction.z);
    let latitude = scalar::acos(direction.y.clamp(-1.0, 1.0));

    (longitude / TAU + 0.5, latitude / std::f32::consts::PI)
}

impl Panorama {
    /// Which direction the texel at `(u, v)` looks along.
    ///
    /// The inverse of [`uv_of`], and an associated function rather than a method
    /// because it depends on nothing but the mapping.
    ///
    /// **Test-only for now**, which is the honest state rather than a permanent
    /// one. Nothing in the cooker goes from a texel to a direction — the
    /// projection only ever asks the other question — and this exists because a
    /// mapping with one direction implemented cannot be checked against
    /// anything. See `a_direction_survives_the_round_trip`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn direction_at(u: f32, v: f32) -> Vec3 {
        let longitude = (u - 0.5) * TAU;
        let latitude = v * std::f32::consts::PI;

        let (sin_longitude, cos_longitude) = scalar::sin_cos(longitude);
        let (sin_latitude, cos_latitude) = scalar::sin_cos(latitude);

        Vec3::new(
            sin_latitude * sin_longitude,
            cos_latitude,
            -sin_latitude * cos_longitude,
        )
    }

    /// The radiance arriving from `direction`, bilinearly filtered.
    ///
    /// Wrapped in longitude and clamped in latitude, which is what the two axes
    /// actually are: the seam at `u = 1` continues into `u = 0` and must filter
    /// across it, while the poles have nothing beyond them. Clamping the wrong
    /// one puts a visible seam down the back of every environment.
    #[must_use]
    pub(crate) fn sample(&self, direction: Vec3) -> Vec3 {
        let (u, v) = uv_of(direction);

        // Texel centres sit at (i + 0.5) / size, so the sample position in pixel
        // space is offset by half a texel. Without this every environment is
        // shifted by half a pixel, which is invisible until it is compared
        // against something.
        let x = u.mul_add(self.width as f32, -0.5);
        let y = v.mul_add(self.height as f32, -0.5);

        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;

        let x0 = x0 as i64;
        let y0 = y0 as i64;

        let top = self.texel(x0, y0).lerp(self.texel(x0 + 1, y0), fx);
        let bottom = self.texel(x0, y0 + 1).lerp(self.texel(x0 + 1, y0 + 1), fx);

        top.lerp(bottom, fy)
    }

    /// One texel, with longitude wrapped and latitude clamped.
    fn texel(&self, x: i64, y: i64) -> Vec3 {
        let width = i64::from(self.width);
        let height = i64::from(self.height);

        // `rem_euclid` rather than `%`: the remainder of a negative number is
        // negative in Rust, and indexing with it would panic rather than wrap.
        let x = x.rem_euclid(width) as usize;
        let y = y.clamp(0, height - 1) as usize;

        self.texels[y * self.width as usize + x]
    }

    /// Decode a Radiance `.hdr` file.
    ///
    /// # Errors
    ///
    /// Fails if the magic, the `FORMAT` line or the resolution line is not one
    /// this understands, or if the pixel data ends early.
    pub(crate) fn decode_radiance(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor { bytes, at: 0 };

        let magic = cursor.line().context("reading the Radiance signature")?;
        if !magic.starts_with("#?") {
            bail!(
                "not a Radiance file: expected a line starting '#?', found {:?}",
                truncated(&magic)
            );
        }

        // The header is free-form variable assignments and comments, ended by an
        // empty line. Only FORMAT is load-bearing.
        let mut format = None;
        loop {
            let line = cursor.line().context("reading the Radiance header")?;
            if line.is_empty() {
                break;
            }

            if let Some(value) = line.strip_prefix("FORMAT=") {
                format = Some(value.trim().to_owned());
            }
        }

        match format.as_deref() {
            Some("32-bit_rle_rgbe") => {}
            // Not a near-miss to be tolerated: XYZE is CIE tristimulus, so
            // decoding it as RGB gives a plausible image in the wrong colour
            // space — which looks like a grading choice rather than a bug.
            Some(other) => bail!("Radiance format '{other}' is not RGBE; only 32-bit_rle_rgbe"),
            None => bail!("the Radiance header declares no FORMAT"),
        }

        let resolution = cursor.line().context("reading the resolution line")?;
        let (width, height) = parse_resolution(&resolution)?;

        let mut texels = Vec::with_capacity(width as usize * height as usize);
        let mut scanline = vec![Rgbe::default(); width as usize];

        for row in 0..height {
            cursor
                .scanline(&mut scanline)
                .with_context(|| format!("reading scanline {row} of {height}"))?;

            texels.extend(scanline.iter().copied().map(Rgbe::to_linear));
        }

        Ok(Self {
            width,
            height,
            texels,
        })
    }
}

/// `-Y height +X width`, and nothing else.
///
/// Radiance permits eight orientations by flipping either sign; every file in
/// circulation writes this one, and the rest are refused **by name** rather than
/// decoded upside down. A silently flipped environment lights a scene from below.
fn parse_resolution(line: &str) -> Result<(u32, u32)> {
    let fields: Vec<&str> = line.split_whitespace().collect();

    let ["-Y", height, "+X", width] = fields[..] else {
        bail!(
            "unsupported Radiance resolution line {:?}; only '-Y <height> +X <width>'",
            truncated(line)
        );
    };

    let height: u32 = height
        .parse()
        .with_context(|| format!("the height {height:?} is not a number"))?;
    let width: u32 = width
        .parse()
        .with_context(|| format!("the width {width:?} is not a number"))?;

    if width == 0 || height == 0 {
        bail!("a {width}x{height} panorama has no pixels");
    }

    Ok((width, height))
}

/// A pixel as the file stores it: three mantissas and a shared exponent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rgbe {
    r: u8,
    g: u8,
    b: u8,
    e: u8,
}

impl Rgbe {
    /// The linear radiance this encodes.
    ///
    /// An exponent of zero is **exactly** black by the format's definition, not
    /// a very small number — so it is tested for rather than fed through the
    /// arithmetic, which would otherwise scale a mantissa of zero by `2^-136`
    /// and arrive at the same place more slowly.
    fn to_linear(self) -> Vec3 {
        if self.e == 0 {
            return Vec3::ZERO;
        }

        // The mantissas are the fraction's high bits, so the scale is
        // 2^(e - 128) over a mantissa scaled by 1/256.
        let scale = exp2i(i32::from(self.e) - 136);

        Vec3::new(
            f32::from(self.r) * scale,
            f32::from(self.g) * scale,
            f32::from(self.b) * scale,
        )
    }
}

/// Two raised to an integer power, exactly.
///
/// Built from the exponent field rather than computed, for two reasons.
/// `powf` would route a transcendental through `libm` to produce a number that
/// is exact by construction, and `powi(2.0, -136)` overflows on the way — it
/// evaluates the reciprocal of `2^136`, which is past `f32::MAX`, and returns
/// zero for a value that is merely small.
///
/// Below the smallest normal the result is flushed to zero. The largest radiance
/// that can reach is `255 * 2^-127`, which is around `1.5e-36`, and no tone
/// mapping ever built distinguishes that from black.
fn exp2i(exponent: i32) -> f32 {
    if exponent < -126 {
        return 0.0;
    }

    if exponent > 127 {
        return f32::INFINITY;
    }

    f32::from_bits(((exponent + 127) as u32) << 23)
}

/// Reads lines and scanlines out of the file.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    /// The next newline-terminated line, without its terminator.
    fn line(&mut self) -> Result<String> {
        let start = self.at;

        while self.at < self.bytes.len() && self.bytes[self.at] != b'\n' {
            self.at += 1;
        }

        if self.at >= self.bytes.len() {
            bail!("the file ends inside a header line");
        }

        let line = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
        self.at += 1;

        Ok(line.trim_end_matches('\r').to_owned())
    }

    /// The next byte.
    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.at)
            .context("the pixel data ends early")?;
        self.at += 1;

        Ok(byte)
    }

    /// One scanline of pixels, in whichever of the three encodings it uses.
    fn scanline(&mut self, into: &mut [Rgbe]) -> Result<()> {
        let width = into.len();

        // Adaptive RLE cannot describe these widths, so a file at one of them is
        // flat by definition and the four bytes below would be misread as a
        // header.
        if !(8..=0x7fff).contains(&width) {
            return self.flat(into, 0);
        }

        let header = [self.byte()?, self.byte()?, self.byte()?, self.byte()?];
        let declared = (usize::from(header[2]) << 8) | usize::from(header[3]);

        if header[0] != 2 || header[1] != 2 || declared != width {
            // Not an adaptive scanline, so those four bytes were its first
            // pixel. Handed back rather than re-read, because the old encoding
            // needs the previous pixel and rewinding would lose it.
            into[0] = Rgbe {
                r: header[0],
                g: header[1],
                b: header[2],
                e: header[3],
            };

            return self.flat(into, 1);
        }

        // Adaptive RLE stores the scanline as four separate planes, each
        // run-length encoded on its own. That is what makes it worth doing: the
        // exponent is near-constant across a scanline and compresses to almost
        // nothing, which it would not if the channels were interleaved.
        for channel in 0..4 {
            let mut written = 0;

            while written < width {
                let count = self.byte()?;

                if count > 128 {
                    // A run: one value, repeated.
                    let run = usize::from(count) - 128;
                    let value = self.byte()?;

                    if written + run > width {
                        bail!("a run of {run} overruns the scanline");
                    }

                    for pixel in &mut into[written..written + run] {
                        set_channel(pixel, channel, value);
                    }

                    written += run;
                } else {
                    // A literal span. Zero is not a valid count — it would make
                    // no progress and the loop would never end.
                    let span = usize::from(count);
                    if span == 0 {
                        bail!("a literal span of zero would never terminate");
                    }

                    if written + span > width {
                        bail!("a literal span of {span} overruns the scanline");
                    }

                    for pixel in &mut into[written..written + span] {
                        set_channel(pixel, channel, self.byte()?);
                    }

                    written += span;
                }
            }
        }

        Ok(())
    }

    /// A scanline with no adaptive encoding, from `start` onwards.
    ///
    /// Still not simply four bytes per pixel: the **old** run-length encoding
    /// lives here, as a pixel whose three mantissas are all one. Handling it is
    /// ten lines; not handling it would decode such a file into garbage with no
    /// error, which is the outcome worth spending ten lines to avoid.
    fn flat(&mut self, into: &mut [Rgbe], start: usize) -> Result<()> {
        let mut at = start;
        // Consecutive markers shift the count by a byte each time, which is how
        // the old encoding names a run longer than 255.
        let mut shift = 0;

        while at < into.len() {
            let pixel = Rgbe {
                r: self.byte()?,
                g: self.byte()?,
                b: self.byte()?,
                e: self.byte()?,
            };

            if pixel.r == 1 && pixel.g == 1 && pixel.b == 1 {
                if at == 0 {
                    bail!("a repeat marker at the start of a scanline has nothing to repeat");
                }

                let run = usize::from(pixel.e) << (8 * shift);
                let previous = into[at - 1];

                if at + run > into.len() {
                    bail!("a repeat of {run} overruns the scanline");
                }

                for slot in &mut into[at..at + run] {
                    *slot = previous;
                }

                at += run;
                shift += 1;
            } else {
                into[at] = pixel;
                at += 1;
                shift = 0;
            }
        }

        Ok(())
    }
}

/// Write one channel of a pixel, for the plane-at-a-time adaptive decoder.
fn set_channel(pixel: &mut Rgbe, channel: usize, value: u8) {
    match channel {
        0 => pixel.r = value,
        1 => pixel.g = value,
        2 => pixel.b = value,
        _ => pixel.e = value,
    }
}

/// A prefix of `line`, for an error that must not paste a whole file into a log.
fn truncated(line: &str) -> String {
    line.chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direction_survives_the_round_trip() {
        // The property that holds the two halves of the mapping together. A sign
        // wrong in either one mirrors the whole environment, which reads as the
        // source being odd rather than as a bug — and mirrored lighting is
        // exactly as plausible as correct lighting when nothing is compared.
        for direction in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Z,
            Vec3::NEG_Z,
            Vec3::new(0.3, 0.5, -0.8).normalize(),
            Vec3::new(-0.6, -0.2, 0.4).normalize(),
        ] {
            let (u, v) = uv_of(direction);
            let back = Panorama::direction_at(u, v);

            assert!(
                (back - direction).length() < 1e-5,
                "{direction:?} became {back:?} through (u, v) = ({u}, {v})"
            );
        }
    }

    #[test]
    fn the_forward_axis_is_the_middle_of_the_panorama() {
        // Which way an environment faces is a decision, and this is where it is
        // written down: the centre column looks along `slop_math::FORWARD`.
        let (u, v) = uv_of(slop_math::FORWARD);

        assert!((u - 0.5).abs() < 1e-6, "forward is at u = {u}, not 0.5");
        assert!((v - 0.5).abs() < 1e-6, "the horizon is at v = {v}, not 0.5");
    }

    #[test]
    fn the_top_row_is_the_up_pole() {
        // Row zero is the top, matching `-Y` in the resolution line. Getting
        // this upside down flips the sky and the ground, and a scene lit from
        // below looks like a normal-mapping bug rather than an import one.
        let (_, top) = uv_of(Vec3::Y);
        let (_, bottom) = uv_of(Vec3::NEG_Y);

        assert!(top < 1e-6, "up is at v = {top}");
        assert!((bottom - 1.0).abs() < 1e-6, "down is at v = {bottom}");
    }

    /// A file with `width * height` pixels, uncompressed.
    fn flat_file(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n");
        bytes.extend_from_slice(format!("-Y {height} +X {width}\n").as_bytes());

        for _ in 0..width * height {
            bytes.extend_from_slice(&pixel);
        }

        bytes
    }

    #[test]
    fn an_uncompressed_file_decodes() {
        // Width under eight, so the adaptive path is not even considered.
        let bytes = flat_file(4, 2, [128, 64, 32, 128]);
        let decoded = Panorama::decode_radiance(&bytes).expect("a valid flat file");

        assert_eq!(decoded.width, 4);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.texels.len(), 8);

        // e = 128 means a scale of 2^-8, so a mantissa of 128 is exactly 0.5.
        assert!((decoded.texels[0].x - 0.5).abs() < 1e-6);
        assert!((decoded.texels[0].y - 0.25).abs() < 1e-6);
        assert!((decoded.texels[0].z - 0.125).abs() < 1e-6);
    }

    #[test]
    fn a_zero_exponent_is_exactly_black() {
        // The format's definition, and worth a test because the arithmetic would
        // otherwise produce something merely very small — which survives a
        // comparison against zero and then shows up as a floor that is not quite
        // black in a scene lit only by the environment.
        let bytes = flat_file(4, 1, [255, 255, 255, 0]);
        let decoded = Panorama::decode_radiance(&bytes).expect("a valid flat file");

        assert_eq!(decoded.texels[0], Vec3::ZERO);
    }

    /// A file whose single scanline is adaptively run-length encoded.
    fn rle_file(width: u32, pixel: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n");
        bytes.extend_from_slice(format!("-Y 1 +X {width}\n").as_bytes());

        // The adaptive header: 2, 2, then the width as two big-endian bytes.
        bytes.extend_from_slice(&[2, 2, (width >> 8) as u8, (width & 0xff) as u8]);

        // Each channel as one run covering the whole scanline.
        for channel in pixel {
            bytes.push(128 + width as u8);
            bytes.push(channel);
        }

        bytes
    }

    #[test]
    fn an_adaptively_encoded_scanline_decodes() {
        let decoded = Panorama::decode_radiance(&rle_file(16, [64, 128, 255, 129]))
            .expect("a valid adaptive file");

        assert_eq!(decoded.texels.len(), 16);

        // e = 129 is a scale of 2^-7, so 64 is 0.5.
        for texel in &decoded.texels {
            assert!((texel.x - 0.5).abs() < 1e-6, "{texel:?}");
            assert!((texel.y - 1.0).abs() < 1e-6, "{texel:?}");
        }
    }

    #[test]
    fn the_two_encodings_agree_on_the_same_pixels() {
        // The one comparison that says the adaptive decoder is decoding rather
        // than merely running: the same image both ways must produce the same
        // texels. A plane-ordering mistake passes every test above and fails
        // this one.
        let pixel = [200, 100, 50, 130];

        let flat = Panorama::decode_radiance(&flat_file(16, 1, pixel)).expect("flat");
        let rle = Panorama::decode_radiance(&rle_file(16, pixel)).expect("adaptive");

        assert_eq!(flat.texels, rle.texels);
    }

    #[test]
    fn the_old_run_length_encoding_is_decoded_rather_than_misread() {
        // A pixel of (1, 1, 1, n) repeats its predecessor n times. Ignoring it
        // would decode such a file into garbage with no error at all, which is
        // the whole reason it is handled.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n");
        bytes.extend_from_slice(b"-Y 1 +X 4\n");
        bytes.extend_from_slice(&[128, 64, 32, 128]);
        bytes.extend_from_slice(&[1, 1, 1, 3]);

        let decoded = Panorama::decode_radiance(&bytes).expect("a valid old-RLE file");

        assert_eq!(decoded.texels.len(), 4);
        assert_eq!(decoded.texels[3], decoded.texels[0]);
    }

    #[test]
    fn something_that_is_not_a_radiance_file_is_refused() {
        let failure = Panorama::decode_radiance(b"not an hdr\n\n-Y 1 +X 1\n")
            .expect_err("the signature is wrong");

        assert!(failure.to_string().contains("Radiance"), "{failure}");
    }

    #[test]
    fn a_colour_space_that_is_not_rgb_is_refused_by_name() {
        // XYZE decodes cleanly and means something else. Accepting it silently
        // would give a plausible image in the wrong colour space.
        let failure =
            Panorama::decode_radiance(b"#?RADIANCE\nFORMAT=32-bit_rle_xyze\n\n-Y 1 +X 1\n")
                .expect_err("XYZE is not RGBE");

        assert!(failure.to_string().contains("xyze"), "{failure}");
    }

    #[test]
    fn an_unsupported_orientation_is_refused_rather_than_flipped() {
        let failure =
            Panorama::decode_radiance(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n+Y 1 +X 1\n")
                .expect_err("+Y is not the supported orientation");

        assert!(failure.to_string().contains("resolution line"), "{failure}");
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_padded() {
        let mut bytes = flat_file(4, 2, [128, 64, 32, 128]);
        bytes.truncate(bytes.len() - 6);

        assert!(Panorama::decode_radiance(&bytes).is_err());
    }

    #[test]
    fn an_empty_file_is_refused_rather_than_panicking() {
        assert!(Panorama::decode_radiance(&[]).is_err());
        assert!(Panorama::decode_radiance(b"#?RADIANCE").is_err());
    }

    #[test]
    fn sampling_wraps_in_longitude_and_clamps_in_latitude() {
        // The seam at u = 1 continues into u = 0 and must filter across it;
        // the poles have nothing beyond them. Clamping the wrong one puts a
        // visible seam down the back of every environment.
        let panorama = Panorama {
            width: 4,
            height: 2,
            texels: vec![Vec3::ONE; 8],
        };

        // Straight up and straight down are past the outermost texel centres in
        // v, so these only return anything finite if latitude is clamped.
        assert_eq!(panorama.sample(Vec3::Y), Vec3::ONE);
        assert_eq!(panorama.sample(Vec3::NEG_Y), Vec3::ONE);
        assert_eq!(panorama.sample(slop_math::FORWARD), Vec3::ONE);
    }

    /// The fetched panorama, or `None` on a checkout that has not fetched it.
    ///
    /// **Skipped by name**, for the reason `examples/model/tests/golden.rs`
    /// gives at length: a blanket skip once let that suite report green while
    /// the demo refused to start. Only the vendored file may be absent.
    fn fetched() -> Option<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/vendor/helipad/helipad.hdr");

        if !path.is_file() {
            eprintln!("skipping: run `cargo run -p slop-cli -- fetch helipad`");
            return None;
        }

        Some(std::fs::read(&path).expect("a fetched panorama is readable"))
    }

    #[test]
    fn a_real_panorama_has_its_sky_above_and_its_sun_above_the_horizon() {
        // The one assumption synthetic data cannot check. Every other test here
        // is self-consistent: it builds a panorama with this module's own
        // convention and reads it back with the same one, so a decoder that had
        // `-Y` backwards — storing scanlines bottom-to-top — would pass all of
        // them and turn every environment upside down.
        //
        // Two facts about outdoor content, neither specific to this file:
        // the upper hemisphere carries more light than the lower, and the
        // brightest thing in the image is the sun, which is above the horizon.
        //
        // **Hemispheres, not the top and bottom rows.** Those were the first
        // attempt and they are wrong: the top row is the zenith, which at golden
        // hour is deep blue and dimmer than the sunlit ground at the nadir. On
        // this file it reads 0.42 against 0.55 — a failure that says nothing
        // about the decoder. The hemispheres read 1.04 against 0.43.
        let Some(bytes) = fetched() else {
            return;
        };

        let panorama = Panorama::decode_radiance(&bytes).expect("the fetched file is a panorama");
        let equator = panorama.height / 2;

        let row = |y: u32| -> f32 {
            let start = y as usize * panorama.width as usize;
            let end = start + panorama.width as usize;

            panorama.texels[start..end]
                .iter()
                .map(|texel| texel.x + texel.y + texel.z)
                .sum::<f32>()
                / panorama.width as f32
        };

        let upper: f32 = (0..equator).map(row).sum::<f32>() / equator as f32;
        let lower: f32 = (equator..panorama.height).map(row).sum::<f32>() / equator as f32;

        assert!(
            upper > lower * 1.5,
            "the sky hemisphere averages {upper} and the ground {lower} — the \
             panorama is upside down, which means `-Y` is being read backwards"
        );

        let (brightest, _) = panorama
            .texels
            .iter()
            .enumerate()
            .max_by(|left, right| {
                let sum = |texel: &Vec3| texel.x + texel.y + texel.z;
                sum(left.1).total_cmp(&sum(right.1))
            })
            .expect("a panorama has texels");

        let sun_row = brightest as u32 / panorama.width;

        assert!(
            sun_row < equator,
            "the brightest texel is at row {sun_row} of {}, which is below the \
             horizon — a sun underground means the rows are reversed",
            panorama.height
        );
    }

    #[test]
    fn a_sampled_gradient_follows_the_direction_it_was_built_from() {
        // Sampling a constant image says nothing about where it read. This
        // builds a panorama whose value *is* its column index, so the sample
        // lands where the mapping says it should.
        let width = 8;
        let texels = (0..width)
            .map(|x| Vec3::splat(x as f32))
            .collect::<Vec<_>>();

        let panorama = Panorama {
            width: width as u32,
            height: 1,
            texels,
        };

        // Column 4's centre is u = 4.5 / 8, which is the direction below.
        let direction = Panorama::direction_at(4.5 / 8.0, 0.5);

        assert!(
            (panorama.sample(direction).x - 4.0).abs() < 1e-4,
            "read {:?}",
            panorama.sample(direction)
        );
    }
}
