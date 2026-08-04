//! Lights, as data a shader can read rather than a constant in one.
//!
//! `docs/PLAN.md` §9.5 E4, first step. §9.4's clustered forward+ assigns lights
//! to cells of a grid and has the forward pass read only its own cell — none of
//! which can be built while the only light in the engine is a `static const`
//! direction in `shaders/passes/scene/model.slang`.
//!
//! # Why the radius is not a hint
//!
//! Physical falloff is inverse-square, which never reaches zero. A light with
//! genuinely unbounded reach belongs to every cluster, and clustering it saves
//! nothing. So [`PointLight::radius`] is a hard cutoff and the shader applies a
//! **windowed** inverse square that reaches exactly zero there — the standard
//! `1 - (d²/r²)²` window, squared, which fades smoothly rather than leaving a
//! visible edge where the light stops.
//!
//! That makes the radius the thing cluster assignment tests against, and makes
//! "is this light in this cell" a question with an answer. Getting this wrong in
//! the other order — cluster on a radius the shading ignores — is the classic
//! version of this bug: lights pop as the camera moves, because a cell stops
//! listing a light that is still contributing to it.
//!
//! # What is deliberately still a constant
//!
//! The **directional** light. It has no position and infinite extent, so it
//! belongs to every cluster by construction and clustering it is meaningless.
//! It becomes data at E5, where cascaded shadows need its direction; making it
//! data now would be moving it for no reader. `docs/PLAN.md` §6.1 has the row.
//!
//! # One buffer per frame in flight
//!
//! Lights move, so this is rewritten every frame — and writing a single shared
//! buffer would corrupt the frame still reading it. [`Frame::slot`] exists for
//! exactly this, and the debug overlay's vertex buffers already work this way.
//!
//! The **heap slots are allocated once**, at construction, one per in-flight
//! slot. That is what keeps the per-frame path free of heap mutation: writing
//! lights needs `&mut self` and nothing else, so it can happen inside the frame
//! closure where the slot index is known, while the heap stays borrowed
//! immutably by everything drawing.
//!
//! [`Frame::slot`]: crate::Frame::slot

use std::sync::Arc;

use slop_core::Handle;
use slop_math::Vec3;
use slop_rhi::{
    Allocator, BindlessHeap, Buffer, BufferConfig, BufferUsage, MemoryLocation, StorageBuffer,
};

use crate::RenderError;

/// A point light, as a caller describes one.
///
/// Position is world space, matching what the vertex shader transforms *from* —
/// see [`Lights::write`] for why that is stated rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointLight {
    /// Where it sits, in world space.
    pub position: Vec3,
    /// Linear RGB, not sRGB. Every colour inside the renderer is linear; the
    /// transfer function is applied once, by the swapchain format.
    pub color: Vec3,
    /// How bright, as a multiplier on `color`.
    ///
    /// Separate from `color` rather than folded into it so that a light can be
    /// brightened without desaturating, which is what happens when a colour
    /// channel is pushed past one and the others are not.
    pub intensity: f32,
    /// Where its contribution reaches exactly zero.
    ///
    /// A hard cutoff, not a falloff hint — see this module's documentation. A
    /// light contributes to a cluster if and only if the cluster's bounds
    /// intersect this sphere.
    pub radius: f32,
}

/// One light as the shader reads it.
///
/// Mirrors `PointLightGpu` in `shaders/passes/scene/model.slang`. Laid out so that
/// std430 and `#[repr(C)]` agree without padding on either side: two rows of
/// `float3` plus a scalar, which is exactly sixteen bytes each. Pairing the
/// radius with the position rather than putting the two scalars together is
/// deliberate — it means a cluster-assignment pass reads one row and has both
/// the centre and the radius of the sphere it is testing.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PointLightGpu {
    position: [f32; 3],
    radius: f32,
    color: [f32; 3],
    intensity: f32,
}

/// The lights a frame draws with, one buffer per frame in flight.
pub struct Lights {
    /// One per in-flight slot, each with its own heap slot allocated up front.
    slots: Vec<Slot>,
    /// How many lights the last [`write`](Self::write) put in.
    count: u32,
    /// How many rows each buffer holds.
    capacity: u32,
}

/// One in-flight slot's buffer, and where the heap keeps it.
struct Slot {
    buffer: Buffer,
    handle: Handle<StorageBuffer>,
}

impl Lights {
    /// Allocate one buffer per in-flight slot and place each in the heap.
    ///
    /// `capacity` is fixed for the lifetime of this: growing it would mean
    /// replacing buffers the heap already points at, mid-frame, which is the
    /// hazard [`MeshRenderer::resize`](crate::MeshRenderer::resize) has to wait
    /// for the device over. A scene wanting more than it was built for gets
    /// [`RenderError::Layout`] from [`write`](Self::write) rather than a
    /// silently truncated light list.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a buffer cannot be allocated, or
    /// [`RenderError::Layout`] if the bindless heap is full or `capacity` is
    /// zero.
    pub fn new(
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        frames_in_flight: usize,
        capacity: u32,
    ) -> Result<Self, RenderError> {
        if capacity == 0 {
            return Err(RenderError::Layout {
                what: "a light buffer with no room for any light is not useful",
            });
        }

        let bytes = u64::from(capacity) * size_of::<PointLightGpu>() as u64;
        let mut slots = Vec::with_capacity(frames_in_flight);

        for index in 0..frames_in_flight {
            let buffer = Buffer::new(
                allocator,
                &BufferConfig {
                    name: "lights",
                    size: bytes,
                    usage: BufferUsage::STORAGE,
                    // Host-visible and written every frame. Staging would cost a
                    // copy and a barrier to move a few kilobytes that the CPU
                    // produces fresh each time.
                    location: MemoryLocation::Upload,
                },
            )?;

            let handle =
                heap.insert_storage_buffer(buffer.handle())
                    .ok_or(RenderError::Layout {
                        what: "the bindless heap had no room for a light buffer",
                    })?;

            slots.push(Slot { buffer, handle });

            debug_assert!(index < frames_in_flight);
        }

        Ok(Self {
            slots,
            count: 0,
            capacity,
        })
    }

    /// Write `lights` into the buffer for `slot`.
    ///
    /// Call inside the frame closure, with [`Frame::slot`](crate::Frame::slot).
    /// That is safe precisely there and nowhere earlier:
    /// [`FrameRenderer::render`](crate::FrameRenderer::render) waits for this
    /// slot's previous submission before handing the frame over, so the GPU has
    /// finished reading whatever was here.
    ///
    /// Positions are taken in **world space** and written unchanged. The shader
    /// converts, because the alternative — transforming on the way in — would
    /// bake a particular view into the buffer, and §9.4's cluster build wants
    /// the same buffer readable from a compute pass that has no view of its own.
    ///
    /// # Errors
    ///
    /// [`RenderError::Layout`] if there are more lights than the capacity this
    /// was built with, or if `slot` names one that does not exist. Both are
    /// caller mistakes reported rather than clamped: a silently dropped light is
    /// a lighting bug that looks like an authoring mistake.
    pub fn write(&mut self, slot: usize, lights: &[PointLight]) -> Result<(), RenderError> {
        if lights.len() > self.capacity as usize {
            return Err(RenderError::Layout {
                what: "more lights than the light buffer was built to hold",
            });
        }

        let Some(target) = self.slots.get_mut(slot) else {
            return Err(RenderError::Layout {
                what: "a frame asked for a light buffer slot that does not exist",
            });
        };

        let rows: Vec<PointLightGpu> = lights
            .iter()
            .map(|light| PointLightGpu {
                position: light.position.to_array(),
                radius: light.radius,
                color: light.color.to_array(),
                intensity: light.intensity,
            })
            .collect();

        let bytes: &[u8] = bytemuck::cast_slice(&rows);
        target.buffer.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        self.count = lights.len() as u32;

        Ok(())
    }

    /// The heap index a shader reads `slot`'s lights through.
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist, which means the frame renderer
    /// was built with more in-flight slots than this was.
    #[must_use]
    pub fn handle(&self, slot: usize) -> u32 {
        self.slots
            .get(slot)
            .expect("the light buffer has a slot per frame in flight")
            .handle
            .index()
    }

    /// How many lights the last [`write`](Self::write) put in.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// How many lights a buffer holds.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }
}

impl std::fmt::Debug for Lights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lights")
            .field("slots", &self.slots.len())
            .field("count", &self.count)
            .field("capacity", &self.capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_light_row_matches_what_the_shader_reads() {
        // std430 and `#[repr(C)]` agreeing is what makes the storage buffer
        // readable at all, and a mismatch shifts every light after the first
        // rather than failing.
        assert_eq!(size_of::<PointLightGpu>(), 32);
        assert_eq!(align_of::<PointLightGpu>(), 4);
    }

    #[test]
    fn the_position_and_its_radius_share_a_row() {
        // Not cosmetic. Cluster assignment tests a sphere against a cell, and
        // reading the centre and the radius from one sixteen-byte row is the
        // difference between one load and two.
        assert_eq!(std::mem::offset_of!(PointLightGpu, position), 0);
        assert_eq!(std::mem::offset_of!(PointLightGpu, radius), 12);
        assert_eq!(std::mem::offset_of!(PointLightGpu, color), 16);
        assert_eq!(std::mem::offset_of!(PointLightGpu, intensity), 28);
    }
}
