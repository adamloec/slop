//! The lighting that has no position.
//!
//! `docs/PLAN.md` §9.5 E5's prerequisite. A directional light and the sky — the
//! two things every fragment gets regardless of where it is, and therefore the
//! two things clustering has nothing to say about.
//!
//! # Why this is separate from [`Lights`](crate::Lights)
//!
//! Not a size or tidiness split. A point light belongs to the cells its radius
//! reaches, which is what makes assigning it worthwhile. A directional light has
//! **no position and infinite extent**, so it belongs to every cell by
//! construction and putting it through the cluster build would be listing it
//! 3456 times to learn nothing.
//!
//! §6.1 carried it as a `static const` in `shaders/passes/scene/model.slang` through
//! E4 for exactly that reason: making it data earlier would have been moving it
//! for no reader. E5 is the reader — cascaded shadow maps are built along its
//! direction, so the direction has to be a value the CPU chooses rather than a
//! number compiled into a shader.
//!
//! # What the sky is, and what it is not
//!
//! Nine spherical-harmonic coefficients — `docs/PLAN.md` §9.7 E6b — which
//! replaced the single `Vec3` that was here through E5. That is the **diffuse**
//! half of image-based lighting and deliberately only that half: nine
//! coefficients reconstruct an irradiance field to within about a percent
//! precisely because it is the environment convolved with a very wide cosine
//! lobe. A sharp reflection is the high-frequency content this basis discards,
//! and it arrives at E6c as a prefiltered cube rather than as more coefficients.
//!
//! A caller with no cooked environment passes [`default_irradiance`], which is a
//! uniform field — the same one path, in its degenerate case, rather than a
//! second one.

use std::sync::Arc;

use slop_core::Handle;
use slop_math::{Sh9, Vec3};
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
    /// The direction `shaders/passes/scene/model.slang` had compiled into it
    /// before this existed, and an intensity re-based at E6d.
    ///
    /// The direction is kept exactly, because that is what made the change from
    /// constant to data checkable: the reference images did not move when it
    /// became a value.
    ///
    /// **The intensity changed from 1 to π, and that is a change of units rather
    /// than of brightness.** Shading through E6c multiplied albedo by the light
    /// directly; Lambert's law is `albedo/π · E`, and E6d added the divisor that
    /// had been missing. Every direct light therefore became three times dimmer
    /// for a reason that had nothing to do with the scene. Multiplying by π
    /// restores exactly what was there and leaves the *specular* term as the only
    /// difference E6d makes to the diffuse look.
    ///
    /// `docs/PLAN.md` §6.1 carries the row this leaves behind: an intensity is
    /// still a number someone picked, in no unit anyone can name. Real
    /// photometry — lux for a sun, lumens for a point — is what makes two lights
    /// authored by different people agree.
    fn default() -> Self {
        Self {
            direction: Vec3::new(0.4, 0.8, 0.45).normalize(),
            color: Vec3::ONE,
            intensity: std::f32::consts::PI,
        }
    }
}

/// What a fragment receives before any light is considered, with no environment.
///
/// A **uniform** field, expressed in the same nine coefficients a cooked
/// environment uses. `docs/PLAN.md` §6.1's row said the flat ambient term would
/// be replaced by image-based lighting, and this is what replacing it left
/// behind: not a second code path for callers without an environment, but the
/// degenerate case of the one path — a sky that happens to be the same colour in
/// every direction.
///
/// The colour is the constant `shaders/passes/scene/model.slang` held before any of
/// this existed, and it is kept exactly. That is what makes the change
/// checkable: [`Sh9::diffuse`] of a constant field is that constant, so a caller
/// that binds no environment renders **bit-identically** to how it did before
/// spherical harmonics arrived. Any reference image that moves at E6b is a
/// caller that opted in, not a side effect.
#[must_use]
pub fn default_irradiance() -> Sh9 {
    Sh9::constant(Vec3::new(0.18, 0.19, 0.22))
}

/// The specular index meaning "there is no environment cube".
///
/// Not zero, for the reason [`NO_CLUSTERS`](crate::NO_CLUSTERS) is not zero:
/// zero is a perfectly good heap slot, and a frame with no environment would
/// sample whichever image happened to land there. The shader tests for it before
/// reading anything, and falls back to the diffuse term alone.
pub const NO_SKY: u32 = u32::MAX;

/// The diffuse term a cooked environment carries.
///
/// The artifact stores nine RGB coefficients as plain arrays, because a file
/// format should not name a library's vector type; this is where they become the
/// maths again.
#[must_use]
pub fn irradiance_of(cooked: &slop_asset::Environment) -> Sh9 {
    Sh9 {
        coefficients: cooked.irradiance.map(Vec3::from_array),
    }
}

/// The environment as the shader reads it.
///
/// Mirrors `EnvironmentGpu` in `shaders/lib/environment.slang`. Laid out so
/// std430 and `#[repr(C)]` agree without padding on either side: two rows of a
/// `float3` and a scalar, which is sixteen bytes each, then nine four-component
/// rows.
///
/// **Four components per coefficient, and only three are read.** An array of
/// `float3` has a sixteen-byte stride in std430 regardless, so the fourth
/// component is padding that exists either way — writing it explicitly is what
/// keeps `#[repr(C)]` and the shader's view of the same bytes identical, and what
/// lets `bytemuck::Pod` derive at all.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EnvironmentGpu {
    sun_direction: [f32; 3],
    sun_intensity: f32,
    sun_color: [f32; 3],
    _pad: f32,
    /// Heap index of the prefiltered cube, or [`NO_SKY`].
    specular: u32,
    /// The sampler that reads it.
    specular_sampler: u32,
    /// How many roughness levels the chain has.
    ///
    /// Written rather than assumed: the cooker decides the chain's length from
    /// the cube's size, and a shader that guessed would map roughness onto the
    /// wrong level — which is a reflection that never quite blurs out, and looks
    /// like a material authoring problem.
    specular_levels: u32,
    _pad2: u32,
    /// Nine raw spherical-harmonic coefficients — see [`Sh9`].
    ///
    /// Raw, not pre-convolved: the cosine weighting is the shader's, in
    /// `irradianceFrom`, so this buffer holds the same numbers the cooked
    /// artifact does and there is one place where the convolution lives.
    irradiance: [[f32; 4]; slop_math::COEFFICIENTS],
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
        irradiance: &Sh9,
        sky: Option<&crate::Sky>,
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
            specular: sky.map_or(NO_SKY, crate::Sky::handle),
            specular_sampler: sky.map_or(NO_SKY, crate::Sky::sampler),
            specular_levels: sky.map_or(0, crate::Sky::levels),
            _pad2: 0,
            irradiance: irradiance
                .coefficients
                .map(|coefficient| [coefficient.x, coefficient.y, coefficient.z, 0.0]),
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
        // Three sixteen-byte rows, then nine of sixteen. If this and
        // `EnvironmentGpu` in `lib/lighting/environment.slang` disagree, the
        // shader reads the sun's colour as a coefficient and nothing reports it.
        assert_eq!(size_of::<EnvironmentGpu>(), 48 + 9 * 16);
        assert_eq!(align_of::<EnvironmentGpu>(), 4);
    }

    #[test]
    fn the_default_sun_points_where_the_shader_constant_did() {
        // The direction is what makes the E5 move from constant to data
        // checkable, and it has not changed since: the reference images did not
        // move when it became a value.
        let sun = DirectionalLight::default();
        let expected = Vec3::new(0.4, 0.8, 0.45).normalize();

        assert!((sun.direction - expected).length() < 1e-6);
        assert_eq!(sun.color, Vec3::ONE);
    }

    #[test]
    fn the_default_sun_intensity_carries_lamberts_divisor() {
        // **A change of units, not of brightness.** Shading through E6c
        // multiplied albedo by the light directly; Lambert's law divides by π,
        // and E6d added the divisor. An intensity of one would therefore have
        // made every direct light three times dimmer for a reason that has
        // nothing to do with the scene.
        //
        // Asserted rather than left as a literal so that a later change to the
        // shading model has to come back here and say what it did to the units.
        assert!((DirectionalLight::default().intensity - std::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn the_default_irradiance_is_the_flat_term_it_replaced() {
        // What makes E6b's change checkable: a caller that binds no environment
        // must render exactly as it did when the ambient term was one colour.
        // The nine coefficients are a different representation of the same
        // number, not a different number, and this is the assertion — every
        // direction reconstructs to the constant the shader used to hold.
        let expected = Vec3::new(0.18, 0.19, 0.22);
        let sh = default_irradiance();

        for normal in [
            Vec3::Y,
            Vec3::NEG_Y,
            Vec3::X,
            Vec3::NEG_Z,
            Vec3::new(0.3, 0.5, -0.8).normalize(),
        ] {
            assert!(
                (sh.diffuse(normal) - expected).length() < 1e-5,
                "{normal:?} reconstructs to {:?}, not {expected:?}",
                sh.diffuse(normal)
            );
        }
    }

    #[test]
    fn a_cooked_environment_becomes_the_coefficients_it_stores() {
        // The artifact holds plain arrays so a file format does not name a
        // library's vector type; this is the one place that conversion happens,
        // and a transposed index here would rotate every environment's lighting.
        let mut cooked = slop_asset::Environment {
            size: 1,
            mip_levels: 1,
            format: slop_asset::Format::Rgba16Float,
            irradiance: [[0.0; 3]; slop_math::COEFFICIENTS],
            texels: Vec::new(),
        };
        cooked.irradiance[3] = [1.0, 2.0, 3.0];

        assert_eq!(
            irradiance_of(&cooked).coefficients[3],
            Vec3::new(1.0, 2.0, 3.0)
        );
    }

    #[test]
    fn the_default_direction_is_a_unit_vector() {
        // The shading maths reads it as a cosine, so a longer vector is a
        // brighter surface — which looks like an exposure problem rather than a
        // normalisation one.
        assert!((DirectionalLight::default().direction.length() - 1.0).abs() < 1e-6);
    }
}
