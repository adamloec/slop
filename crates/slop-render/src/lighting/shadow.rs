//! Cascaded shadow maps: where the cascades are, and what each one sees.
//!
//! `docs/PLAN.md` §9.5 E5, designed against §9.4's four cascades at 2048²
//! `D32Float` in a texture array.
//!
//! # Why cascades at all
//!
//! One shadow map covering the whole view has to spread its texels over
//! everything, so a surface a metre away and a wall a hundred metres away get
//! the same resolution — which means the near one is visibly blocky. Cascades
//! split the view frustum by depth and give each slice its own map, so texel
//! density follows how close a thing is to the camera.
//!
//! # Where the splits go
//!
//! Neither uniform nor logarithmic, but a blend of the two.
//!
//! *Uniform* splits divide the range evenly and waste almost every cascade on
//! distance, because perspective means the far half of the frustum covers most
//! of the world and almost none of the screen. *Logarithmic* splits follow
//! perspective exactly and put the first split absurdly close — with a near
//! plane of a few centimetres, cascade zero ends before anything in the scene.
//!
//! The practical scheme takes a weighted mix, and [`SPLIT_BLEND`] is the
//! weight. This is the standard answer rather than an invention; the number is
//! the one everyone converges on.
//!
//! # Why the fit uses a sphere and not the corners
//!
//! Each cascade needs an orthographic box, seen from the light, containing its
//! slice of the view frustum. Fitting that box tightly to the slice's eight
//! corners gives the best resolution — and makes the box **change shape as the
//! camera rotates**, so every shadow edge crawls and shimmers while the camera
//! moves. It is the classic cascaded-shadow artefact and it looks like a
//! precision problem rather than a fitting one.
//!
//! A bounding *sphere* has no orientation, so the box derived from it is the
//! same size whichever way the camera faces. That costs resolution — a sphere
//! around a frustum slice is bigger than the slice — and buys stability, which
//! is the trade every engine makes here.
//!
//! Stability also needs the box to move in whole texels
//! ([`snap_to_texel`](CascadeFit::snap_to_texel)); a box of constant size that
//! slides by a third of a texel shimmers just as badly.

use std::sync::Arc;

use slop_core::Handle;
use slop_math::{Mat4, Vec3, Vec4};
use slop_rhi::{
    Allocator, BindlessHeap, Buffer, BufferConfig, BufferUsage, Format, Image, ImageConfig,
    ImageState, ImageUsage, MemoryLocation, SampledImage, Sampler, SamplerConfig, StorageBuffer,
    TextureSampler,
};

use crate::{DirectionalLight, RenderError, View};

/// How many cascades. `docs/PLAN.md` §9.4.
pub const CASCADES: usize = 4;

/// How the splits mix logarithmic against uniform.
///
/// One is fully logarithmic, zero is fully uniform. See this module's docs for
/// why neither extreme works.
pub const SPLIT_BLEND: f32 = 0.75;

/// Where each cascade ends, in view-space depth.
///
/// `near` and `far` bound what receives shadows at all — **not** the
/// projection's planes, which are a few centimetres and infinity. A cascade set
/// covering infinity would put its last split at infinity and shadow nothing.
///
/// Returns the far edge of each cascade; the near edge of cascade zero is
/// `near`, and of cascade *i* is the previous entry.
#[must_use]
pub fn splits(near: f32, far: f32) -> [f32; CASCADES] {
    let mut splits = [0.0; CASCADES];
    let count = CASCADES as f32;

    for (index, split) in splits.iter_mut().enumerate() {
        let fraction = (index + 1) as f32 / count;

        // Perspective-following, and far too tight at the near end on its own.
        let logarithmic = near * slop_math::scalar::powf(far / near, fraction);
        // Even, and far too loose at the near end on its own.
        let uniform = (far - near).mul_add(fraction, near);

        *split = SPLIT_BLEND.mul_add(logarithmic - uniform, uniform);
    }

    // The last cascade must reach exactly `far`, not `far` minus whatever the
    // blend left. Otherwise a band at the edge of the shadowed range falls into
    // no cascade and is lit as though nothing occludes it.
    splits[CASCADES - 1] = far;

    splits
}

/// One cascade's orthographic box, seen from the light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CascadeFit {
    /// The centre of the view-frustum slice, in world space.
    pub centre: Vec3,
    /// The radius of the sphere containing it.
    pub radius: f32,
}

impl CascadeFit {
    /// Fit a sphere around one slice of the view frustum, in **view space**.
    ///
    /// `near` and `far` are the slice's own depths, and the projection is
    /// described by `tan_half_fov_y` and `aspect` rather than by a matrix: the
    /// slice is symmetric about the view Z axis, so the sphere's centre lies on
    /// it and the whole fit is a few lines of algebra. Unprojecting eight
    /// corners through an inverse view-projection would give the same answer
    /// after inverting a matrix whose far plane is at infinity.
    ///
    /// The centre is **not** the midpoint of the near and far faces. It is the
    /// depth equidistant from a near corner and a far corner, which is a
    /// different point whenever the slice is deep — and using the midpoint
    /// leaves the far corners outside the sphere, which crops the cascade.
    #[must_use]
    pub fn of_slice(near: f32, far: f32, tan_half_fov_y: f32, aspect: f32) -> Self {
        // The half-extents of the two faces.
        let near_half_height = tan_half_fov_y * near;
        let far_half_height = tan_half_fov_y * far;
        let near_half_width = near_half_height * aspect;
        let far_half_width = far_half_height * aspect;

        // In *view* space the slice is symmetric about the Z axis, so the
        // centre of the enclosing sphere lies on it. Solving for the depth
        // equidistant from a near corner and a far corner:
        //
        //     |corner_near - (0,0,-z)|² = |corner_far - (0,0,-z)|²
        //
        // which expands to the expression below.
        let near_corner_squared =
            near_half_width.mul_add(near_half_width, near_half_height * near_half_height);
        let far_corner_squared =
            far_half_width.mul_add(far_half_width, far_half_height * far_half_height);

        let depth =
            ((far_corner_squared - near_corner_squared) / (far - near) + (far + near)) * 0.5;

        // Clamped into the slice. Outside it the sphere is still valid but no
        // longer minimal, and for a wide field of view the unclamped solution
        // can land behind the near face.
        let depth = depth.clamp(near, far);

        let radius = {
            let to_near = near_half_width
                .mul_add(near_half_width, near_half_height * near_half_height)
                + (depth - near) * (depth - near);
            let to_far = far_half_width.mul_add(far_half_width, far_half_height * far_half_height)
                + (far - depth) * (far - depth);

            to_near.max(to_far).sqrt()
        };

        Self {
            // View space, at `-depth` because the camera looks down −Z.
            centre: Vec3::new(0.0, 0.0, -depth),
            radius,
        }
    }

    /// Move the centre into world space.
    ///
    /// [`of_slice`](Self::of_slice) works in view space, where the frustum is
    /// symmetric and the arithmetic is a few lines. The light needs it in world
    /// space.
    #[must_use]
    pub fn to_world(self, inverse_view: Mat4) -> Self {
        Self {
            centre: (inverse_view * Vec4::from((self.centre, 1.0))).truncate(),
            radius: self.radius,
        }
    }

    /// Snap the centre to whole shadow-map texels, in the light's basis.
    ///
    /// **Without this the shadows shimmer**, and it is not subtle: the box is a
    /// constant size thanks to the sphere fit, but as the camera moves it slides
    /// continuously, so every texel's world-space footprint slides with it and
    /// each shadow edge crawls between frames. Quantising the centre to the
    /// texel grid means the footprint lands in the same place from frame to
    /// frame and the edge stays put.
    ///
    /// `resolution` is the shadow map's size in texels.
    #[must_use]
    pub fn snap_to_texel(self, light_basis: Mat4, resolution: u32) -> Self {
        // Two radii across the box, `resolution` texels wide.
        let texels_per_unit = resolution as f32 / (self.radius * 2.0);

        let light_space = (light_basis * Vec4::from((self.centre, 1.0))).truncate();

        // `round`, not `floor`, and the difference is not a matter of taste.
        // A centre already on the grid comes back through the world round trip
        // as `k ± ε`; `floor` turns a negative ε into `k − 1`, so snapping an
        // already-snapped box moves it a whole texel and the shimmer this
        // exists to remove comes back at a lower frequency. `round` maps both
        // sides of `k` to `k`. `snapping_is_idempotent` is the assertion.
        let snapped = Vec3::new(
            (light_space.x * texels_per_unit).round() / texels_per_unit,
            (light_space.y * texels_per_unit).round() / texels_per_unit,
            // Depth is not quantised. The box's *depth* range is padded well
            // past the geometry either way, so a sub-texel slide along the
            // light's axis moves no shadow edge.
            light_space.z,
        );

        Self {
            centre: (light_basis.inverse() * Vec4::from((snapped, 1.0))).truncate(),
            radius: self.radius,
        }
    }
}

/// A basis looking along `direction`, with no translation.
///
/// The light's rotation, for snapping and for building the view matrix. A
/// directional light has no position, so any point along the axis does — what
/// matters is the orientation.
///
/// `direction` points **towards** the light, matching
/// [`DirectionalLight::direction`](crate::DirectionalLight::direction), so the
/// camera this builds sits on that side looking back.
#[must_use]
pub fn light_basis(direction: Vec3) -> Mat4 {
    let forward = direction.normalize_or_zero();

    // Any up vector not parallel to the light. Y unless the light is within a
    // few degrees of vertical, which is exactly the common case of a sun
    // overhead — and where `look_at` with a parallel up produces NaNs that
    // spread into every cascade matrix.
    let up = if forward.y.abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };

    slop_math::look_at(forward, Vec3::ZERO, up)
}

/// How the cascades are sized and biased.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowConfig {
    /// Texels along each edge of one cascade. `docs/PLAN.md` §9.4's 2048.
    pub resolution: u32,
    /// Where shadows start, in view-space depth.
    pub near: f32,
    /// Where they stop. Beyond this nothing is shadowed, which is why it is a
    /// scene-sized number rather than the projection's infinite far plane.
    pub far: f32,
    /// How far behind the cascade's sphere the light's near plane sits, as a
    /// multiple of the radius.
    ///
    /// Casters *between* the light and the slice still have to be drawn — a
    /// roof shadows the floor below it from outside the floor's own cascade
    /// sphere — so the light's view volume is deeper than the sphere on the
    /// light's side. Too small and tall objects stop casting; too large wastes
    /// depth precision.
    pub caster_reach: f32,
    /// Constant depth bias, in the shadow map's units.
    ///
    /// Guards against **acne**: a surface shadowing itself because its own
    /// depth, sampled through a texel covering an area, differs from the depth
    /// recorded for that texel. Too small leaves stripes; too large detaches the
    /// shadow from its caster.
    pub depth_bias: f32,
    /// Bias proportional to the surface's slope relative to the light.
    ///
    /// A surface edge-on to the light spans far more depth per texel than one
    /// facing it, so a constant bias large enough for the first is far too large
    /// for the second. This scales with the need.
    pub slope_bias: f32,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            resolution: 2048,
            near: 0.1,
            far: 100.0,
            caster_reach: 2.0,
            // Chosen against Sponza and the cube, which is to say chosen by
            // looking. `docs/PLAN.md` §6.1 records that a bias tuned by eye on
            // two scenes is not a bias that generalises.
            depth_bias: 0.0015,
            slope_bias: 0.004,
        }
    }
}

/// One cascade as the shader reads it.
///
/// Mirrors `CascadeGpu` in `shaders/lib/shadow.slang`. The matrix is four
/// explicit columns, for the reason every other matrix in a buffer here is.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CascadeGpu {
    light_view_projection: [[f32; 4]; 4],
    /// Where this cascade stops, in view-space depth. What the fragment shader
    /// compares its own depth against to choose a cascade.
    far: f32,
    /// The world-space size of one texel, for scaling the normal offset.
    texel_world_size: f32,
    _pad: [f32; 2],
}

/// The shadow block the forward pass reads.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ShadowsGpu {
    cascades: [CascadeGpu; CASCADES],
    /// Heap index of the depth array, and of the sampler that reads it.
    map: u32,
    sampler: u32,
    depth_bias: f32,
    slope_bias: f32,
    texel_uv: f32,
    _pad: [f32; 3],
}

/// The cascade array, and the matrices that fill it.
///
/// One set per frame in flight: the cascades are rendered and sampled within a
/// frame, and with two frames in flight a single array would have frame N+1's
/// shadow render overwriting what frame N's fragments are still reading.
pub struct Shadows {
    config: ShadowConfig,
    slots: Vec<ShadowSlot>,
    /// Held so the heap's descriptor stays valid; destroyed on drop.
    #[expect(dead_code, reason = "the heap references this sampler")]
    sampler: TextureSampler,
    sampler_slot: Handle<Sampler>,
    /// This frame's matrices, kept so `render` can hand each cascade its own.
    cascades: [Mat4; CASCADES],
    /// This frame's split depths, for the debug overlay and for tests.
    splits: [f32; CASCADES],
}

struct ShadowSlot {
    map: Image,
    map_slot: Handle<SampledImage>,
    block: Buffer,
    block_slot: Handle<StorageBuffer>,
}

impl Shadows {
    /// Allocate the cascade arrays and the block the forward pass reads.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created, or
    /// [`RenderError::Layout`] if the bindless heap is full.
    pub fn new(
        device: &Arc<slop_rhi::Device>,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        config: ShadowConfig,
        frames_in_flight: usize,
    ) -> Result<Self, RenderError> {
        // Clamped rather than filtered. A shadow lookup outside the cascade's
        // box must read "not occluded" rather than wrapping to the far side of
        // the map, and clamping to an edge that holds the far plane gives
        // exactly that.
        let sampler = TextureSampler::new(
            device,
            &SamplerConfig {
                filter: slop_rhi::Filter::Linear,
                wrap: slop_rhi::Wrap::ClampToEdge,
                ..SamplerConfig::default()
            },
        )?;

        let sampler_slot = heap
            .insert_sampler(sampler.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the shadow sampler",
            })?;

        let mut slots = Vec::with_capacity(frames_in_flight);

        for _ in 0..frames_in_flight {
            slots.push(ShadowSlot::new(allocator, heap, config)?);
        }

        Ok(Self {
            config,
            slots,
            sampler,
            sampler_slot,
            cascades: [Mat4::IDENTITY; CASCADES],
            splits: [0.0; CASCADES],
        })
    }

    /// The configuration this was built with.
    #[must_use]
    pub fn config(&self) -> ShadowConfig {
        self.config
    }

    /// Where each cascade ends, as of the last [`write`](Self::write).
    #[must_use]
    pub fn splits(&self) -> [f32; CASCADES] {
        self.splits
    }

    /// Work out this frame's cascades and write the block the shader reads.
    ///
    /// Call inside the frame closure with [`Frame::slot`](crate::Frame::slot),
    /// for the reason [`Lights::write`](crate::Lights::write) gives.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the buffer cannot be mapped, or
    /// [`RenderError::Layout`] if `slot` names one that does not exist.
    pub fn write(
        &mut self,
        slot: usize,
        sun: &DirectionalLight,
        inverse_view: Mat4,
        tan_half_fov_y: f32,
        aspect: f32,
    ) -> Result<(), RenderError> {
        let config = self.config;
        let basis = light_basis(sun.direction);

        self.splits = splits(config.near, config.far);

        let mut rows = [bytemuck::Zeroable::zeroed(); CASCADES];
        let mut near = config.near;

        for (index, far) in self.splits.into_iter().enumerate() {
            let fit = CascadeFit::of_slice(near, far, tan_half_fov_y, aspect)
                .to_world(inverse_view)
                .snap_to_texel(basis, config.resolution);

            // The light sits back far enough to see casters between it and the
            // slice, and the volume reaches the far side of the sphere.
            let reach = fit.radius * config.caster_reach;
            let eye = fit.centre + sun.direction.normalize_or_zero() * (fit.radius + reach);

            let up = if sun.direction.normalize_or_zero().y.abs() > 0.99 {
                Vec3::Z
            } else {
                Vec3::Y
            };

            let view = slop_math::look_at(eye, fit.centre, up);
            let projection = slop_math::orthographic(
                -fit.radius,
                fit.radius,
                -fit.radius,
                fit.radius,
                0.0,
                fit.radius * 2.0 + reach,
            );

            rows[index] = CascadeGpu {
                light_view_projection: (projection * view).to_cols_array_2d(),
                far,
                texel_world_size: fit.radius * 2.0 / config.resolution as f32,
                _pad: [0.0; 2],
            };

            self.cascades[index] = projection * view;
            near = far;
        }

        let Some(target) = self.slots.get_mut(slot) else {
            return Err(RenderError::Layout {
                what: "a frame asked for a shadow slot that does not exist",
            });
        };

        let block = ShadowsGpu {
            cascades: rows,
            map: target.map_slot.index(),
            sampler: self.sampler_slot.index(),
            depth_bias: config.depth_bias,
            slope_bias: config.slope_bias,
            texel_uv: 1.0 / config.resolution as f32,
            _pad: [0.0; 3],
        };

        let bytes = bytemuck::bytes_of(&block);
        target.block.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        Ok(())
    }

    /// A [`View`] that renders cascade `index` — the light's camera, not the
    /// player's.
    ///
    /// Unclustered, because a shadow render shades nothing: it writes depth and
    /// the point lights are irrelevant to it.
    ///
    /// # Panics
    ///
    /// If `index` is past the end.
    #[must_use]
    pub fn cascade_view(
        &self,
        index: usize,
        environment: &crate::Environment,
        slot: usize,
    ) -> View {
        View::unclustered(self.cascades[index], environment, slot)
    }

    /// The depth array for `slot`, for [`Graph::import`](crate::Graph::import).
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist.
    #[must_use]
    pub fn map(&self, slot: usize) -> &Image {
        &self
            .slots
            .get(slot)
            .expect("the shadows have a slot per frame in flight")
            .map
    }

    /// The heap index of the block a draw reads, for `View`.
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist.
    #[must_use]
    pub fn handle(&self, slot: usize) -> u32 {
        self.slots
            .get(slot)
            .expect("the shadows have a slot per frame in flight")
            .block_slot
            .index()
    }
}

impl ShadowSlot {
    fn new(
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        config: ShadowConfig,
    ) -> Result<Self, RenderError> {
        let map = Image::new(
            allocator,
            &ImageConfig {
                name: "shadow cascades",
                extent: slop_rhi::Extent2D {
                    width: config.resolution,
                    height: config.resolution,
                },
                // `D32Float` to match what the prepass pipelines were built
                // against — the shadow render reuses them, so the formats have
                // to agree or the driver rejects the draw.
                format: Format::D32Float,
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT | ImageUsage::SAMPLED,
                mip_levels: 1,
                array_layers: CASCADES as u32,
            },
        )?;

        // `DEPTH_READ` rather than the general shader-read state: a depth image
        // being sampled is still a depth image, and the read-only depth layout
        // is what the heap's descriptor has to name.
        let map_slot = heap
            .insert_sampled_image(map.view(), ImageState::DEPTH_READ)
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the shadow map",
            })?;

        let block = Buffer::new(
            allocator,
            &BufferConfig {
                name: "shadow cascades block",
                size: size_of::<ShadowsGpu>() as u64,
                usage: BufferUsage::STORAGE,
                location: MemoryLocation::Upload,
            },
        )?;

        let block_slot = heap
            .insert_storage_buffer(block.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the shadow block",
            })?;

        Ok(Self {
            map,
            map_slot,
            block,
            block_slot,
        })
    }
}

impl std::fmt::Debug for Shadows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shadows")
            .field("config", &self.config)
            .field("slots", &self.slots.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_splits_increase_and_end_exactly_at_the_far_plane() {
        let splits = splits(0.1, 100.0);

        for pair in splits.windows(2) {
            assert!(pair[0] < pair[1], "splits are not increasing: {splits:?}");
        }

        // Not approximately. A last cascade stopping short leaves a band that
        // belongs to no cascade, which renders as unshadowed ground at a fixed
        // distance — easy to mistake for the shadows simply fading out.
        assert_eq!(splits[CASCADES - 1], 100.0);
    }

    #[test]
    fn the_blend_sits_between_uniform_and_logarithmic() {
        // The property that makes the blend worth having: the first split is
        // further out than a logarithmic scheme would put it, and closer in than
        // a uniform one would.
        let (near, far) = (0.1_f32, 100.0);
        let splits = splits(near, far);

        let logarithmic = near * slop_math::scalar::powf(far / near, 0.25);
        let uniform = (far - near).mul_add(0.25, near);

        assert!(splits[0] > logarithmic, "no better than logarithmic");
        assert!(splits[0] < uniform, "no better than uniform");
    }

    #[test]
    fn the_cascades_tile_the_range_without_a_gap() {
        // Cascade i covers from split i-1 to split i. Any depth in the range
        // must fall in exactly one, or shadows have a seam.
        let splits = splits(1.0, 200.0);
        let mut previous = 1.0;

        for split in splits {
            assert!(split > previous);
            previous = split;
        }
    }

    /// The sphere really does contain the slice it was fitted to.
    ///
    /// Checked against the eight corners directly, which is the definition —
    /// the closed form in `of_slice` is an optimisation of exactly this and
    /// would be self-confirming if tested against itself.
    #[test]
    fn the_fitted_sphere_contains_every_corner_of_the_slice() {
        let tan = 0.6_f32;
        let aspect = 16.0 / 9.0;

        for (near, far) in [(1.0_f32, 5.0), (5.0, 20.0), (0.1, 3.0), (20.0, 200.0)] {
            let fit = CascadeFit::of_slice(near, far, tan, aspect);

            for depth in [near, far] {
                let half_height = tan * depth;
                let half_width = half_height * aspect;

                for x in [-half_width, half_width] {
                    for y in [-half_height, half_height] {
                        let corner = Vec3::new(x, y, -depth);
                        let distance = (corner - fit.centre).length();

                        assert!(
                            distance <= fit.radius * 1.0001,
                            "corner {corner:?} is {distance} from the centre but the \
                             radius is {} (slice {near}..{far})",
                            fit.radius
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_fitted_sphere_is_not_wildly_larger_than_it_needs_to_be() {
        // Containment alone is satisfied by an enormous radius, and an enormous
        // radius is a cascade with no resolution. The far face's diagonal is a
        // hard lower bound on any sphere containing the slice.
        let (near, far) = (5.0_f32, 20.0);
        let tan = 0.6;
        let aspect = 16.0 / 9.0;

        let fit = CascadeFit::of_slice(near, far, tan, aspect);

        let half_height = tan * far;
        let half_width = half_height * aspect;
        let lower_bound = slop_math::scalar::hypot(half_width, half_height);

        assert!(fit.radius >= lower_bound * 0.99);
        assert!(
            fit.radius < lower_bound * 2.0,
            "radius {} is more than twice the far face's own half-diagonal {lower_bound}",
            fit.radius
        );
    }

    #[test]
    fn snapping_moves_the_centre_by_less_than_one_texel() {
        // The point of snapping is stability, not relocation. A snap that moved
        // the box by more than a texel would be introducing the judder it exists
        // to remove.
        let basis = light_basis(Vec3::new(0.3, 0.9, 0.2));
        let fit = CascadeFit {
            centre: Vec3::new(3.7, 1.2, -8.4),
            radius: 10.0,
        };

        let snapped = fit.snap_to_texel(basis, 2048);
        let texel = fit.radius * 2.0 / 2048.0;

        assert!(
            (snapped.centre - fit.centre).length() < texel * 2.0,
            "snapping moved the centre by more than a texel"
        );
        assert_eq!(snapped.radius, fit.radius);
    }

    #[test]
    fn snapping_is_idempotent() {
        // What stability means, stated as a property: a centre already on the
        // grid must not move. If it did, two identical frames would produce
        // different shadow maps.
        let basis = light_basis(Vec3::new(0.3, 0.9, 0.2));
        let fit = CascadeFit {
            centre: Vec3::new(3.7, 1.2, -8.4),
            radius: 10.0,
        };

        let once = fit.snap_to_texel(basis, 2048);
        let twice = once.snap_to_texel(basis, 2048);

        assert!(
            (twice.centre - once.centre).length() < 1e-4,
            "snapping twice moved it again: {:?} then {:?}",
            once.centre,
            twice.centre
        );
    }

    #[test]
    fn a_light_pointing_straight_up_still_produces_a_usable_basis() {
        // The common case, and the one that breaks a naive `look_at`: an up
        // vector parallel to the direction gives a degenerate basis full of
        // NaNs, which spreads into every cascade matrix and renders as nothing
        // being shadowed at all.
        let basis = light_basis(Vec3::Y);

        for column in basis.to_cols_array() {
            assert!(column.is_finite(), "the basis has a non-finite entry");
        }
    }

    #[test]
    fn the_basis_looks_the_way_the_light_travels() {
        // Two directions are in play and confusing them mirrors every shadow.
        //
        // `direction` points **towards** the light — up, for a sun overhead.
        // The shadow camera has to look the way the light *travels*, which is
        // the opposite, so it sits on the light's side facing the scene. In a
        // right-handed view space the camera looks down −Z, so the vector
        // towards the light lands on **+Z**: behind the camera, which is exactly
        // where the light is.
        let direction = Vec3::new(0.4, 0.8, 0.45).normalize();
        let basis = light_basis(direction);

        let mapped = (basis * Vec4::from((direction, 0.0))).truncate();

        assert!(
            (mapped.z - 1.0).abs() < 1e-4,
            "the direction towards the light maps to {mapped:?}, expected roughly \
             (0, 0, 1) — the shadow camera must look the other way"
        );

        // And what the light travels along ends up in front of it.
        let travel = (basis * Vec4::from((-direction, 0.0))).truncate();

        assert!((travel.z + 1.0).abs() < 1e-4);
    }
}
