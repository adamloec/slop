//! Reading and writing golden images as PNG.
//!
//! PNG because it is lossless — the entire point is comparing exact bytes — and
//! because it renders in a browser, a diff viewer, and a pull request without
//! any tooling. A raw dump would be simpler to write and impossible to look at,
//! and looking at it is what a failed golden test needs first.

use std::path::Path;

use png::{BitDepth, ColorType, Compression, Decoder, Encoder};

use crate::{Rgba8, VerifyError};

/// Write an image to `path`, creating parent directories as needed.
///
/// # Errors
///
/// Fails if the directory cannot be created or the file cannot be written.
pub fn encode_png(path: &Path, image: &Rgba8) -> Result<(), VerifyError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| VerifyError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let file = std::fs::File::create(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut encoder = Encoder::new(std::io::BufWriter::new(file), image.width(), image.height());
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    // These files live in the repository forever and are written once. Trading
    // encode time for a smaller committed artifact is the right way round.
    encoder.set_compression(Compression::High);

    let png_error = |source: png::EncodingError| VerifyError::Png {
        path: path.to_path_buf(),
        source: Box::new(source),
    };

    let mut writer = encoder.write_header().map_err(png_error)?;
    writer.write_image_data(image.pixels()).map_err(png_error)?;
    // Explicit rather than left to `Drop`, which cannot report a failure — and
    // a truncated golden image that nothing complained about is precisely the
    // kind of silent wrong this crate exists to prevent.
    writer.finish().map_err(png_error)?;

    Ok(())
}

/// Read an 8-bit RGBA image from `path`.
///
/// # Errors
///
/// Fails if the file is missing, is not a PNG, or is not 8-bit RGBA.
/// Non-RGBA PNGs are rejected rather than converted: a golden image is written
/// by [`encode_png`], so a different format means the file was replaced by
/// something else, and silently converting it would hide that.
pub fn decode_png(path: &Path) -> Result<Rgba8, VerifyError> {
    let file = std::fs::File::open(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let png_error = |source: png::DecodingError| VerifyError::Png {
        path: path.to_path_buf(),
        source: Box::new(source),
    };

    let mut reader = Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(png_error)?;

    let mut pixels = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut pixels).map_err(png_error)?;

    if info.color_type != ColorType::Rgba || info.bit_depth != BitDepth::Eight {
        return Err(VerifyError::Png {
            path: path.to_path_buf(),
            source: format!(
                "expected 8-bit RGBA, found {:?} at {:?}",
                info.color_type, info.bit_depth
            )
            .into(),
        });
    }

    // `next_frame` may decode fewer bytes than the buffer holds when the file
    // is an APNG or is truncated; trusting the buffer's length rather than the
    // frame's would compare against uninitialized zeroes.
    pixels.truncate(info.buffer_size());

    Rgba8::new(info.width, info.height, pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinctive gradient — the failure a solid colour would not catch is
    /// rows or channels being transposed on the way through the codec.
    fn gradient(width: u32, height: u32) -> Rgba8 {
        let mut pixels = Vec::new();

        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[
                    u8::try_from(x % 256).unwrap_or(0),
                    u8::try_from(y % 256).unwrap_or(0),
                    u8::try_from((x + y) % 256).unwrap_or(0),
                    255,
                ]);
            }
        }

        Rgba8::new(width, height, pixels).expect("well-formed")
    }

    #[test]
    fn an_image_survives_a_round_trip_byte_for_byte() {
        let directory = std::env::temp_dir().join("slop-verify-round-trip");
        let path = directory.join("gradient.png");
        let original = gradient(17, 13);

        encode_png(&path, &original).expect("encoding must succeed");
        let decoded = decode_png(&path).expect("decoding must succeed");

        assert_eq!(decoded, original);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_file_is_reported_as_io_not_as_a_codec_failure() {
        let path = std::env::temp_dir().join("slop-verify-definitely-absent.png");
        let _ = std::fs::remove_file(&path);

        assert!(matches!(decode_png(&path), Err(VerifyError::Io { .. })));
    }

    #[test]
    fn a_file_that_is_not_a_png_is_rejected() {
        let directory = std::env::temp_dir().join("slop-verify-not-a-png");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let path = directory.join("actually-text.png");
        std::fs::write(&path, b"this is not a PNG").expect("write");

        assert!(matches!(decode_png(&path), Err(VerifyError::Png { .. })));

        let _ = std::fs::remove_dir_all(&directory);
    }
}
