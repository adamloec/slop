//! What rendering a frame can fail with.

use thiserror::Error;

/// Why a frame could not be rendered.
///
/// Typed rather than a string, because this is a library surface
/// (`docs/CONVENTIONS.md` §6). The examples this crate replaces returned
/// `String` throughout, which was right for a binary that prints and exits and
/// wrong for something a caller has to make decisions about — a swapchain that
/// cannot be recreated is recoverable by resizing, and a device lost is not.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The device cannot present.
    ///
    /// A device enumerated without a surface has no present queue family
    /// (`slop-rhi` refuses `VK_KHR_swapchain` in that case, deliberately). The
    /// examples papered over this by falling back to the graphics queue, which
    /// happens to work on hardware where the two families coincide and is a spec
    /// violation where they do not — the kind of bug that appears only on
    /// someone else's GPU.
    #[error("this device has no present queue; it was created without a surface")]
    NoPresentQueue,

    /// Zero frames in flight were requested.
    #[error("frames_in_flight must be at least one")]
    NoFramesInFlight,

    /// A shader reads vertex locations that are not `0..n`.
    ///
    /// Vulkan allows sparse locations; `VertexLayout` does not express them,
    /// because its attribute array is positional. Refused rather than packed
    /// down, which would bind every attribute after the gap to the wrong slot.
    #[error(
        "vertex input locations must be contiguous from zero; expected {expected}, found {found}"
    )]
    VertexLocationGap {
        /// The location the layout needed next.
        expected: u32,
        /// What the shader declared instead.
        found: u32,
    },

    /// Something underneath failed.
    #[error(transparent)]
    Rhi(#[from] slop_rhi::RhiError),
}
