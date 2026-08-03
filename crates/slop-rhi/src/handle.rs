//! Opaque names for GPU objects.
//!
//! Every one of these wraps a raw Vulkan handle and exposes nothing. They exist
//! so that a renderer can *refer* to an image, a buffer or a semaphore without
//! naming `vk::Image`, `vk::Buffer` or `vk::Semaphore` in its own signatures.
//!
//! The distinction matters because of `docs/DESIGN.md` §2.2. That section bought
//! an owned RHI at an explicitly accepted cost — 8–15k lines before a first
//! triangle — on the promise that "a DX12 backend then slots in cleanly". A
//! promise like that is kept or broken at the type level: while
//! `slop_render::Target` had a `pub image: vk::Image` field, adding a backend
//! meant editing every consumer, which is the §1.2 principle 6 test coming out
//! on the wrong side.
//!
//! They are `Copy` and cheap. They are also *not* lifetime-checked — holding one
//! past the death of the resource it names is exactly as wrong as holding the
//! raw handle would be. What they buy is the backend boundary, not safety.

use ash::vk;

/// Declares a handle newtype and its escape hatch.
///
/// The `raw` accessors are the handle-level counterpart of [`crate::vk`] being
/// re-exported: the boundary is about what the layers above name *by default*,
/// not about making the underlying handle unreachable. Something genuinely has
/// to reach past it — this crate's own integration tests build Vulkan structures
/// directly, and a vendor extension would too — and the alternative to a named
/// escape hatch is not purity, it is those callers taking their own `ash`
/// dependency.
///
/// Reaching for `raw` inside `slop-render` or an example is the smell this whole
/// module exists to remove.
macro_rules! handle {
    ($(#[$meta:meta])* $name:ident, $raw:ty) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(pub(crate) $raw);

        impl $name {
            /// The underlying Vulkan handle.
            ///
            /// The escape hatch. See this module's documentation for when it is
            /// the right tool, which is rarely.
            #[must_use]
            pub fn raw(self) -> $raw {
                self.0
            }
        }
    };
}

handle!(
    /// An image, as something to name rather than something to touch.
    ImageHandle,
    vk::Image
);
handle!(
    /// A view of an image.
    ImageViewHandle,
    vk::ImageView
);
handle!(
    /// A buffer.
    BufferHandle,
    vk::Buffer
);
handle!(
    /// A sampler.
    SamplerHandle,
    vk::Sampler
);
handle!(
    /// A semaphore, binary or timeline.
    SemaphoreHandle,
    vk::Semaphore
);
handle!(
    /// A queue to submit to.
    QueueHandle,
    vk::Queue
);
