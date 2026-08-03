//! The cluster grid: which lights reach which part of the view.
//!
//! `docs/PLAN.md` §9.5 E4, designed against §9.4. The forward pass shades a
//! fragment with the lights that reach it; without a structure saying which
//! those are, it has to consider all of them, and the cost is fragments times
//! lights. Clustering divides the view frustum into cells, decides once per
//! frame which lights touch each cell, and leaves the forward pass reading a
//! handful of indices.
//!
//! # The grid
//!
//! 16 × 9 × 24 — §9.4's decision, and the well-trodden configuration. The tile
//! count follows a 16:9 aspect ratio so cells stay roughly square on screen; the
//! 24 depth slices are spaced **exponentially**, because perspective compresses
//! distance and uniform slices would put almost every fragment in the first
//! one.
//!
//! # Why the near and far here are not the projection's
//!
//! `docs/DESIGN.md` §2.2 uses reverse-Z with an **infinite** far plane, and the
//! exponential slice mapping needs a finite range to divide. So [`ClusterGrid`]
//! carries its own `far` — the distance clustering covers — and anything beyond
//! it lands in the last slice. That is not a hole: the last slice is a real
//! cluster with a real light list, it is just deeper than the others.
//!
//! Getting this wrong in the tempting direction — reusing the projection's near
//! plane, which is tiny — wastes slices on the first centimetre of the view.
//!
//! # The mapping, and why it is stored as a scale and a bias
//!
//! Slice *k* covers view-space depth `near · (far/near)^(k/slices)` to the same
//! with `k+1`. Inverting that for a fragment at depth *z*:
//!
//! ```text
//! k = floor( log(z) · slices/log(far/near)  −  slices·log(near)/log(far/near) )
//!            \_______  scale  _______/         \________  bias  ___________/
//! ```
//!
//! Both halves are constant for a frame, so they are computed once on the CPU
//! and read by both shaders. **Both** is the point: the compute pass places
//! lights into slices and the fragment shader looks its own slice up, and the
//! two disagreeing puts a fragment in a cell whose light list was built for
//! somewhere else. One buffer, read twice, is what stops that.

use std::sync::Arc;

use slop_asset::Reflection;
use slop_core::Handle;
// `scalar` rather than `f32`'s own methods: these numbers are written into a
// buffer the GPU reads, so a machine that rounded `ln` differently would build a
// different grid — `docs/DESIGN.md` §2.14, and `clippy.toml` enforces it.
use slop_math::{Mat4, Vec3, scalar};
use slop_rhi::{
    Allocator, BindlessHeap, Buffer, BufferConfig, BufferUsage, CommandBuffer, ComputePipeline,
    Device, MemoryLocation, PipelineLayout, PipelineLayoutConfig, ShaderModule, ShaderStage,
    StorageBuffer,
};

use crate::{Lights, RenderError};

/// How the view frustum is divided.
///
/// `docs/PLAN.md` §9.4 fixes the counts; they are fields rather than constants
/// because the tile counts want to follow the window's aspect ratio eventually,
/// and because a test that can build a 2 × 2 × 2 grid can check the mapping by
/// hand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterGrid {
    /// Tiles across the screen.
    pub tiles_x: u32,
    /// Tiles down the screen.
    pub tiles_y: u32,
    /// Depth slices.
    pub slices: u32,
    /// Where clustering starts, in view-space depth.
    pub near: f32,
    /// Where it stops. Fragments beyond this land in the last slice.
    pub far: f32,
    /// How many lights one cluster may list.
    ///
    /// A fixed stride rather than a compacted list with an atomic allocator.
    /// The waste is bounded and small — the default grid at the default stride
    /// is under a megabyte — and the *seam* is the same either way, because a
    /// cluster's range is written as an offset and a count rather than derived
    /// from its index. Compaction changes how the offset is computed and
    /// nothing else. `docs/PLAN.md` §6.1 carries the row.
    pub max_per_cluster: u32,
}

impl Default for ClusterGrid {
    fn default() -> Self {
        Self {
            tiles_x: 16,
            tiles_y: 9,
            slices: 24,
            // A centimetre, in a scene measured in metres. Far closer than
            // anything gets shaded, and far enough from zero that the logarithm
            // below stays well conditioned.
            near: 0.1,
            far: 500.0,
            max_per_cluster: 64,
        }
    }
}

impl ClusterGrid {
    /// How many cells there are.
    #[must_use]
    pub fn count(self) -> u32 {
        self.tiles_x * self.tiles_y * self.slices
    }

    /// How many light indices the list buffer holds.
    #[must_use]
    pub fn index_capacity(self) -> u32 {
        self.count() * self.max_per_cluster
    }

    /// The multiplier in the depth-to-slice mapping. See this module's docs.
    #[must_use]
    pub fn slice_scale(self) -> f32 {
        self.slices as f32 / scalar::ln(self.far / self.near)
    }

    /// The offset in the depth-to-slice mapping.
    #[must_use]
    pub fn slice_bias(self) -> f32 {
        -(self.slices as f32) * scalar::ln(self.near) / scalar::ln(self.far / self.near)
    }

    /// Which depth slice a view-space depth falls in.
    ///
    /// Clamped at both ends: nearer than `near` is slice zero, and beyond `far`
    /// is the last one. On the CPU only — the shaders apply
    /// [`slice_scale`](Self::slice_scale) and [`slice_bias`](Self::slice_bias)
    /// themselves — and it exists so the mapping can be checked against values
    /// worked out by hand rather than against itself.
    #[must_use]
    pub fn slice_of(self, depth: f32) -> u32 {
        if depth <= self.near {
            return 0;
        }

        let slice = scalar::ln(depth).mul_add(self.slice_scale(), self.slice_bias());

        (slice as u32).min(self.slices - 1)
    }

    /// The view-space depth where slice `index` begins.
    ///
    /// The inverse of [`slice_of`](Self::slice_of), for tests and for working
    /// out what a grid actually covers.
    #[must_use]
    pub fn slice_start(self, index: u32) -> f32 {
        self.near * scalar::powf(self.far / self.near, index as f32 / self.slices as f32)
    }

    /// One cluster's bounds in **view space**, as an axis-aligned box.
    ///
    /// View space is where the cells are axis-aligned; in world space they are
    /// arbitrarily oriented and the sphere test stops being cheap. This is the
    /// CPU twin of what `shaders/passes/cluster_build.slang` computes, and
    /// exists so that the shader's version can be checked against something
    /// rather than trusted.
    ///
    /// `tan_half_fov_y` and `aspect` describe the projection. A symmetric
    /// perspective projection is assumed — which is what `slop_math` builds and
    /// what §9.4's frame uses — and an off-centre one would need the frustum
    /// planes instead.
    ///
    /// # Two sign conventions meet here, and both are easy to get wrong
    ///
    /// `slop_math::look_at` is **right-handed**, so the camera looks down `-Z`
    /// and everything in front of it has negative `z`. And
    /// `slop_math::perspective` **flips Y** for Vulkan's clip space, so screen
    /// coordinates increasing downwards correspond to view-space `y` increasing
    /// *upwards*.
    ///
    /// So a *depth* — what [`slice_of`](Self::slice_of) takes, and what a
    /// fragment reads out of `1/SV_Position.w` — is the positive distance
    /// `-z`, while the box this returns lives in real view space with `z`
    /// negative. Keeping the box in a private convention where depth is
    /// positive would be tidier right up until a light position arrives through
    /// an ordinary view matrix and lands somewhere else entirely.
    #[must_use]
    pub fn bounds(self, cell: (u32, u32, u32), tan_half_fov_y: f32, aspect: f32) -> (Vec3, Vec3) {
        let (x, y, z) = cell;

        // Normalised device coordinates for the tile's edges, in [-1, 1]. Tile
        // zero is at the *top* of the screen, which is NDC −1 under Vulkan's
        // downward Y.
        let ndc_x0 = (x as f32 / self.tiles_x as f32).mul_add(2.0, -1.0);
        let ndc_x1 = ((x + 1) as f32 / self.tiles_x as f32).mul_add(2.0, -1.0);
        let ndc_y0 = (y as f32 / self.tiles_y as f32).mul_add(2.0, -1.0);
        let ndc_y1 = ((y + 1) as f32 / self.tiles_y as f32).mul_add(2.0, -1.0);

        let half_height = tan_half_fov_y;
        let half_width = tan_half_fov_y * aspect;

        // The eight corners, rather than reasoning about which is extremal. A
        // tile spanning the centre line has its widest edge at neither end, and
        // a min/max over the corners is right without a case analysis — which
        // also means the sign flips below need no separate thought.
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);

        for depth in [self.slice_start(z), self.slice_start(z + 1)] {
            for ndc_x in [ndc_x0, ndc_x1] {
                for ndc_y in [ndc_y0, ndc_y1] {
                    let corner = Vec3::new(
                        ndc_x * half_width * depth,
                        // Negated: the projection's Y flip means NDC −1 is the
                        // top of the screen and the *top* is positive Y in view
                        // space.
                        -ndc_y * half_height * depth,
                        // Negated: right-handed, so in front of the camera is
                        // negative Z.
                        -depth,
                    );

                    min = min.min(corner);
                    max = max.max(corner);
                }
            }
        }

        (min, max)
    }
}

/// The grid as both shaders read it.
///
/// Mirrors `ClusterGridGpu` in `shaders/lib/cluster.slang`. Everything the
/// cluster build and the forward pass need is here, in one buffer, and that is
/// the point rather than a convenience: the build places lights into cells and
/// the forward pass looks its own cell up, and the two disagreeing puts a
/// fragment in a cluster whose light list was built for somewhere else. Nothing
/// reports that — it looks like lights in the wrong place.
///
/// The view matrix is four explicit columns for the reason [`InstanceGpu`]
/// gives: a matrix in a structured buffer has a layout convention on each side.
///
/// [`InstanceGpu`]: crate::MeshRenderer
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ClusterGridGpu {
    view_columns: [[f32; 4]; 4],

    tiles_x: u32,
    tiles_y: u32,
    slices: u32,
    max_per_cluster: u32,

    slice_scale: f32,
    slice_bias: f32,
    z_near: f32,
    z_far: f32,

    screen: [f32; 2],

    tan_half_fov_y: f32,
    aspect: f32,

    lights: u32,
    light_count: u32,
    ranges: u32,
    indices: u32,
}

/// One cluster's light indices: where they start, and how many.
///
/// Mirrors `ClusterRange` in `shaders/lib/cluster.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ClusterRangeGpu {
    offset: u32,
    count: u32,
}

/// Push constants for the build pass, matching `cluster_build.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BuildPush {
    grid: u32,
}

/// What the camera has to say for the grid to be built.
///
/// A struct rather than four arguments, for `CONVENTIONS.md` §5.1's reason —
/// and because `view` and the projection parameters have to describe the *same*
/// camera. Passing them separately is how a frame ends up clustering against
/// last frame's view.
#[derive(Debug, Clone, Copy)]
pub struct ClusterCamera {
    /// World to view. Right-handed, so in front of the camera is negative `z`.
    pub view: Mat4,
    /// The tangent of half the vertical field of view.
    pub tan_half_fov_y: f32,
    /// Width over height.
    pub aspect: f32,
    /// The target's size in pixels, for turning a fragment into a tile.
    pub screen: (f32, f32),
}

/// The cluster grid on the GPU: the build pass, and the buffers it fills.
///
/// One set of buffers per frame in flight. The build pass writes them and the
/// forward pass of the *same* frame reads them, so with two frames in flight a
/// single set would have frame N+1's build overwriting what frame N's fragments
/// are still reading — the hazard [`Frame::slot`](crate::Frame::slot) exists
/// for.
pub struct Clusters {
    grid: ClusterGrid,
    pipeline: ComputePipeline,
    slots: Vec<ClusterSlot>,
    /// Threads per workgroup, from the cooked reflection rather than restated.
    workgroup: u32,
    push_constant_bytes: u32,
}

/// One in-flight slot's buffers.
struct ClusterSlot {
    /// The configuration both shaders read. Rewritten every frame — the view
    /// matrix changes, and so does the screen size after a resize.
    config: Buffer,
    config_slot: Handle<StorageBuffer>,
    /// One [`ClusterRangeGpu`] per cluster.
    ranges: Buffer,
    ranges_slot: Handle<StorageBuffer>,
    /// The light indices themselves.
    indices: Buffer,
    indices_slot: Handle<StorageBuffer>,
}

impl Clusters {
    /// Build the compute pipeline and allocate one set of buffers per slot.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created,
    /// [`RenderError::Layout`] if the heap is full or the shader is not a
    /// compute shader.
    pub fn new(
        device: &Arc<Device>,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        module: &ShaderModule,
        reflection: &Reflection,
        grid: ClusterGrid,
        frames_in_flight: usize,
    ) -> Result<Self, RenderError> {
        let push_constant_bytes = reflection.push_constant_bytes;

        if push_constant_bytes as usize > size_of::<BuildPush>() {
            return Err(RenderError::Layout {
                what: "the cluster build shader's push constant block is larger than this writes",
            });
        }

        // From the cooked reflection, so the dispatch and `[numthreads]` cannot
        // disagree — the failure mode of restating it is a grid that is
        // partially built, which looks like unlit regions.
        let workgroup = reflection
            .thread_group
            .ok_or(RenderError::Layout {
                what: "the cluster build shader has no compute entry point",
            })?
            .first()
            .copied()
            .filter(|size| *size > 0)
            .ok_or(RenderError::Layout {
                what: "the cluster build shader declares a zero-wide workgroup",
            })?;

        let layout = Arc::new(PipelineLayout::new(
            device,
            &PipelineLayoutConfig {
                heap: Some(heap),
                push_constant_bytes,
            },
        )?);

        let pipeline = ComputePipeline::new(
            device,
            &layout,
            ShaderStage {
                module,
                entry: c"clusterBuildMain",
            },
        )?;

        let mut slots = Vec::with_capacity(frames_in_flight);

        for _ in 0..frames_in_flight {
            slots.push(ClusterSlot::new(allocator, heap, grid)?);
        }

        Ok(Self {
            grid,
            pipeline,
            slots,
            workgroup,
            push_constant_bytes,
        })
    }

    /// The grid this was built for.
    #[must_use]
    pub fn grid(&self) -> ClusterGrid {
        self.grid
    }

    /// Write this frame's configuration, ready for [`build`](Self::build).
    ///
    /// Call inside the frame closure with [`Frame::slot`](crate::Frame::slot),
    /// for the reason [`Lights::write`] gives: `render` has already waited for
    /// this slot's previous submission, so the GPU has finished reading what was
    /// there.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the buffer cannot be mapped, or
    /// [`RenderError::Layout`] if `slot` names one that does not exist.
    pub fn write(
        &mut self,
        slot: usize,
        camera: &ClusterCamera,
        lights: &Lights,
    ) -> Result<(), RenderError> {
        let grid = self.grid;
        let light_handle = lights.handle(slot);
        let light_count = lights.count();

        let Some(target) = self.slots.get_mut(slot) else {
            return Err(RenderError::Layout {
                what: "a frame asked for a cluster slot that does not exist",
            });
        };

        let config = ClusterGridGpu {
            view_columns: camera.view.to_cols_array_2d(),
            tiles_x: grid.tiles_x,
            tiles_y: grid.tiles_y,
            slices: grid.slices,
            max_per_cluster: grid.max_per_cluster,
            slice_scale: grid.slice_scale(),
            slice_bias: grid.slice_bias(),
            z_near: grid.near,
            z_far: grid.far,
            screen: [camera.screen.0, camera.screen.1],
            tan_half_fov_y: camera.tan_half_fov_y,
            aspect: camera.aspect,
            lights: light_handle,
            light_count,
            ranges: target.ranges_slot.index(),
            indices: target.indices_slot.index(),
        };

        let bytes = bytemuck::bytes_of(&config);
        target.config.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        Ok(())
    }

    /// Record the build dispatch.
    ///
    /// Names no barrier: what this writes and what reads it are declared to the
    /// graph, which derives them.
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist, which means the frame renderer
    /// was built with more in-flight slots than this was.
    pub fn build(&self, command: &CommandBuffer, heap: &BindlessHeap, slot: usize) {
        let target = self
            .slots
            .get(slot)
            .expect("the cluster grid has a slot per frame in flight");

        let push = BuildPush {
            grid: target.config_slot.index(),
        };

        let compute = command.bind_compute(&self.pipeline);
        compute.bind_heap(heap);
        compute.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);

        // Rounded up, so the last workgroup runs past the end — which the shader
        // checks for. Rounding *down* would leave the final clusters never
        // built, and an unbuilt cluster reads as a region with no lights rather
        // than as a failure.
        compute.dispatch(self.grid.count().div_ceil(self.workgroup), 1, 1);
    }

    /// The heap index of the configuration a draw reads, for `View`.
    ///
    /// # Panics
    ///
    /// If `slot` names one that does not exist.
    #[must_use]
    pub fn handle(&self, slot: usize) -> u32 {
        self.slots
            .get(slot)
            .expect("the cluster grid has a slot per frame in flight")
            .config_slot
            .index()
    }

    /// The buffers the graph must barrier, for one slot.
    ///
    /// Both, because the build writes both and the forward pass reads both.
    /// Returned rather than declared here: what a pass touches is the *caller's*
    /// declaration to make, which is what keeps the graph the single place
    /// barriers come from.
    #[must_use]
    pub fn buffers(&self, slot: usize) -> [slop_rhi::BufferHandle; 2] {
        let target = self
            .slots
            .get(slot)
            .expect("the cluster grid has a slot per frame in flight");

        [target.ranges.handle(), target.indices.handle()]
    }
}

impl ClusterSlot {
    fn new(
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        grid: ClusterGrid,
    ) -> Result<Self, RenderError> {
        let config = Buffer::new(
            allocator,
            &BufferConfig {
                name: "cluster grid",
                size: size_of::<ClusterGridGpu>() as u64,
                usage: BufferUsage::STORAGE,
                // Host-visible: rewritten every frame with the camera's view.
                location: MemoryLocation::Upload,
            },
        )?;

        let ranges = Buffer::new(
            allocator,
            &BufferConfig {
                name: "cluster ranges",
                size: u64::from(grid.count()) * size_of::<ClusterRangeGpu>() as u64,
                // `TRANSFER_SRC` so this can be read back, which is not a debug
                // affordance but the only way the pass is checkable at all:
                // a correct assignment and one that lists every light in every
                // cell produce the *same image*, so the final frame cannot tell
                // them apart. `tests/cluster.rs` compares the whole grid against
                // the CPU twin in this module.
                usage: BufferUsage::STORAGE | BufferUsage::TRANSFER_SRC,
                // Device-only: written by compute, read by the fragment shader,
                // never mapped.
                location: MemoryLocation::DeviceOnly,
            },
        )?;

        let indices = Buffer::new(
            allocator,
            &BufferConfig {
                name: "cluster light indices",
                size: u64::from(grid.index_capacity()) * 4,
                usage: BufferUsage::STORAGE | BufferUsage::TRANSFER_SRC,
                location: MemoryLocation::DeviceOnly,
            },
        )?;

        let full = || RenderError::Layout {
            what: "the bindless heap had no room for a cluster buffer",
        };

        let config_slot = heap
            .insert_storage_buffer(config.handle())
            .ok_or_else(full)?;
        let ranges_slot = heap
            .insert_storage_buffer(ranges.handle())
            .ok_or_else(full)?;
        let indices_slot = heap
            .insert_storage_buffer(indices.handle())
            .ok_or_else(full)?;

        Ok(Self {
            config,
            config_slot,
            ranges,
            ranges_slot,
            indices,
            indices_slot,
        })
    }
}

impl std::fmt::Debug for Clusters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clusters")
            .field("grid", &self.grid)
            .field("slots", &self.slots.len())
            .field("workgroup", &self.workgroup)
            .finish()
    }
}

/// Whether a sphere touches an axis-aligned box.
///
/// The test the cluster build runs once per light per cell. Written here as
/// well as in the shader because it is the one piece of the assignment that can
/// be checked exhaustively on the CPU, and because getting it subtly wrong —
/// testing the centre rather than the nearest point — assigns lights to too few
/// cells and shows as light popping at cell boundaries rather than as an error.
#[must_use]
pub fn sphere_touches_box(centre: Vec3, radius: f32, min: Vec3, max: Vec3) -> bool {
    // The nearest point of the box to the centre, per axis. Inside the box on
    // an axis contributes nothing, which is what makes this work for a centre
    // inside the box as well as outside it.
    let nearest = centre.clamp(min, max);
    let offset = centre - nearest;

    offset.dot(offset) <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid whose numbers are easy to reason about by hand.
    fn grid() -> ClusterGrid {
        ClusterGrid {
            tiles_x: 2,
            tiles_y: 2,
            slices: 4,
            near: 1.0,
            far: 16.0,
            max_per_cluster: 8,
        }
    }

    #[test]
    fn the_slice_boundaries_are_a_geometric_series() {
        // near · (far/near)^(k/slices) with near = 1 and far = 16 over four
        // slices is 1, 2, 4, 8, 16 — which is why these numbers were chosen.
        let grid = grid();

        for (index, expected) in [1.0, 2.0, 4.0, 8.0, 16.0].into_iter().enumerate() {
            let start = grid.slice_start(index as u32);

            assert!(
                (start - expected).abs() < 1e-4,
                "slice {index} starts at {start}, expected {expected}"
            );
        }
    }

    #[test]
    fn a_depth_lands_in_the_slice_its_boundaries_say_it_should() {
        // The property that matters: the mapping the shaders apply — a scale
        // and a bias on a logarithm — must agree with the boundaries above. The
        // two are computed differently, so this is not circular.
        let grid = grid();

        for slice in 0..grid.slices {
            let start = grid.slice_start(slice);
            let end = grid.slice_start(slice + 1);
            // Inside the slice rather than on its edge, where floating point
            // makes either answer defensible.
            let middle = (start + end) * 0.5;

            assert_eq!(
                grid.slice_of(middle),
                slice,
                "depth {middle} should be in slice {slice}"
            );
        }
    }

    #[test]
    fn depths_outside_the_range_clamp_rather_than_escaping_the_grid() {
        // An index past the end would read another cluster's light list, or
        // past the buffer. Both are worse than the last slice being deep.
        let grid = grid();

        assert_eq!(grid.slice_of(0.0), 0);
        assert_eq!(grid.slice_of(grid.near * 0.5), 0);
        assert_eq!(grid.slice_of(grid.far), grid.slices - 1);
        assert_eq!(grid.slice_of(grid.far * 1000.0), grid.slices - 1);
    }

    #[test]
    fn the_slices_tile_the_range_without_a_gap() {
        // Every depth in the range belongs to exactly one slice, which is what
        // makes "the fragment's cluster" a well-defined thing.
        let grid = grid();
        let mut depth = grid.near * 1.01;

        while depth < grid.far {
            let slice = grid.slice_of(depth);

            assert!(depth >= grid.slice_start(slice), "{depth} below its slice");
            assert!(
                depth < grid.slice_start(slice + 1),
                "{depth} above its slice"
            );

            depth *= 1.05;
        }
    }

    #[test]
    fn a_cells_bounds_span_its_slice_with_depth_running_negative() {
        // Right-handed view space: in front of the camera is negative Z, so the
        // *near* edge of a slice is the larger coordinate. Asserted rather than
        // commented, because a sign error here puts every cluster behind the
        // camera and every light list empty — which renders as a scene lit only
        // by the ambient term, not as a failure.
        let grid = grid();

        for slice in 0..grid.slices {
            let (min, max) = grid.bounds((0, 0, slice), 1.0, 1.0);

            assert!(max.z < 0.0, "slice {slice} is not in front of the camera");
            assert!((max.z + grid.slice_start(slice)).abs() < 1e-4);
            assert!((min.z + grid.slice_start(slice + 1)).abs() < 1e-4);
        }
    }

    #[test]
    fn the_top_tile_is_above_the_bottom_one_in_view_space() {
        // The projection flips Y, so tile row zero — the top of the screen — is
        // *positive* Y in view space. Getting this backwards mirrors the light
        // assignment vertically, which looks like lights in the wrong place
        // rather than like an axis convention.
        let grid = grid();

        let (_, top_max) = grid.bounds((0, 0, 1), 1.0, 1.0);
        let (bottom_min, _) = grid.bounds((0, grid.tiles_y - 1, 1), 1.0, 1.0);

        assert!(top_max.y > 0.0, "the top tile should be above the axis");
        assert!(bottom_min.y < 0.0, "the bottom tile should be below it");
    }

    #[test]
    fn the_cells_of_one_slice_cover_it_and_do_not_overlap() {
        // A tile grid with a hole assigns no lights to whatever falls in it, and
        // the symptom is an unlit rectangle rather than a failure.
        let grid = grid();

        let (left_min, left_max) = grid.bounds((0, 0, 0), 1.0, 1.0);
        let (right_min, right_max) = grid.bounds((1, 0, 0), 1.0, 1.0);

        assert!(
            (left_max.x - right_min.x).abs() < 1e-5,
            "a gap between horizontally adjacent tiles"
        );
        assert!(left_min.x < right_max.x);

        let (upper_min, _) = grid.bounds((0, 0, 0), 1.0, 1.0);
        let (_, lower_max) = grid.bounds((0, 1, 0), 1.0, 1.0);

        assert!(
            (upper_min.y - lower_max.y).abs() < 1e-5,
            "a gap between vertically adjacent tiles"
        );
    }

    #[test]
    fn a_wider_aspect_ratio_widens_the_cells_without_moving_them_in_depth() {
        let grid = grid();

        let (narrow_min, narrow_max) = grid.bounds((0, 0, 1), 1.0, 1.0);
        let (wide_min, wide_max) = grid.bounds((0, 0, 1), 1.0, 2.0);

        assert!(wide_max.x - wide_min.x > narrow_max.x - narrow_min.x);
        assert!((wide_min.z - narrow_min.z).abs() < 1e-6);
    }

    #[test]
    fn a_sphere_is_found_by_its_nearest_point_not_its_centre() {
        // The mistake this guards: testing whether the *centre* is in the box
        // misses every light that reaches into a cell from outside it, which is
        // most of them.
        let min = Vec3::new(0.0, 0.0, 0.0);
        let max = Vec3::new(1.0, 1.0, 1.0);

        assert!(
            sphere_touches_box(Vec3::new(2.0, 0.5, 0.5), 1.5, min, max),
            "a sphere reaching into the box from outside must be found"
        );
        assert!(
            !sphere_touches_box(Vec3::new(2.0, 0.5, 0.5), 0.5, min, max),
            "a sphere that stops short of the box must not be"
        );
        assert!(
            sphere_touches_box(Vec3::new(0.5, 0.5, 0.5), 0.01, min, max),
            "a sphere inside the box must be found"
        );
    }

    #[test]
    fn a_sphere_touching_a_corner_diagonally_is_found() {
        // The case a per-axis test gets wrong. The corner is sqrt(3) ≈ 1.732
        // away from the centre of a unit cube's opposite corner, so a radius
        // between 1 and 1.732 clears every individual axis and still misses.
        let min = Vec3::splat(0.0);
        let max = Vec3::splat(1.0);
        let centre = Vec3::new(2.0, 2.0, 2.0);

        // sqrt(3) ≈ 1.732 from the near corner at (1, 1, 1).
        assert!(!sphere_touches_box(centre, 1.7, min, max));
        assert!(sphere_touches_box(centre, 1.8, min, max));
    }

    #[test]
    fn the_default_grid_is_what_the_plan_says() {
        let grid = ClusterGrid::default();

        assert_eq!(grid.tiles_x, 16);
        assert_eq!(grid.tiles_y, 9);
        assert_eq!(grid.slices, 24);
        assert_eq!(grid.count(), 3456);
    }

    #[test]
    fn the_index_buffer_stays_small_enough_not_to_think_about() {
        // The fixed stride wastes memory by design — see `max_per_cluster`. What
        // makes that acceptable is the number being this one rather than a
        // surprise.
        let bytes = u64::from(ClusterGrid::default().index_capacity()) * 4;

        assert!(bytes < 1_000_000, "{bytes} bytes is more than expected");
    }
}
