//! How a texture is filtered, as an owned object rather than a raw handle.
//!
//! Three call sites created a `vk::Sampler` by hand and destroyed it in a `Drop`
//! they each wrote — which put `unsafe` in `slop-render` and in an example,
//! while `docs/CONVENTIONS.md` §7 confines it to three named crates. Owning the
//! object here removes both the duplication and the `unsafe`.
//!
//! This is *not* the sampler cache `docs/PLAN.md` §6.1 records. A cache
//! deduplicates identical descriptions so a thousand materials asking for
//! trilinear-repeat share one slot; this just makes one sampler an RAII value.
//! The cache arrives with the material system and will be built out of these.

use std::sync::Arc;

use ash::vk;

use crate::{Device, RhiError, SamplerHandle};

/// How a sampler reads between texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Blend between neighbours. What a surface texture wants.
    #[default]
    Linear,
    /// Take the nearest texel.
    ///
    /// Not merely "lower quality": a golden-image test that samples a
    /// checkerboard depends on it, because a linear filter makes the result
    /// depend on the driver's rounding and turns an exact comparison into a
    /// tolerance.
    Nearest,
}

/// What happens outside the zero-to-one range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrap {
    /// Tile. What surface textures and tiling detail want.
    #[default]
    Repeat,
    /// Hold the edge texel.
    ///
    /// What a UI atlas wants: repeating there bleeds the opposite edge of the
    /// atlas into a glyph, which looks like a font bug.
    ClampToEdge,
}

/// How to build a [`TextureSampler`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SamplerConfig {
    /// Filtering between texels, magnifying and minifying alike.
    pub filter: Filter,
    /// What happens outside the texture.
    pub wrap: Wrap,
    /// Maximum anisotropy, or `None` to disable it.
    ///
    /// Anisotropic filtering is the single most visible difference between a
    /// bring-up renderer and one that looks right: without it a floor viewed at
    /// a grazing angle blurs to a smear. It is in the required feature tier, so
    /// asking for it never fails.
    pub anisotropy: Option<f32>,
}

/// A sampler, destroyed when dropped.
#[derive(Debug)]
pub struct TextureSampler {
    handle: vk::Sampler,
    device: Arc<Device>,
}

impl TextureSampler {
    /// Create a sampler.
    ///
    /// # Errors
    ///
    /// [`RhiError`] if the device rejects it, which for these parameters means
    /// the driver is out of resources rather than the description being wrong.
    pub fn new(device: &Arc<Device>, config: &SamplerConfig) -> Result<Self, RhiError> {
        let filter = match config.filter {
            Filter::Linear => vk::Filter::LINEAR,
            Filter::Nearest => vk::Filter::NEAREST,
        };
        let mipmap = match config.filter {
            Filter::Linear => vk::SamplerMipmapMode::LINEAR,
            Filter::Nearest => vk::SamplerMipmapMode::NEAREST,
        };
        let wrap = match config.wrap {
            Wrap::Repeat => vk::SamplerAddressMode::REPEAT,
            Wrap::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
        };

        let mut info = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .mipmap_mode(mipmap)
            .address_mode_u(wrap)
            .address_mode_v(wrap)
            .address_mode_w(wrap)
            // Unclamped, so a texture with a full mip chain uses all of it. A
            // texture with one level is unaffected.
            .max_lod(vk::LOD_CLAMP_NONE);

        if let Some(anisotropy) = config.anisotropy {
            info = info.anisotropy_enable(true).max_anisotropy(anisotropy);
        }

        // SAFETY: `info` is fully initialized and borrows nothing that outlives
        // the call.
        let handle = unsafe { device.raw().create_sampler(&info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
        })
    }

    /// The underlying handle, for placing in the bindless heap.
    pub fn handle(&self) -> SamplerHandle {
        SamplerHandle(self.handle)
    }
}

impl Drop for TextureSampler {
    fn drop(&mut self) {
        // SAFETY: the sampler came from this device, and callers wait for the
        // device before dropping the objects a pending frame references — the
        // same contract every resource in this crate has.
        unsafe { self.device.raw().destroy_sampler(self.handle, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_what_a_surface_texture_wants() {
        // Linear and repeating, because that is right for nearly every texture
        // in a scene. The exceptions — a UI atlas, a golden-image checkerboard —
        // say so explicitly, which is the direction the defaults should push.
        let config = SamplerConfig::default();

        assert_eq!(config.filter, Filter::Linear);
        assert_eq!(config.wrap, Wrap::Repeat);
        assert_eq!(config.anisotropy, None, "opt in, since it costs bandwidth");
    }
}
