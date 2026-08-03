//! The lighting that has no position.
//!
//! `docs/PLAN.md` §9.5 E5's prerequisite. A directional light and a constant
//! ambient term — the two things every fragment gets regardless of where it is,
//! and therefore the two things clustering has nothing to say about.
//!
//! # Why this is separate from [`Lights`](crate::Lights)
//!
//! Not a size or tidiness split. A point light belongs to the cells its radius
//! reaches, which is what makes assigning it worthwhile. A directional light has
//! **no position and infinite extent**, so it belongs to every cell by
//! construction and putting it through the cluster build would be listing it
//! 3456 times to learn nothing.
//!
//! §6.1 carried it as a `static const` in `shaders/passes/model.slang` through
//! E4 for exactly that reason: making it data earlier would have been moving it
//! for no reader. E5 is the reader — cascaded shadow maps are built along its
//! direction, so the direction has to be a value the CPU chooses rather than a
//! number compiled into a shader.
//!
//! # What arrives here next
//!
//! The cascades themselves, and at E6 the image-based lighting that replaces the
//! constant ambient term with something directional. Both are per-frame values
//! every shading pass needs, which is what this buffer is.

use std::sync::Arc;

use slop_core::Handle;
use slop_math::Vec3;
use slop_rhi::{
    Allocator, BindlessHeap, Buffer, BufferConfig, BufferUsage, MemoryLocation, StorageBuffer,
};

use crate::RenderError;

/// A light infinitely far away, so every fragment sees it from the same angle.
///
/// The sun, in practice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionalLight {
    /// Which way the light travels **from**, normalised.
    ///
    /// Towards the source, not away from it — so a sun overhead points up. That
    /// is the convention the shading maths wants (`dot(normal, direction)` is
    /// the cosine of the incidence angle with no negation), and the opposite
    /// convention is equally defensible, which is why it is stated here rather
    /// than left to be inferred from a shader.
    pub direction: Vec3,
    /// Linear RGB.
    pub color: Vec3,
    /// A multiplier on `color`, so a light can be brightened without
    /// desaturating.
    pub intensity: f32,
}

impl Default for DirectionalLight {
    /// The values `shaders/passes/model.slang` had compiled into it before this
    /// existed.
    ///
    /// Kept exactly, because that is what makes the change to data checkable:
    /// the reference images must not move when a constant becomes a value.
    fn default() -> Self {
        Self {
            direction: Vec3::new(0.4, 0.8, 0.45).normalize(),
            color: Vec3::ONE,
            intensity: 1.0,
        }
    }
}

/// What a fragment receives before any light is considered.
///
/// A flat term, and a placeholder for what E6 replaces it with: real ambient
/// light arrives from different directions with different colours, which is what
/// image-based lighting captures and a constant cannot.
#[must_use]
pub fn default_ambient() -> Vec3 {
    Vec3::new(0.18, 0.19, 0.22)
}

/// The environment as the shader reads it.
///
/// Mirrors `EnvironmentGpu` in `shaders/lib/environment.slang`. Laid out so
/// std430 and `#[repr(C)]` agree without padding on either side: two rows of a
/// `float3` and a scalar, which is sixteen bytes each.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EnvironmentGpu {
    sun_direction: [f32; 3],
    sun_intensity: f32,
    sun_color: [f32; 3],
    _pad: f32,
    ambient: [f32; 3],
    _pad2: f32,
}

/// The per-frame environment buffer, one per frame in flight.
pub struct Environment {
    slots: Vec<Slot>,
}

struct Slot {
    buffer: Buffer,
    handle: Handle<StorageBuffer>,
}

impl Environment {
    /// Allocate one buffer per in-flight slot and place each in the heap.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a buffer cannot be allocated, or
    /// [`RenderError::Layout`] if the bindless heap is full.
    pub fn new(
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        frames_in_flight: usize,
    ) -> Result<Self, RenderError> {
        let mut slots = Vec::with_capacity(frames_in_flight);

        for _ in 0..frames_in_flight {
            let buffer = Buffer::new(
                allocator,
                &BufferConfig {
                    name: "environment",
                    size: size_of::<EnvironmentGpu>() as u64,
                    usage: BufferUsage::STORAGE,
                    // Host-visible: rewritten every frame, since the sun moves
                    // and the ambient term is about to become something a
                    // caller edits.
                    location: MemoryLocation::Upload,
                },
            )?;

            let handle =
                heap.insert_storage_buffer(buffer.handle())
                    .ok_or(RenderError::Layout {
                        what: "the bindless heap had no room for an environment buffer",
                    })?;

            slots.push(Slot { buffer, handle });
        }

        Ok(Self { slots })
    }

    /// Write this frame's environment.
    ///
    /// Call inside the frame closure with [`Frame::slot`](crate::Frame::slot),
    /// for the reason [`Lights::write`](crate::Lights::write) gives.
    ///
    /// The direction is normalised here rather than trusted: a caller that
    /// scales it changes the apparent brightness of every lit surface, which
    /// looks like an exposure problem rather than an un-normalised vector.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the buffer cannot be mapped, or
    /// [`RenderError::Layout`] if `slot` names one that does not exist.
    pub fn write(
        &mut self,
        slot: usize,
        sun: &DirectionalLight,
        ambient: Vec3,
    ) -> Result<(), RenderError> {
        let Some(target) = self.slots.get_mut(slot) else {
            return Err(RenderError::Layout {
                what: "a frame asked for an environment slot that does not exist",
            });
        };

        let written = EnvironmentGpu {
            sun_direction: sun.direction.normalize_or_zero().to_array(),
            sun_intensity: sun.intensity,
            sun_color: sun.color.to_array(),
            _pad: 0.0,
            ambient: ambient.to_array(),
            _pad2: 0.0,
        };

        let bytes = bytemuck::bytes_of(&written);
        target.buffer.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        Ok(())
    }

    /// The heap index a shader reads `slot`'s environment through.
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist, which means the frame renderer
    /// was built with more in-flight slots than this was.
    #[must_use]
    pub fn handle(&self, slot: usize) -> u32 {
        self.slots
            .get(slot)
            .expect("the environment has a slot per frame in flight")
            .handle
            .index()
    }
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("slots", &self.slots.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_row_matches_what_the_shader_reads() {
        assert_eq!(size_of::<EnvironmentGpu>(), 48);
        assert_eq!(align_of::<EnvironmentGpu>(), 4);
    }

    #[test]
    fn the_default_sun_is_the_constant_the_shader_used_to_hold() {
        // What makes the move from constant to data checkable: the reference
        // images must not move. If these values drift, the change stops being
        // a refactor and the goldens stop being evidence of one.
        let sun = DirectionalLight::default();
        let expected = Vec3::new(0.4, 0.8, 0.45).normalize();

        assert!((sun.direction - expected).length() < 1e-6);
        assert_eq!(sun.color, Vec3::ONE);
        assert_eq!(sun.intensity, 1.0);
        assert_eq!(default_ambient(), Vec3::new(0.18, 0.19, 0.22));
    }

    #[test]
    fn the_default_direction_is_a_unit_vector() {
        // The shading maths reads it as a cosine, so a longer vector is a
        // brighter surface — which looks like an exposure problem rather than a
        // normalisation one.
        assert!((DirectionalLight::default().direction.length() - 1.0).abs() < 1e-6);
    }
}
