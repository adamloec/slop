//! Images: tiled GPU memory with a format, and the view that reads it.

use std::sync::Arc;

use ash::vk;
use gpu_allocator::vulkan as ga;

use crate::resource::{Allocator, MemoryLocation};
use crate::{
    Extent2D, Format, ImageAspect, ImageHandle, ImageUsage, ImageViewHandle, RhiError, aspect_of,
};

/// What an image is, and what it is for.
///
/// Optimal tiling always; everything else is a field. Mip chains, array layers,
/// cube maps and depth formats each arrived that way rather than as a parallel
/// constructor — `docs/CONVENTIONS.md` §5.1's rule that configuration is a
/// struct, so adding a knob does not fork a call graph.
#[derive(Debug, Clone)]
pub struct ImageConfig<'a> {
    /// A name for validation messages and allocator reports.
    pub name: &'a str,
    /// Size in pixels.
    pub extent: Extent2D,
    /// Pixel format.
    pub format: Format,
    /// How the image will be used.
    pub usage: ImageUsage,
    /// How many mip levels to allocate, including level zero.
    ///
    /// One means no mips, which is right for a render target or a depth buffer:
    /// nothing samples them at a distance. Sampled textures want the full chain,
    /// because a surface drawn smaller than its texture aliases badly without
    /// one — that shimmer on a distant floor is undersampling, and mips are the
    /// prefiltered answer to it.
    pub mip_levels: u32,
    /// What this image is, and how many layers that implies.
    pub kind: ImageKind,
}

/// How an image's layers are meant to be read.
///
/// **An enum rather than a layer count and a flag**, because a cube is not "six
/// layers plus a bit set" — it is six layers *and* that bit, and the two being
/// separate fields makes "six layers without the flag" and "the flag with four
/// layers" both expressible and both wrong. Vulkan will reject the second and
/// silently accept the first, giving an image that cannot be viewed as a cube for
/// a reason nothing reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// One layer. Render targets, depth buffers, ordinary textures.
    Flat,

    /// Several same-sized images a shader indexes with a third coordinate.
    ///
    /// What `docs/PLAN.md` §9.4's four shadow cascades are — an array rather
    /// than four separate images because the shader picks a cascade at runtime:
    /// one handle and a layer index, instead of four handles and a branch.
    ///
    /// Not a 3D image, which is a different thing: layers do not filter into one
    /// another, and a shadow cascade blended with its neighbour would be
    /// nonsense.
    Array(u32),

    /// Six layers, sampled by direction rather than by coordinate.
    ///
    /// `docs/PLAN.md` §9.7's environment map. The layer order is fixed by the
    /// API — `+X, -X, +Y, -Y, +Z, -Z` — and `slop-cook`'s `cube.rs` writes them
    /// in that order, because the CPU and the hardware sampler disagreeing about
    /// which texel a direction lands on is not a thing that can be debugged from
    /// an image.
    ///
    /// What this buys over [`Array`](ImageKind::Array) is **seamless filtering**:
    /// a sample near a face edge blends with the neighbouring face, in hardware.
    /// That is the whole reason §9.7 chose a cube over an octahedral map, whose
    /// edges would need a hand-maintained border on every level.
    Cube,
}

impl ImageKind {
    /// How many array layers this needs.
    #[must_use]
    pub const fn layers(self) -> u32 {
        match self {
            Self::Flat => 1,
            // Floored at one: zero is meaningless to Vulkan and would be
            // rejected with a message about the image rather than the caller.
            Self::Array(layers) => {
                if layers > 1 {
                    layers
                } else {
                    1
                }
            }
            Self::Cube => 6,
        }
    }

    /// Whether the whole-image view is a cube.
    const fn is_cube(self) -> bool {
        matches!(self, Self::Cube)
    }
}

impl ImageConfig<'_> {
    /// Levels, floored at one.
    ///
    /// Zero is meaningless to Vulkan and would be rejected at creation with a
    /// message about the image rather than about the caller.
    fn levels(&self) -> u32 {
        self.mip_levels.max(1)
    }

    /// Layers, from the kind.
    fn layers(&self) -> u32 {
        self.kind.layers()
    }
}

/// A GPU image, the memory backing it, and a view covering all of it.
///
/// The view is created here rather than separately because every image this
/// engine makes needs at least one, and an image with no view is not usable by
/// anything. Images needing *several* views — a mip chain sampled whole and
/// written per-level — will grow an explicit accessor; that is a real case, and
/// not one M0 has.
pub struct Image {
    // Drop order: the views must be destroyed before the image they read.
    view: vk::ImageView,
    /// One view per layer, for an array image, so a single layer can be an
    /// attachment while the whole array stays samplable.
    ///
    /// **Empty for a single-layer image**, where the whole-image view above is
    /// already exactly a view of layer zero — not approximately, exactly, so
    /// [`layer_view`](Image::layer_view) returning it is not a fallback that
    /// might be wrong.
    layer_views: Vec<vk::ImageView>,
    handle: vk::Image,
    // `Option` so `Drop` can move the allocation back to the allocator. Always
    // `Some` between construction and drop.
    allocation: Option<ga::Allocation>,
    allocator: Arc<Allocator>,
    extent: Extent2D,
    format: Format,
}

impl Image {
    /// Allocate an image and a view of it.
    ///
    /// Always [`MemoryLocation::DeviceOnly`]: an optimally tiled image has a
    /// driver-private memory layout, so mapping one and reading it gives bytes
    /// in no documented order. Getting pixels to the CPU means copying to a
    /// buffer — see [`CommandBuffer::copy_image_to_buffer`].
    ///
    /// [`CommandBuffer::copy_image_to_buffer`]: crate::CommandBuffer::copy_image_to_buffer
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects the image — an unsupported format or usage
    /// combination is the usual cause — or if device-local memory is exhausted.
    pub fn new(allocator: &Arc<Allocator>, config: &ImageConfig<'_>) -> Result<Self, RhiError> {
        check_format_support(allocator.device(), config.format, config.usage)?;

        let device = allocator.device().raw();

        // `CUBE_COMPATIBLE` has to be set at *creation*, not at view time: it
        // tells the driver the six layers may be sampled as one directional
        // image, which can change how they are laid out. Asking for a cube view
        // of an image created without it is a validation error.
        let flags = if config.kind.is_cube() {
            vk::ImageCreateFlags::CUBE_COMPATIBLE
        } else {
            vk::ImageCreateFlags::empty()
        };

        let create_info = vk::ImageCreateInfo::default()
            .flags(flags)
            .image_type(vk::ImageType::TYPE_2D)
            .format(config.format.to_vk())
            .extent(vk::Extent3D {
                width: config.extent.width,
                height: config.extent.height,
                depth: 1,
            })
            .mip_levels(config.levels())
            .array_layers(config.layers())
            .samples(vk::SampleCountFlags::TYPE_1)
            // OPTIMAL, not LINEAR. Linear tiling is mappable and is the reason
            // people reach for it, but support is narrow enough that a format
            // working on one vendor and not another is normal, and sampling
            // from it is slow. The staging copy is the portable path.
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(config.usage.to_vk())
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // Contents start undefined, which is what every barrier in this
            // crate transitions *from* on first use.
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `create_info` is fully initialized, and the device is alive
        // because the allocator holds an `Arc` to it.
        let handle = unsafe { device.create_image(&create_info, None) }?;

        // SAFETY: `handle` was just created from this device.
        let requirements = unsafe { device.get_image_memory_requirements(handle) };

        // Not linear: optimal tiling, so the allocator must keep this off any
        // page shared with a buffer, per Vulkan's buffer-image granularity.
        let allocation = match allocator.allocate(
            config.name,
            requirements,
            MemoryLocation::DeviceOnly,
            false,
        ) {
            Ok(allocation) => allocation,
            Err(error) => {
                // SAFETY: created from this device and never used.
                unsafe { device.destroy_image(handle, None) };
                return Err(error);
            }
        };

        // SAFETY: the allocation satisfies `handle`'s memory requirements, the
        // image has no memory bound yet, and the allocation outlives the image
        // because both are owned by the value returned below.
        let bound =
            unsafe { device.bind_image_memory(handle, allocation.memory(), allocation.offset()) };

        if let Err(error) = bound {
            allocator.free(allocation);
            // SAFETY: created from this device and never used.
            unsafe { device.destroy_image(handle, None) };
            return Err(error.into());
        }

        // The whole image, viewed as whatever the kind says it is. A `TYPE_2D`
        // view of a multi-layer image would see only the first layer, and a
        // `TYPE_2D_ARRAY` view of a cube is samplable but only by coordinate —
        // the directional lookup and the seamless filtering both come from the
        // view type rather than from the image.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(handle)
            .view_type(match config.kind {
                ImageKind::Flat => vk::ImageViewType::TYPE_2D,
                ImageKind::Array(_) => vk::ImageViewType::TYPE_2D_ARRAY,
                ImageKind::Cube => vk::ImageViewType::CUBE,
            })
            .format(config.format.to_vk())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect_of(config.format).to_vk(),
                base_mip_level: 0,
                level_count: config.levels(),
                base_array_layer: 0,
                layer_count: config.layers(),
            });

        // SAFETY: `handle` has memory bound and `view_info` is fully
        // initialized.
        let view = match unsafe { device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                allocator.free(allocation);
                // SAFETY: created from this device and never used.
                unsafe { device.destroy_image(handle, None) };
                return Err(error.into());
            }
        };

        // One `TYPE_2D` view per layer, so a layer can be a render attachment
        // while the array view above stays samplable. Skipped entirely for a
        // single-layer image, where `view` already *is* the view of layer zero.
        let mut layer_views = Vec::new();

        if config.layers() > 1 {
            for layer in 0..config.layers() {
                let layer_info = vk::ImageViewCreateInfo::default()
                    .image(handle)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(config.format.to_vk())
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect_of(config.format).to_vk(),
                        base_mip_level: 0,
                        level_count: config.levels(),
                        base_array_layer: layer,
                        layer_count: 1,
                    });

                // SAFETY: as above; `handle` has memory bound and `layer_info`
                // is fully initialized.
                match unsafe { device.create_image_view(&layer_info, None) } {
                    Ok(created) => layer_views.push(created),
                    Err(error) => {
                        // SAFETY: every view in `layer_views` and `view` was
                        // created from this device and none has been used.
                        unsafe {
                            for created in &layer_views {
                                device.destroy_image_view(*created, None);
                            }
                            device.destroy_image_view(view, None);
                            device.destroy_image(handle, None);
                        }
                        allocator.free(allocation);
                        return Err(error.into());
                    }
                }
            }
        }

        Ok(Self {
            view,
            layer_views,
            handle,
            allocation: Some(allocation),
            allocator: Arc::clone(allocator),
            extent: config.extent,
            format: config.format,
        })
    }

    /// The underlying handle, for barriers and copies.
    pub fn handle(&self) -> ImageHandle {
        ImageHandle(self.handle)
    }

    /// A view covering the whole image, for attachments and descriptors.
    pub fn view(&self) -> ImageViewHandle {
        ImageViewHandle(self.view)
    }

    /// Size in pixels.
    pub fn extent(&self) -> Extent2D {
        self.extent
    }

    /// Pixel format.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Which aspect this image's format carries, for barriers and copies.
    pub fn aspect(&self) -> ImageAspect {
        aspect_of(self.format)
    }

    /// How many array layers this was allocated with.
    pub fn layers(&self) -> u32 {
        // One, or one view per layer. See `layer_views`.
        u32::try_from(self.layer_views.len()).unwrap_or(1).max(1)
    }

    /// A view of one layer, for use as a render attachment.
    ///
    /// [`view`](Image::view) is the whole image and is what a shader samples;
    /// this is a single layer and is what a pass renders into. Rendering into
    /// the array view instead would need a layered pass, which is a different
    /// feature and one nothing here wants: the four cascades have four different
    /// light matrices, so they are four draws either way.
    ///
    /// # Panics
    ///
    /// If `layer` is past the end. `layer_views` is sized at creation, so this
    /// is a programming error rather than a condition.
    pub fn layer_view(&self, layer: u32) -> ImageViewHandle {
        if self.layer_views.is_empty() {
            assert_eq!(layer, 0, "a single-layer image has no layer {layer}");

            return ImageViewHandle(self.view);
        }

        ImageViewHandle(
            self.layer_views[usize::try_from(layer).expect("a layer index fits in a usize")],
        )
    }
}

/// The best depth format this device supports, and whether it carries stencil.
///
/// Preference order is `D32_SFLOAT` first, and that is a `docs/DESIGN.md` §2.7
/// decision rather than a taste one: `slop-math` commits to **reversed** depth,
/// which buys its precision from the floating-point exponent. A 24-bit
/// fixed-point format has uniform spacing and gains nothing from reversal, so
/// pairing reverse-Z with `D24_UNORM_S8_UINT` would pay the complexity and
/// collect none of the benefit.
///
/// Stencil is not requested. Nothing needs it yet, and a combined format costs
/// bandwidth on every depth write. It appears in the fallbacks only because a
/// device offering no pure-depth format leaves no choice.
///
/// # Panics
///
/// Panics if the device supports no depth format at all as a depth attachment.
/// Vulkan requires `D16_UNORM` of every implementation, so this is unreachable
/// on a conformant driver and would mean the device is lying about its formats.
/// Refuse a format the device cannot use the way this image intends to.
///
/// **Checked here rather than left to the driver, and the driver turns out not
/// to check it at all.** Disabling this and asking for a BC7 colour attachment —
/// which the specification forbids outright, for every device — returns a
/// perfectly good `VkImage` on the development machine. `vkCreateImage` is
/// permitted to accept it; the undefined behaviour arrives later, when something
/// renders into it. So this is not a nicer error message in front of a driver
/// rejection that was going to happen anyway. It is the only thing that
/// rejects it.
///
/// Queried per image rather than cached. `vkGetPhysicalDeviceFormatProperties`
/// is widely understood to be a host-side lookup, but that is **not measured
/// here** and this comment does not claim it is. What makes it a safe default is
/// where image creation happens: at startup and on resize, never inside a frame.
/// If that changes — a render graph allocating transients per frame is the
/// obvious way — this wants measuring before it wants caching.
fn check_format_support(
    device: &Arc<crate::Device>,
    format: Format,
    usage: ImageUsage,
) -> Result<(), RhiError> {
    // SAFETY: the physical device came from this instance's enumeration.
    let properties = unsafe {
        device
            .instance()
            .raw()
            .get_physical_device_format_properties(device.physical_device(), format.to_vk())
    };

    // Optimal tiling, matching what `Image::new` creates. Linear tiling has its
    // own, much narrower, feature set and this crate never asks for it.
    let supported = properties.optimal_tiling_features;

    // One usage at a time rather than one combined mask, so the error names the
    // use that is unsupported instead of reporting the whole set and leaving the
    // caller to bisect it.
    const USES: [(ImageUsage, &str); 6] = [
        (ImageUsage::TRANSFER_SRC, "transfer source"),
        (ImageUsage::TRANSFER_DST, "transfer destination"),
        (ImageUsage::SAMPLED, "sampling"),
        (ImageUsage::COLOR_ATTACHMENT, "colour attachment"),
        (
            ImageUsage::DEPTH_STENCIL_ATTACHMENT,
            "depth-stencil attachment",
        ),
        (ImageUsage::STORAGE, "storage image"),
    ];

    for (one, missing) in USES {
        if usage.contains(one) && !supported.contains(one.required_format_features()) {
            return Err(RhiError::FormatUnsupported { format, missing });
        }
    }

    Ok(())
}

pub fn preferred_depth_format(device: &Arc<crate::Device>) -> Format {
    const CANDIDATES: [Format; 4] = [
        Format::D32Float,
        Format::D32FloatS8Uint,
        Format::D24UnormS8Uint,
        // Required of every conformant implementation, so this is the floor.
        Format::D16Unorm,
    ];

    for format in CANDIDATES {
        // SAFETY: the physical device came from this instance's enumeration.
        let properties = unsafe {
            device
                .instance()
                .raw()
                .get_physical_device_format_properties(device.physical_device(), format.to_vk())
        };

        if properties
            .optimal_tiling_features
            .contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT)
        {
            return format;
        }
    }

    unreachable!("Vulkan requires D16_UNORM support on every conformant device")
}

impl Drop for Image {
    fn drop(&mut self) {
        let device = self.allocator.device().raw();

        // SAFETY: both were created from this device and are destroyed exactly
        // once, view before image. The device outlives this because the
        // allocator holds an `Arc` to it. That no GPU work still references
        // them is the caller's obligation.
        unsafe {
            for layer in &self.layer_views {
                device.destroy_image_view(*layer, None);
            }

            device.destroy_image_view(self.view, None);
            device.destroy_image(self.handle, None);
        }

        if let Some(allocation) = self.allocation.take() {
            self.allocator.free(allocation);
        }
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("extent", &self.extent)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}
