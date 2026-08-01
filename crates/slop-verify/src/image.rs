//! An 8-bit RGBA image, in the layout a GPU readback produces.

use crate::VerifyError;

/// Tightly packed 8-bit RGBA pixels, row-major, top row first.
///
/// The same layout as a `VK_FORMAT_R8G8B8A8_UNORM` image copied to a buffer
/// with zero `bufferRowLength`, which is not a coincidence: the whole point is
/// that readback bytes become an image with no conversion step that could
/// itself be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba8 {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Rgba8 {
    /// Bytes per pixel.
    pub const CHANNELS: usize = 4;

    /// Wrap pixel data, checking it against the dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::MalformedImage`] if `pixels` is not exactly
    /// `width * height * 4` bytes. Checked rather than asserted because the
    /// caller is usually holding a buffer whose size came from the driver, and
    /// a length mismatch there is a real diagnostic rather than a programmer
    /// error.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, VerifyError> {
        let expected = width as usize * height as usize * Self::CHANNELS;

        if pixels.len() != expected {
            return Err(VerifyError::MalformedImage {
                width,
                height,
                expected,
                found: pixels.len(),
            });
        }

        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// All pixels, row-major.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// How many pixels there are.
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    /// One pixel's four channels, or `None` if out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let start = (y as usize * self.width as usize + x as usize) * Self::CHANNELS;

        self.pixels
            .get(start..start + Self::CHANNELS)
            .and_then(|slice| slice.try_into().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_and_length_must_agree() {
        assert!(Rgba8::new(2, 2, vec![0; 16]).is_ok());
        assert!(matches!(
            Rgba8::new(2, 2, vec![0; 15]),
            Err(VerifyError::MalformedImage {
                expected: 16,
                found: 15,
                ..
            })
        ));
    }

    #[test]
    fn pixels_are_addressed_row_major() {
        // Two rows of two: the second row starts at byte 8, so (0, 1) is the
        // third pixel. Getting this backwards transposes every diff report,
        // which is exactly the kind of wrong that looks plausible.
        let mut pixels = vec![0_u8; 16];
        pixels[8..12].copy_from_slice(&[1, 2, 3, 4]);

        let image = Rgba8::new(2, 2, pixels).expect("well-formed");

        assert_eq!(image.pixel(0, 1), Some([1, 2, 3, 4]));
        assert_eq!(image.pixel(0, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn out_of_bounds_reads_are_none_not_wrapped() {
        // The failure this guards against: `x = width` folding into the next
        // row's first pixel and comparing successfully against the wrong thing.
        let image = Rgba8::new(2, 2, vec![0; 16]).expect("well-formed");

        assert_eq!(image.pixel(2, 0), None);
        assert_eq!(image.pixel(0, 2), None);
    }
}
