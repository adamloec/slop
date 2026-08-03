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

use crate::Lights;

/// The camera, and the lights, for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// World space to clip space.
    pub view_projection: Mat4,
    /// Heap index of the light buffer this frame reads.
    ///
    /// Meaningless when [`light_count`](Self::light_count) is zero, which is
    /// why [`unlit`](Self::unlit) exists rather than a caller inventing an
    /// index to mean "none".
    pub lights: u32,
    /// How many lights that buffer holds.
    pub light_count: u32,
}

impl View {
    /// A view lit by `lights`, reading the buffer for this frame's slot.
    ///
    /// `slot` is [`Frame::slot`](crate::Frame::slot). Taking it here rather than
    /// letting a caller pass a bare index is the point: the light buffer is a
    /// ring, and reading the wrong element of it is a corrupted frame rather
    /// than an error.
    #[must_use]
    pub fn new(view_projection: Mat4, lights: &Lights, slot: usize) -> Self {
        Self {
            view_projection,
            lights: lights.handle(slot),
            light_count: lights.count(),
        }
    }

    /// A view with no point lights at all.
    ///
    /// What a depth prepass uses — it shades nothing — and what a caller that
    /// has not placed any lights uses. The directional light in
    /// `shaders/passes/model.slang` still applies; it is not data yet.
    #[must_use]
    pub fn unlit(view_projection: Mat4) -> Self {
        Self {
            view_projection,
            lights: 0,
            light_count: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlit_view_reads_no_lights() {
        // The heap index is not "no buffer" — zero is a real slot. The count is
        // what stops the loop, which is why it rather than the index is the
        // thing that means "none".
        let view = View::unlit(Mat4::IDENTITY);

        assert_eq!(view.light_count, 0);
    }
}
