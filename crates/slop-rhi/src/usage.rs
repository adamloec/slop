//! What a buffer or image is going to be used for.
//!
//! Owned flag sets rather than `vk::BufferUsageFlags` and
//! `vk::ImageUsageFlags`, for the reason in [`crate::format`]. Together with
//! [`Format`](crate::Format) and [`Extent2D`](crate::Extent2D) these are the
//! four types that accounted for most of what leaked above this crate.
//!
//! Hand-rolled rather than taking a `bitflags` dependency: there are five
//! variants each, the operations needed are `|` and `contains`, and the
//! workspace does not otherwise pay for that crate. `docs/CONVENTIONS.md` §1
//! wants dependencies added at the point they earn themselves.

use std::ops::{BitOr, BitOrAssign};

use ash::vk;

/// What a buffer is for.
///
/// Combine with `|`. A buffer must declare every use it will be put to at
/// creation, which is why a vertex buffer filled by a staging copy needs
/// `VERTEX | TRANSFER_DST` rather than just `VERTEX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferUsage(u32);

impl BufferUsage {
    /// No usage. Not valid for a real buffer; the identity for `|`.
    pub const NONE: Self = Self(0);
    /// The source of a transfer.
    pub const TRANSFER_SRC: Self = Self(1 << 0);
    /// The destination of a transfer.
    pub const TRANSFER_DST: Self = Self(1 << 1);
    /// Vertex data.
    pub const VERTEX: Self = Self(1 << 2);
    /// Index data.
    pub const INDEX: Self = Self(1 << 3);
    /// Read or written by a shader through the bindless heap.
    pub const STORAGE: Self = Self(1 << 4);

    /// Whether every flag in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The Vulkan value this maps to. The escape hatch — see [`crate::handle`].
    #[must_use]
    pub fn to_vk(self) -> vk::BufferUsageFlags {
        let mut flags = vk::BufferUsageFlags::empty();

        if self.contains(Self::TRANSFER_SRC) {
            flags |= vk::BufferUsageFlags::TRANSFER_SRC;
        }
        if self.contains(Self::TRANSFER_DST) {
            flags |= vk::BufferUsageFlags::TRANSFER_DST;
        }
        if self.contains(Self::VERTEX) {
            flags |= vk::BufferUsageFlags::VERTEX_BUFFER;
        }
        if self.contains(Self::INDEX) {
            flags |= vk::BufferUsageFlags::INDEX_BUFFER;
        }
        if self.contains(Self::STORAGE) {
            flags |= vk::BufferUsageFlags::STORAGE_BUFFER;
        }

        flags
    }
}

impl BitOr for BufferUsage {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOrAssign for BufferUsage {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// What an image is for.
///
/// Combine with `|`, as [`BufferUsage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImageUsage(u32);

impl ImageUsage {
    /// No usage. Not valid for a real image; the identity for `|`.
    pub const NONE: Self = Self(0);
    /// The source of a transfer or blit — which a mip chain's later levels are.
    pub const TRANSFER_SRC: Self = Self(1 << 0);
    /// The destination of a transfer or blit.
    pub const TRANSFER_DST: Self = Self(1 << 1);
    /// Sampled by a shader.
    pub const SAMPLED: Self = Self(1 << 2);
    /// Rendered into as colour.
    pub const COLOR_ATTACHMENT: Self = Self(1 << 3);
    /// Rendered into as depth, stencil, or both.
    pub const DEPTH_STENCIL_ATTACHMENT: Self = Self(1 << 4);

    /// Whether every flag in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The Vulkan value this maps to. The escape hatch — see [`crate::handle`].
    #[must_use]
    pub fn to_vk(self) -> vk::ImageUsageFlags {
        let mut flags = vk::ImageUsageFlags::empty();

        if self.contains(Self::TRANSFER_SRC) {
            flags |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        if self.contains(Self::TRANSFER_DST) {
            flags |= vk::ImageUsageFlags::TRANSFER_DST;
        }
        if self.contains(Self::SAMPLED) {
            flags |= vk::ImageUsageFlags::SAMPLED;
        }
        if self.contains(Self::COLOR_ATTACHMENT) {
            flags |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        }
        if self.contains(Self::DEPTH_STENCIL_ATTACHMENT) {
            flags |= vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT;
        }

        flags
    }
}

impl BitOr for ImageUsage {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOrAssign for ImageUsage {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_combined_usage_contains_both_halves() {
        let usage = BufferUsage::VERTEX | BufferUsage::TRANSFER_DST;

        assert!(usage.contains(BufferUsage::VERTEX));
        assert!(usage.contains(BufferUsage::TRANSFER_DST));
        assert!(!usage.contains(BufferUsage::INDEX));
    }

    #[test]
    fn an_empty_usage_contains_nothing_but_itself() {
        assert!(BufferUsage::NONE.is_empty());
        assert!(BufferUsage::NONE.contains(BufferUsage::NONE));
        assert!(!BufferUsage::NONE.contains(BufferUsage::VERTEX));
        assert!(ImageUsage::NONE.is_empty());
    }

    /// Every flag maps to a distinct Vulkan bit. A copy-paste slip in `to_vk`
    /// that mapped two flags to the same bit would otherwise be silent until a
    /// buffer was used for something it had not declared.
    #[test]
    fn every_buffer_flag_maps_to_a_distinct_vulkan_flag() {
        const ALL: [BufferUsage; 5] = [
            BufferUsage::TRANSFER_SRC,
            BufferUsage::TRANSFER_DST,
            BufferUsage::VERTEX,
            BufferUsage::INDEX,
            BufferUsage::STORAGE,
        ];

        for (index, one) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(one.to_vk(), other.to_vk(), "{one:?} and {other:?} collide");
            }
        }
    }

    #[test]
    fn every_image_flag_maps_to_a_distinct_vulkan_flag() {
        const ALL: [ImageUsage; 5] = [
            ImageUsage::TRANSFER_SRC,
            ImageUsage::TRANSFER_DST,
            ImageUsage::SAMPLED,
            ImageUsage::COLOR_ATTACHMENT,
            ImageUsage::DEPTH_STENCIL_ATTACHMENT,
        ];

        for (index, one) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(one.to_vk(), other.to_vk(), "{one:?} and {other:?} collide");
            }
        }
    }

    #[test]
    fn combining_flags_combines_the_vulkan_flags() {
        let usage = ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST;

        assert_eq!(
            usage.to_vk(),
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST
        );
    }
}
