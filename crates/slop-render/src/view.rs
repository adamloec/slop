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

use crate::{Clusters, Environment};

/// The camera, and the lighting, for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// World space to clip space.
    pub view_projection: Mat4,
    /// Heap index of the cluster grid this frame's draws read.
    ///
    /// The grid carries the light buffer's index as well as the cell layout, so
    /// this one number is everything a shading pass needs to find its point
    /// lights. That is deliberate: the cluster build reads the same buffer, and
    /// two passes reading one description cannot disagree about where a cell is.
    ///
    /// [`NO_CLUSTERS`] means there is no grid, and shading falls back to the
    /// environment alone.
    pub grid: u32,
    /// Heap index of the directional light and ambient term.
    ///
    /// A second index rather than fields in the grid: the grid describes where
    /// the *cells* are, and the shadow passes read the sun's direction without
    /// caring about clustering at all.
    pub environment: u32,
    /// Heap index of the shadow cascades, or [`NO_SHADOWS`].
    ///
    /// `NO_SHADOWS` is what a shadow render itself uses: a cascade cannot
    /// shadow itself, and sampling the map a pass is currently writing would be
    /// a hazard as well as nonsense.
    pub shadows: u32,
}

/// The shadow index meaning "there are no cascades".
///
/// Not zero, for the reason [`NO_CLUSTERS`] is not zero.
pub const NO_SHADOWS: u32 = u32::MAX;

/// The grid index meaning "there is no cluster grid".
///
/// Not zero: zero is a perfectly good heap slot, and a view without clusters
/// would read whichever buffer happened to land there. The shader tests for it
/// before reading anything.
pub const NO_CLUSTERS: u32 = u32::MAX;

impl View {
    /// A view lit by `environment` and the point lights `clusters` assigned.
    ///
    /// `slot` is [`Frame::slot`](crate::Frame::slot). Taking it here rather than
    /// letting a caller pass a bare index is the point: both are rings, and
    /// reading the wrong element of one is a corrupted frame rather than an
    /// error.
    /// `shadows` is `None` for a pass that casts rather than receives — see
    /// [`NO_SHADOWS`].
    #[must_use]
    pub fn new(
        view_projection: Mat4,
        environment: &Environment,
        clusters: &Clusters,
        shadows: Option<&crate::Shadows>,
        slot: usize,
    ) -> Self {
        Self {
            view_projection,
            grid: clusters.handle(slot),
            environment: environment.handle(slot),
            shadows: shadows.map_or(NO_SHADOWS, |shadows| shadows.handle(slot)),
        }
    }

    /// A view with the environment but no point lights.
    ///
    /// What a depth prepass uses — it shades nothing, so the cluster grid would
    /// be along for the ride — and what a caller that has placed no point lights
    /// uses. The sun and the ambient term still apply.
    #[must_use]
    pub fn unclustered(view_projection: Mat4, environment: &Environment, slot: usize) -> Self {
        Self {
            view_projection,
            grid: NO_CLUSTERS,
            environment: environment.handle(slot),
            shadows: NO_SHADOWS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_grid_cannot_be_spelled_as_slot_zero() {
        // Zero is a real heap slot, so "none" cannot be spelled that way — an
        // unclustered view would otherwise read whatever buffer landed in slot
        // zero and interpret it as a cluster grid.
        assert_ne!(NO_CLUSTERS, 0);
        assert_eq!(NO_CLUSTERS, u32::MAX);
    }
}
