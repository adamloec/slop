//! The environment cube on the GPU: what a reflection reads.
//!
//! `docs/PLAN.md` §9.7 E6d. The specular half of image-based lighting, and the
//! counterpart of the nine coefficients [`Environment`](crate::Environment)
//! carries — those are the diffuse term, which has no detail left after being
//! convolved with a cosine lobe. A reflection does have detail, which is why it
//! is an image and not more coefficients.
//!
//! # A level is a roughness
//!
//! The chain is not mips of one image. `slop-cook`'s `specular` module writes
//! level zero as the environment untouched and every level below it as the same
//! sky convolved with a wider lobe, so a material's roughness selects a level.
//! The mapping is linear from zero at the sharpest to one at the smallest, and
//! **it is stated in two places that must agree** — `roughness_of` in the cooker
//! and `specularFrom` in `lib/lighting/environment.slang`. There is no way to
//! derive one from the other across a file format, so [`levels`](Sky::levels) is
//! written into the buffer the shader reads rather than assumed.
//!
//! # Uploaded once, not per frame
//!
//! An environment is scene data. Nothing here rings by frame in flight, unlike
//! [`Environment`](crate::Environment) or `Lights`, because nothing writes to it
//! after load — the day an editor lets someone swap environments live, this gets
//! rebuilt rather than rewritten, which is a different operation.

use std::sync::Arc;

use slop_core::Handle;
use slop_rhi::{
    Allocator, BindlessHeap, Device, Extent2D, Format, Image, ImageConfig, ImageKind, ImageState,
    ImageUsage, SampledImage, Sampler, SamplerConfig, Subresource, TextureSampler,
};

use crate::RenderError;
use crate::upload::Uploads;

/// A prefiltered environment cube, uploaded and in the heap.
pub struct Sky {
    image: Image,
    slot: Handle<SampledImage>,
    /// Held so the heap's descriptor stays valid; destroyed on drop.
    #[expect(dead_code, reason = "the heap references this sampler")]
    sampler: TextureSampler,
    sampler_slot: Handle<Sampler>,
    levels: u32,
}

impl Sky {
    /// Upload a cooked environment's cube and place it in the heap.
    ///
    /// Blocks until the transfer completes, which is what every load path here
    /// does — see [`Uploads`](crate::upload::Uploads).
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created or the upload
    /// fails, or [`RenderError::Layout`] if the bindless heap is full.
    pub fn upload(
        device: &Arc<Device>,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        cooked: &slop_asset::Environment,
    ) -> Result<Self, RenderError> {
        let image = Image::new(
            allocator,
            &ImageConfig {
                name: "environment",
                extent: Extent2D {
                    width: cooked.size,
                    height: cooked.size,
                },
                format: Format::Rgba16Float,
                usage: ImageUsage::TRANSFER_DST | ImageUsage::SAMPLED,
                mip_levels: cooked.mip_levels,
                kind: ImageKind::Cube,
            },
        )?;

        // Linear **and** mip-linear. The second matters more than it looks: a
        // material's roughness lands between two levels far more often than on
        // one, and sampling the nearer level instead would quantise every
        // reflection to nine discrete blurs — which reads as banding across a
        // curved surface rather than as a filtering choice.
        let sampler = TextureSampler::new(
            device,
            &SamplerConfig {
                filter: slop_rhi::Filter::Linear,
                // Clamped, though a cube view makes it nearly irrelevant: the
                // hardware wraps across faces itself, and there is nowhere for a
                // direction to fall off the edge of a sphere.
                wrap: slop_rhi::Wrap::ClampToEdge,
                ..SamplerConfig::default()
            },
        )?;

        let mut uploads = Uploads::new(device)?;

        let staging = uploads
            .stage(allocator, "environment staging", &cooked.texels)?
            .handle();

        uploads.command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::UNDEFINED,
            ImageState::TRANSFER_DST,
        );

        // One copy per face per level, all out of the one staging buffer, in the
        // order the artifact stores them. `slop_asset::environment::face` does
        // the offset arithmetic, so nothing here restates the layout.
        for level in 0..cooked.mip_levels {
            for face in 0..slop_asset::environment::FACES {
                let placed = cooked
                    .face(level, face)
                    .expect("every face of every level exists");

                uploads.command.copy_buffer_to_image_part(
                    staging,
                    placed.offset as u64,
                    image.handle(),
                    image.aspect(),
                    Extent2D {
                        width: placed.size,
                        height: placed.size,
                    },
                    Subresource { level, layer: face },
                );
            }
        }

        uploads.command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::TRANSFER_DST,
            ImageState::SHADER_READ,
        );

        uploads.finish(device)?;

        let slot = heap
            .insert_sampled_image(image.view(), ImageState::SHADER_READ)
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the environment cube",
            })?;

        let sampler_slot = heap
            .insert_sampler(sampler.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the environment sampler",
            })?;

        Ok(Self {
            image,
            slot,
            sampler,
            sampler_slot,
            levels: cooked.mip_levels,
        })
    }

    /// The heap index a shader samples the cube through.
    #[must_use]
    pub fn handle(&self) -> u32 {
        self.slot.index()
    }

    /// The heap index of the sampler that reads it.
    #[must_use]
    pub fn sampler(&self) -> u32 {
        self.sampler_slot.index()
    }

    /// How many roughness levels the chain has.
    #[must_use]
    pub fn levels(&self) -> u32 {
        self.levels
    }

    /// The image itself, for [`Graph::import`](crate::Graph::import).
    ///
    /// E6e's skybox reads level zero of this as an ordinary sampled resource,
    /// which the graph has to know about to barrier correctly.
    #[must_use]
    pub fn image(&self) -> &Image {
        &self.image
    }
}

impl std::fmt::Debug for Sky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sky")
            .field("size", &self.image.extent().width)
            .field("levels", &self.levels)
            .finish_non_exhaustive()
    }
}
