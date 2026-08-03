//! Pixel and vertex-attribute formats, and the image aspects they imply.
//!
//! An owned enumeration rather than a re-export of `vk::Format`. `docs/DESIGN.md`
//! §2.2 bought this crate on the promise that a second backend "slots in
//! cleanly", and that promise is only kept if the layers above name types this
//! crate defines. A consumer writing `vk::Format::R8G8B8A8_UNORM` has hardcoded
//! Vulkan into a renderer that is supposed to be backend-agnostic — explicit,
//! which §2.2 wanted, but also leaking the backend's type system, which is a
//! different property.
//!
//! Deliberately small: every variant here is one the engine actually uses. A
//! format nothing names is a format nothing has decided about, and the mapping
//! to a second backend is the place that decision would surface.

use ash::vk;

/// A pixel or vertex-attribute format.
///
/// Names follow the channel-then-type convention (`Rgba8Unorm`) rather than
/// Vulkan's underscore-separated spelling, because this is the engine's
/// vocabulary and not a mirror of one API's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// No format. The state a swapchain is in before it is first built.
    Undefined,

    /// Four 8-bit channels, unsigned normalized. Linear data — normal maps,
    /// metallic-roughness, anything the shader must not see gamma-decoded.
    Rgba8Unorm,
    /// Four 8-bit channels, sRGB-encoded. Colour the shader wants linearized on
    /// read, which is albedo and little else.
    Rgba8Srgb,
    /// Four 8-bit channels in BGRA order, sRGB-encoded. The order most desktop
    /// swapchains prefer.
    Bgra8Srgb,

    /// One 32-bit float.
    R32Float,
    /// Two 32-bit floats. UVs.
    Rg32Float,
    /// Three 32-bit floats. Positions and normals.
    Rgb32Float,
    /// Four 32-bit floats. Tangents, with the handedness in `w`.
    Rgba32Float,

    /// BC7 block compression, unsigned normalized. The fixed feature tier's
    /// texture format — see `docs/DESIGN.md` §2.7.
    Bc7Unorm,

    /// 16-bit unsigned normalized depth. Required of every conformant device,
    /// so this is the floor rather than a choice.
    D16Unorm,
    /// 32-bit float depth. What reversed-Z wants, per `docs/DESIGN.md` §2.7.
    D32Float,
    /// 16-bit depth with 8 bits of stencil.
    D16UnormS8Uint,
    /// 24-bit unsigned normalized depth with 8 bits of stencil.
    D24UnormS8Uint,
    /// 32-bit float depth with 8 bits of stencil.
    D32FloatS8Uint,
    /// 24 bits of depth in a 32-bit word, no stencil.
    X8D24UnormPack32,
    /// 8 bits of stencil, no depth.
    S8Uint,
}

impl Format {
    /// The Vulkan format this maps to. The escape hatch — see [`crate::handle`].
    #[must_use]
    pub fn to_vk(self) -> vk::Format {
        match self {
            Self::Undefined => vk::Format::UNDEFINED,
            Self::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            Self::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
            Self::Bgra8Srgb => vk::Format::B8G8R8A8_SRGB,
            Self::R32Float => vk::Format::R32_SFLOAT,
            Self::Rg32Float => vk::Format::R32G32_SFLOAT,
            Self::Rgb32Float => vk::Format::R32G32B32_SFLOAT,
            Self::Rgba32Float => vk::Format::R32G32B32A32_SFLOAT,
            Self::Bc7Unorm => vk::Format::BC7_UNORM_BLOCK,
            Self::D16Unorm => vk::Format::D16_UNORM,
            Self::D32Float => vk::Format::D32_SFLOAT,
            Self::D16UnormS8Uint => vk::Format::D16_UNORM_S8_UINT,
            Self::D24UnormS8Uint => vk::Format::D24_UNORM_S8_UINT,
            Self::D32FloatS8Uint => vk::Format::D32_SFLOAT_S8_UINT,
            Self::X8D24UnormPack32 => vk::Format::X8_D24_UNORM_PACK32,
            Self::S8Uint => vk::Format::S8_UINT,
        }
    }

    /// The format a Vulkan one maps to, or `None` when the engine has no name
    /// for it.
    ///
    /// Fallible on purpose. The alternative — an `Other(vk::Format)` variant —
    /// would re-open the leak this type exists to close, and would let a format
    /// nothing has reasoned about reach a pipeline. The one caller that can
    /// encounter an arbitrary format is swapchain surface-format selection, and
    /// "the driver offered only formats we do not understand" is a real error
    /// rather than something to paper over.
    pub(crate) fn from_vk(format: vk::Format) -> Option<Self> {
        Some(match format {
            vk::Format::UNDEFINED => Self::Undefined,
            vk::Format::R8G8B8A8_UNORM => Self::Rgba8Unorm,
            vk::Format::R8G8B8A8_SRGB => Self::Rgba8Srgb,
            vk::Format::B8G8R8A8_SRGB => Self::Bgra8Srgb,
            vk::Format::R32_SFLOAT => Self::R32Float,
            vk::Format::R32G32_SFLOAT => Self::Rg32Float,
            vk::Format::R32G32B32_SFLOAT => Self::Rgb32Float,
            vk::Format::R32G32B32A32_SFLOAT => Self::Rgba32Float,
            vk::Format::BC7_UNORM_BLOCK => Self::Bc7Unorm,
            vk::Format::D16_UNORM => Self::D16Unorm,
            vk::Format::D32_SFLOAT => Self::D32Float,
            vk::Format::D16_UNORM_S8_UINT => Self::D16UnormS8Uint,
            vk::Format::D24_UNORM_S8_UINT => Self::D24UnormS8Uint,
            vk::Format::D32_SFLOAT_S8_UINT => Self::D32FloatS8Uint,
            vk::Format::X8_D24_UNORM_PACK32 => Self::X8D24UnormPack32,
            vk::Format::S8_UINT => Self::S8Uint,
            _ => return None,
        })
    }

    /// Whether this format carries depth.
    #[must_use]
    pub fn has_depth(self) -> bool {
        matches!(
            self,
            Self::D16Unorm
                | Self::D32Float
                | Self::D16UnormS8Uint
                | Self::D24UnormS8Uint
                | Self::D32FloatS8Uint
                | Self::X8D24UnormPack32
        )
    }

    /// Whether this format carries stencil.
    #[must_use]
    pub fn has_stencil(self) -> bool {
        matches!(
            self,
            Self::S8Uint | Self::D16UnormS8Uint | Self::D24UnormS8Uint | Self::D32FloatS8Uint
        )
    }
}

/// Which parts of an image a barrier or view refers to.
///
/// A depth-stencil image has two aspects and a barrier covering it must name
/// both, which is the mistake [`aspect_of`] exists to make unmakeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAspect {
    /// Colour data.
    Color,
    /// Depth only.
    Depth,
    /// Stencil only.
    Stencil,
    /// Depth and stencil together.
    DepthStencil,
}

impl ImageAspect {
    /// The Vulkan aspect mask this maps to. The escape hatch — see
    /// [`crate::handle`].
    #[must_use]
    pub fn to_vk(self) -> vk::ImageAspectFlags {
        match self {
            Self::Color => vk::ImageAspectFlags::COLOR,
            Self::Depth => vk::ImageAspectFlags::DEPTH,
            Self::Stencil => vk::ImageAspectFlags::STENCIL,
            Self::DepthStencil => vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL,
        }
    }
}

/// The aspects a format implies.
///
/// Both aspects must appear in a barrier covering a depth-stencil image, or the
/// transition is incomplete.
#[must_use]
pub fn aspect_of(format: Format) -> ImageAspect {
    match (format.has_depth(), format.has_stencil()) {
        (true, true) => ImageAspect::DepthStencil,
        (true, false) => ImageAspect::Depth,
        (false, true) => ImageAspect::Stencil,
        (false, false) => ImageAspect::Color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant survives the round trip. Guards the two matches drifting
    /// apart, which is the one way a hand-written mapping goes wrong.
    #[test]
    fn every_format_round_trips_through_vulkan() {
        const ALL: [Format; 16] = [
            Format::Undefined,
            Format::Rgba8Unorm,
            Format::Rgba8Srgb,
            Format::Bgra8Srgb,
            Format::R32Float,
            Format::Rg32Float,
            Format::Rgb32Float,
            Format::Rgba32Float,
            Format::Bc7Unorm,
            Format::D16Unorm,
            Format::D32Float,
            Format::D16UnormS8Uint,
            Format::D24UnormS8Uint,
            Format::D32FloatS8Uint,
            Format::X8D24UnormPack32,
            Format::S8Uint,
        ];

        for format in ALL {
            assert_eq!(
                Format::from_vk(format.to_vk()),
                Some(format),
                "{format:?} did not survive the round trip"
            );
        }
    }

    #[test]
    fn a_format_the_engine_does_not_name_has_no_mapping() {
        assert_eq!(Format::from_vk(vk::Format::R4G4_UNORM_PACK8), None);
    }

    #[test]
    fn depth_stencil_formats_report_both_aspects() {
        assert_eq!(aspect_of(Format::D32FloatS8Uint), ImageAspect::DepthStencil);
        assert_eq!(aspect_of(Format::D32Float), ImageAspect::Depth);
        assert_eq!(aspect_of(Format::S8Uint), ImageAspect::Stencil);
        assert_eq!(aspect_of(Format::Rgba8Unorm), ImageAspect::Color);
    }

    /// The aspect a depth format implies is what a barrier over it must name,
    /// so this is the property `aspect_of` exists for rather than a restatement
    /// of the match above.
    #[test]
    fn every_depth_format_implies_a_depth_aspect() {
        for format in [
            Format::D16Unorm,
            Format::D32Float,
            Format::D16UnormS8Uint,
            Format::D24UnormS8Uint,
            Format::D32FloatS8Uint,
            Format::X8D24UnormPack32,
        ] {
            assert!(
                matches!(
                    aspect_of(format),
                    ImageAspect::Depth | ImageAspect::DepthStencil
                ),
                "{format:?} is a depth format but implies no depth aspect"
            );
        }
    }
}
