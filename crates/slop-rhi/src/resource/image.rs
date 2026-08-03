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
/// One mip, one array layer, colour aspect, optimal tiling. Mip chains, cube
/// maps and depth formats are all coming, and each will arrive as a field here
/// rather than as a parallel constructor — `docs/CONVENTIONS.md` §5.1's rule
/// that configuration is a struct, so adding a knob does not fork a call graph.
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
}

impl ImageConfig<'_> {
    /// Levels, floored at one.
    ///
    /// Zero is meaningless to Vulkan and would be rejected at creation with a
    /// message about the image rather than about the caller.
    fn levels(&self) -> u32 {
        self.mip_levels.max(1)
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
    // Drop order: the view must be destroyed before the image it reads.
    view: vk::ImageView,
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

        let create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(config.format.to_vk())
            .extent(vk::Extent3D {
                width: config.extent.width,
                height: config.extent.height,
                depth: 1,
            })
            .mip_levels(config.levels())
            .array_layers(1)
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

        let view_info = vk::ImageViewCreateInfo::default()
            .image(handle)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(config.format.to_vk())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect_of(config.format).to_vk(),
                base_mip_level: 0,
                level_count: config.levels(),
                base_array_layer: 0,
                layer_count: 1,
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

        Ok(Self {
            view,
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
