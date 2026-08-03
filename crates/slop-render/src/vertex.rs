//! Turning what a shader says it reads into what a pipeline binds.
//!
//! `slop-asset` carries the reflection and knows nothing about Vulkan —
//! deliberately, since it is also read by tools that have no GPU. This is where
//! the two meet.

use slop_asset::Reflection;
use slop_asset::shader::VertexFormat;
use slop_rhi::{Format, VertexLayout};

use crate::RenderError;

/// A vertex layout derived from a cooked shader's reflection.
///
/// Owns the attribute array so [`VertexBinding::layout`] can hand out a
/// [`VertexLayout`] borrowing it — `VertexLayout` is a view, and something has
/// to hold what it views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexBinding {
    attributes: Vec<(Format, u32)>,
    stride: u32,
}

impl VertexBinding {
    /// Derive the layout for one tightly packed, interleaved vertex buffer.
    ///
    /// This is what replaces a hand-written attribute table beside every shader.
    /// The failure it removes is not a compile error and never was: a field added
    /// to the shader's input struct and not to the Rust table makes the GPU read
    /// the previous vertex's data, and the symptom is geometry that looks
    /// scrambled.
    ///
    /// # Errors
    ///
    /// [`RenderError::VertexLocationGap`] if the shader's locations are not
    /// `0..n`. Vulkan allows sparse locations; [`VertexLayout`] does not express
    /// them, because its attribute array is positional. Refused rather than
    /// silently packed down, which would bind every attribute to the wrong
    /// location.
    pub fn interleaved(reflection: &Reflection) -> Result<Self, RenderError> {
        let (placed, stride) = reflection.interleaved();
        let mut attributes = Vec::with_capacity(placed.len());

        for (index, input) in placed.iter().enumerate() {
            let expected = index as u32;

            if input.location != expected {
                return Err(RenderError::VertexLocationGap {
                    expected,
                    found: input.location,
                });
            }

            attributes.push((vulkan_format(input.format), input.offset));
        }

        Ok(Self { attributes, stride })
    }

    /// The layout, for a [`GraphicsPipelineConfig`](slop_rhi::GraphicsPipelineConfig).
    pub fn layout(&self) -> VertexLayout<'_> {
        VertexLayout {
            stride: self.stride,
            attributes: &self.attributes,
        }
    }

    /// Bytes between consecutive vertices.
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Whether the shader reads no vertex attributes at all.
    ///
    /// True for a shader generating its positions from `SV_VertexID`, which is
    /// what the triangle does. Such a pipeline wants no vertex layout rather than
    /// an empty one.
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }
}

/// The Vulkan format for one reflected input type.
const fn vulkan_format(format: VertexFormat) -> Format {
    match format {
        VertexFormat::Float32 => Format::R32Float,
        VertexFormat::Float32x2 => Format::Rg32Float,
        VertexFormat::Float32x3 => Format::Rgb32Float,
        VertexFormat::Float32x4 => Format::Rgba32Float,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_asset::shader::VertexInput;

    fn cube() -> Reflection {
        Reflection {
            push_constant_bytes: 136,
            vertex_inputs: vec![
                VertexInput {
                    location: 0,
                    format: VertexFormat::Float32x3,
                },
                VertexInput {
                    location: 1,
                    format: VertexFormat::Float32x3,
                },
                VertexInput {
                    location: 2,
                    format: VertexFormat::Float32x2,
                },
            ],
            // A graphics shader: vertex and fragment, no compute stage.
            thread_group: None,
        }
    }

    #[test]
    fn the_cube_layout_matches_what_was_hand_written() {
        // The exact table `examples/cube/src/mesh.rs` used to hold, now derived.
        // If these ever disagree, one of the two is wrong and it is no longer
        // possible to tell which by reading.
        let binding = VertexBinding::interleaved(&cube()).expect("contiguous");
        let layout = binding.layout();

        assert_eq!(layout.stride, 32);
        assert_eq!(
            layout.attributes,
            &[
                (Format::Rgb32Float, 0),
                (Format::Rgb32Float, 12),
                (Format::Rg32Float, 24),
            ]
        );
    }

    #[test]
    fn a_shader_with_no_inputs_binds_nothing() {
        let binding = VertexBinding::interleaved(&Reflection::default()).expect("valid");

        assert!(binding.is_empty());
        assert_eq!(binding.stride(), 0);
    }

    #[test]
    fn a_gap_in_the_locations_is_refused_rather_than_packed_down() {
        // `VertexLayout`'s attribute array is positional, so an input at
        // location 2 with nothing at 1 would silently bind as location 1 — every
        // attribute after the gap reading the wrong data.
        let sparse = Reflection {
            push_constant_bytes: 0,
            vertex_inputs: vec![
                VertexInput {
                    location: 0,
                    format: VertexFormat::Float32x3,
                },
                VertexInput {
                    location: 2,
                    format: VertexFormat::Float32x2,
                },
            ],
            thread_group: None,
        };

        assert!(matches!(
            VertexBinding::interleaved(&sparse),
            Err(RenderError::VertexLocationGap {
                expected: 1,
                found: 2
            })
        ));
    }

    #[test]
    fn every_format_maps_to_the_matching_vulkan_one() {
        // Component count is what a wrong mapping gets wrong, and the symptom is
        // a shader reading a garbage channel rather than an error.
        for (format, expected) in [
            (VertexFormat::Float32, Format::R32Float),
            (VertexFormat::Float32x2, Format::Rg32Float),
            (VertexFormat::Float32x3, Format::Rgb32Float),
            (VertexFormat::Float32x4, Format::Rgba32Float),
        ] {
            assert_eq!(vulkan_format(format), expected, "{format:?}");
        }
    }
}
