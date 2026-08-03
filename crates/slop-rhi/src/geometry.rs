//! Sizes and rectangles in pixels.
//!
//! Owned rather than re-exported for the reason in [`crate::format`]: a
//! swapchain extent is the single most-passed value above this crate, and while
//! it was `vk::Extent2D` every consumer that named a window size named Vulkan.
//!
//! These are plain data with public fields — there is no invariant to protect,
//! and a constructor would only be ceremony. What they buy is a name that does
//! not change when the backend does.

use ash::vk;

/// A size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Extent2D {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Extent2D {
    /// A size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether either dimension is zero.
    ///
    /// Worth asking before building anything sized from this: a minimized
    /// window reports a zero extent, and a swapchain cannot be created at that
    /// size.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Width over height, or zero for an empty extent.
    ///
    /// Here rather than at each call site because dividing by a zero height is
    /// exactly what a minimized window invites, and a `NaN` aspect ratio
    /// reaches a projection matrix and turns into a blank screen rather than an
    /// error.
    #[must_use]
    pub fn aspect_ratio(self) -> f32 {
        if self.height == 0 {
            return 0.0;
        }

        self.width as f32 / self.height as f32
    }

    /// The Vulkan value this maps to. The escape hatch — see [`crate::handle`].
    pub fn to_vk(self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.width,
            height: self.height,
        }
    }

    pub(crate) fn from_vk(extent: vk::Extent2D) -> Self {
        Self {
            width: extent.width,
            height: extent.height,
        }
    }
}

/// A point in pixels, measured from the top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Offset2D {
    /// Distance from the left edge.
    pub x: i32,
    /// Distance from the top edge.
    pub y: i32,
}

impl Offset2D {
    /// A point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// The Vulkan value this maps to. The escape hatch — see [`crate::handle`].
    pub fn to_vk(self) -> vk::Offset2D {
        vk::Offset2D {
            x: self.x,
            y: self.y,
        }
    }
}

/// A rectangle in pixels. What a scissor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect2D {
    /// The top-left corner.
    pub offset: Offset2D,
    /// The size.
    pub extent: Extent2D,
}

impl Rect2D {
    /// A rectangle.
    #[must_use]
    pub const fn new(offset: Offset2D, extent: Extent2D) -> Self {
        Self { offset, extent }
    }

    /// The Vulkan value this maps to. The escape hatch — see [`crate::handle`].
    pub fn to_vk(self) -> vk::Rect2D {
        vk::Rect2D {
            offset: self.offset.to_vk(),
            extent: self.extent.to_vk(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_extent_with_a_zero_dimension_is_empty() {
        assert!(Extent2D::new(0, 1080).is_empty());
        assert!(Extent2D::new(1920, 0).is_empty());
        assert!(!Extent2D::new(1920, 1080).is_empty());
    }

    /// A minimized window reports zero height, and the aspect ratio taken from
    /// it must not be `NaN` — that value reaches a projection matrix and blanks
    /// the screen rather than failing.
    #[test]
    fn a_zero_height_yields_a_finite_aspect_ratio() {
        let ratio = Extent2D::new(1920, 0).aspect_ratio();

        assert!(ratio.is_finite(), "aspect ratio was {ratio}");
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn an_aspect_ratio_is_width_over_height() {
        assert_eq!(Extent2D::new(1920, 1080).aspect_ratio(), 1920.0 / 1080.0);
    }
}
