//! What every draw in a frame shares.
//!
//! A camera and the lights it sees. Both are per *frame*, and both were
//! previously either a parameter threaded through every call or a constant in a
//! shader — so this exists to stop the draw signatures growing a parameter each
//! time §9.4 adds something the whole frame needs.
//!
//! `docs/CONVENTIONS.md` §5.1's reason for a struct rather than arguments
//! applies exactly: adding a field does not fork every call site, and two `u32`
//! that read the same at a call site cannot be swapped silently.

use slop_math::Mat4;

use crate::Clusters;

/// The camera, and the lights, for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// World space to clip space.
    pub view_projection: Mat4,
    /// Heap index of the cluster grid this frame's draws read.
    ///
    /// The grid carries the light buffer's index as well as the cell layout, so
    /// this one number is everything a shading pass needs. That is deliberate:
    /// the cluster build reads the same buffer, and two passes reading one
    /// description cannot disagree about where a cell is.
    ///
    /// [`NO_CLUSTERS`] means there is no grid, and shading falls back to the
    /// directional light alone.
    pub grid: u32,
}

/// The grid index meaning "there is no cluster grid".
///
/// Not zero: zero is a perfectly good heap slot, and a view without clusters
/// would read whichever buffer happened to land there. The shader tests for it
/// before reading anything.
pub const NO_CLUSTERS: u32 = u32::MAX;

impl View {
    /// A view whose lighting comes from `clusters`, for this frame's slot.
    ///
    /// `slot` is [`Frame::slot`](crate::Frame::slot). Taking it here rather than
    /// letting a caller pass a bare index is the point: the grid buffers are a
    /// ring, and reading the wrong element of one is a corrupted frame rather
    /// than an error.
    #[must_use]
    pub fn new(view_projection: Mat4, clusters: &Clusters, slot: usize) -> Self {
        Self {
            view_projection,
            grid: clusters.handle(slot),
        }
    }

    /// A view with no clustered lighting at all.
    ///
    /// What a depth prepass uses — it shades nothing — and what a caller with no
    /// lights uses. The directional light in `shaders/passes/model.slang` still
    /// applies; it is not data yet.
    #[must_use]
    pub fn unlit(view_projection: Mat4) -> Self {
        Self {
            view_projection,
            grid: NO_CLUSTERS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlit_view_names_no_grid() {
        // Zero is a real heap slot, so "none" cannot be spelled that way — an
        // unlit view would otherwise read whatever buffer landed in slot zero
        // and interpret it as a cluster grid.
        let view = View::unlit(Mat4::IDENTITY);

        assert_eq!(view.grid, NO_CLUSTERS);
        assert_ne!(view.grid, 0);
    }
}
