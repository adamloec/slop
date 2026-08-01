//! The swapchain: the rotating set of images the GPU draws into and the OS
//! presents.
//!
//! Creation involves four independent choices — format, present mode, image
//! count, and extent — each of which has a wrong answer that looks fine on one
//! machine. They are made explicitly here, and each records why.

use std::sync::Arc;

use ash::vk;
use slop_core::diagnostics::tracing::{debug, info, warn};

use crate::{Device, RhiError, Surface};

/// How presentation is paced.
///
/// This is a user-facing graphics setting — a player's "vsync" toggle — so it
/// arrives as a parameter rather than being decided here
/// (`docs/CONVENTIONS.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresentMode {
    /// Wait for vertical blank. No tearing, latency of up to one refresh.
    /// The only mode Vulkan guarantees exists.
    Vsync,
    /// Like [`Vsync`](Self::Vsync), but present immediately if the frame missed
    /// its blank. Trades a tear on a late frame for not stalling to the next
    /// interval.
    VsyncRelaxed,
    /// Present immediately. Tears, lowest latency.
    Immediate,
    /// Replace the queued frame with the newest one. No tearing and low
    /// latency, at the cost of rendering frames that are then discarded.
    /// Falls back to [`Vsync`](Self::Vsync) when unsupported.
    #[default]
    Mailbox,
}

impl PresentMode {
    fn to_vk(self) -> vk::PresentModeKHR {
        match self {
            Self::Vsync => vk::PresentModeKHR::FIFO,
            Self::VsyncRelaxed => vk::PresentModeKHR::FIFO_RELAXED,
            Self::Immediate => vk::PresentModeKHR::IMMEDIATE,
            Self::Mailbox => vk::PresentModeKHR::MAILBOX,
        }
    }
}

/// How to build a [`Swapchain`].
#[derive(Debug, Clone)]
pub struct SwapchainConfig {
    /// Presentation pacing. Falls back to [`PresentMode::Vsync`] if the
    /// requested mode is unsupported, since FIFO is always available.
    pub present_mode: PresentMode,
    /// Size in **physical** pixels.
    ///
    /// Not logical: a window requested at 1280×720 on a display at 150% scaling
    /// is 1920×1080 physical, and sizing a swapchain from the logical figure
    /// produces a blurry image or a validation error. Use the window's
    /// `inner_size()`, which is already physical.
    pub extent: vk::Extent2D,
}

/// The images presented to the display, and their views.
pub struct Swapchain {
    // Drop order: views are created from the swapchain's images, so they go
    // first; the swapchain then goes before the device that owns it.
    views: Vec<vk::ImageView>,
    handle: vk::SwapchainKHR,
    loader: ash::khr::swapchain::Device,
    device: Arc<Device>,

    images: Vec<vk::Image>,
    format: vk::Format,
    color_space: vk::ColorSpaceKHR,
    extent: vk::Extent2D,
    present_mode: vk::PresentModeKHR,
}

impl Swapchain {
    /// Create a swapchain for `surface` on `device`.
    ///
    /// # Errors
    ///
    /// Fails if the surface reports no formats or present modes, or the driver
    /// rejects creation.
    pub fn new(
        device: &Arc<Device>,
        surface: &Surface,
        config: &SwapchainConfig,
    ) -> Result<Self, RhiError> {
        let loader = ash::khr::swapchain::Device::new(device.instance().raw(), device.raw());

        let mut swapchain = Self {
            views: Vec::new(),
            handle: vk::SwapchainKHR::null(),
            loader,
            device: Arc::clone(device),
            images: Vec::new(),
            format: vk::Format::UNDEFINED,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            extent: config.extent,
            present_mode: vk::PresentModeKHR::FIFO,
        };

        swapchain.build(surface, config, vk::SwapchainKHR::null())?;

        Ok(swapchain)
    }

    /// Rebuild for a new size, reusing the old swapchain to keep presenting
    /// during the transition.
    ///
    /// Required after a resize, and after acquire or present reports the
    /// swapchain out of date.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation.
    pub fn recreate(&mut self, surface: &Surface, extent: vk::Extent2D) -> Result<(), RhiError> {
        // Outstanding work may still reference the images being replaced. This
        // is the blunt instrument; a per-frame fence is the eventual answer, but
        // resizing is rare enough that correctness wins here.
        self.device.wait_idle()?;

        let config = SwapchainConfig {
            present_mode: self.requested_present_mode(),
            extent,
        };
        let old = self.handle;

        self.destroy_views();
        self.build(surface, &config, old)?;

        if old != vk::SwapchainKHR::null() {
            // SAFETY: `old` was retired by `build`, which passed it as
            // `oldSwapchain`; the driver no longer presents from it, and
            // `wait_idle` above ensured no work references it.
            unsafe { self.loader.destroy_swapchain(old, None) };
        }

        Ok(())
    }

    fn build(
        &mut self,
        surface: &Surface,
        config: &SwapchainConfig,
        old: vk::SwapchainKHR,
    ) -> Result<(), RhiError> {
        let physical = self.device.physical_device();
        let capabilities = surface.capabilities(physical)?;

        let (format, color_space) = select_format(&surface.formats(physical)?)?;
        let present_mode =
            select_present_mode(&surface.present_modes(physical)?, config.present_mode);
        let extent = select_extent(&capabilities, config.extent);
        let image_count = select_image_count(&capabilities);

        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface.handle())
            .min_image_count(image_count)
            .image_format(format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            // EXCLUSIVE is correct even when graphics and present are different
            // families: sharing images across families costs bandwidth, and the
            // right answer is an explicit ownership transfer barrier, not
            // CONCURRENT. On this hardware the families coincide anyway.
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            // Permits the driver to discard pixels obscured by another window.
            .clipped(true)
            .old_swapchain(old);

        // SAFETY: every borrowed field outlives the call, and `surface` belongs
        // to the same instance as the device.
        let handle = unsafe { self.loader.create_swapchain(&create_info, None) }?;

        // SAFETY: `handle` was just created by this loader.
        let images = unsafe { self.loader.get_swapchain_images(handle) }?;

        self.handle = handle;
        self.images = images;
        self.format = format;
        self.color_space = color_space;
        self.extent = extent;
        self.present_mode = present_mode;
        self.create_views()?;

        info!(
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            format = ?format,
            present_mode = ?present_mode,
            "created swapchain"
        );

        Ok(())
    }

    fn create_views(&mut self) -> Result<(), RhiError> {
        self.views.reserve(self.images.len());

        for &image in &self.images {
            let create_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(self.format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });

            // SAFETY: `image` came from this swapchain and `format` is the one
            // it was created with.
            let view = unsafe { self.device.raw().create_image_view(&create_info, None) }?;

            self.views.push(view);
        }

        Ok(())
    }

    fn destroy_views(&mut self) {
        for view in self.views.drain(..) {
            // SAFETY: each view was created by `create_views` from this device
            // and is destroyed exactly once.
            unsafe { self.device.raw().destroy_image_view(view, None) };
        }
    }

    /// Map the active Vulkan present mode back to our own enum, so
    /// [`recreate`](Self::recreate) preserves the caller's request rather than
    /// silently re-selecting.
    fn requested_present_mode(&self) -> PresentMode {
        match self.present_mode {
            vk::PresentModeKHR::MAILBOX => PresentMode::Mailbox,
            vk::PresentModeKHR::IMMEDIATE => PresentMode::Immediate,
            vk::PresentModeKHR::FIFO_RELAXED => PresentMode::VsyncRelaxed,
            _ => PresentMode::Vsync,
        }
    }

    /// The underlying handle.
    pub fn handle(&self) -> vk::SwapchainKHR {
        self.handle
    }

    /// The extension loader, for acquire and present.
    pub fn loader(&self) -> &ash::khr::swapchain::Device {
        &self.loader
    }

    /// The presentable images.
    pub fn images(&self) -> &[vk::Image] {
        &self.images
    }

    /// One view per image, in the same order.
    pub fn views(&self) -> &[vk::ImageView] {
        &self.views
    }

    /// The image format, which render passes must match.
    pub fn format(&self) -> vk::Format {
        self.format
    }

    /// The color space the presented images are interpreted in.
    pub fn color_space(&self) -> vk::ColorSpaceKHR {
        self.color_space
    }

    /// Size in physical pixels.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// The present mode actually in use, which may differ from the request.
    pub fn present_mode(&self) -> vk::PresentModeKHR {
        self.present_mode
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // Presented images may still be in flight.
        let _ = self.device.wait_idle();

        self.destroy_views();

        // SAFETY: created by this loader, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.loader.destroy_swapchain(self.handle, None) };
    }
}

impl std::fmt::Debug for Swapchain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Swapchain")
            .field("extent", &self.extent)
            .field("images", &self.images.len())
            .field("format", &self.format)
            .field("present_mode", &self.present_mode)
            .finish()
    }
}

/// Prefer an sRGB format, so the display hardware performs the final transfer
/// function rather than a shader approximating it.
///
/// `docs/DESIGN.md` §4.2 stage A renders in HDR and tonemaps; the swapchain is
/// the low-dynamic-range destination, and getting its color space wrong makes
/// everything uniformly too dark or too bright in a way that is easy to
/// misattribute to the lighting.
fn select_format(
    available: &[vk::SurfaceFormatKHR],
) -> Result<(vk::Format, vk::ColorSpaceKHR), RhiError> {
    const PREFERRED: [vk::Format; 2] = [vk::Format::B8G8R8A8_SRGB, vk::Format::R8G8B8A8_SRGB];

    for &format in &PREFERRED {
        if let Some(found) = available.iter().find(|candidate| {
            candidate.format == format && candidate.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        }) {
            return Ok((found.format, found.color_space));
        }
    }

    let fallback = available.first().ok_or(RhiError::NoSurfaceFormats)?;

    warn!(
        format = ?fallback.format,
        "no sRGB surface format available; colors will be incorrect unless \
         the shader compensates"
    );

    Ok((fallback.format, fallback.color_space))
}

/// Use the requested mode when supported, falling back to FIFO.
///
/// FIFO is the only mode Vulkan guarantees, so it is always a valid answer.
fn select_present_mode(
    available: &[vk::PresentModeKHR],
    requested: PresentMode,
) -> vk::PresentModeKHR {
    let wanted = requested.to_vk();

    if available.contains(&wanted) {
        return wanted;
    }

    debug!(
        requested = ?wanted,
        "present mode unsupported; falling back to FIFO"
    );

    vk::PresentModeKHR::FIFO
}

/// Resolve the swapchain extent.
///
/// When `current_extent` is `u32::MAX` the surface is telling us to choose —
/// Wayland does this, Windows does not. Ignoring that case produces a swapchain
/// sized `0xFFFFFFFF` on Linux and works fine on Windows, which is exactly the
/// class of breakage `docs/DESIGN.md` §2.13 exists to catch.
fn select_extent(
    capabilities: &vk::SurfaceCapabilitiesKHR,
    requested: vk::Extent2D,
) -> vk::Extent2D {
    if capabilities.current_extent.width != u32::MAX {
        return capabilities.current_extent;
    }

    vk::Extent2D {
        width: requested.width.clamp(
            capabilities.min_image_extent.width,
            capabilities.max_image_extent.width,
        ),
        height: requested.height.clamp(
            capabilities.min_image_extent.height,
            capabilities.max_image_extent.height,
        ),
    }
}

/// One more than the minimum, so the CPU is not forced to wait for the driver
/// to release an image before preparing the next frame.
///
/// `max_image_count` of zero means unlimited, which is why it cannot be clamped
/// against naively.
fn select_image_count(capabilities: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let desired = capabilities.min_image_count + 1;

    if capabilities.max_image_count > 0 {
        desired.min(capabilities.max_image_count)
    } else {
        desired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(min: u32, max: u32) -> vk::SurfaceCapabilitiesKHR {
        vk::SurfaceCapabilitiesKHR {
            min_image_count: min,
            max_image_count: max,
            current_extent: vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            min_image_extent: vk::Extent2D {
                width: 1,
                height: 1,
            },
            max_image_extent: vk::Extent2D {
                width: 4096,
                height: 4096,
            },
            ..Default::default()
        }
    }

    fn format(format: vk::Format, space: vk::ColorSpaceKHR) -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format,
            color_space: space,
        }
    }

    #[test]
    fn prefers_bgra_srgb() {
        let available = [
            format(
                vk::Format::R8G8B8A8_UNORM,
                vk::ColorSpaceKHR::SRGB_NONLINEAR,
            ),
            format(vk::Format::B8G8R8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        ];

        let (chosen, space) = select_format(&available).expect("formats exist");

        assert_eq!(chosen, vk::Format::B8G8R8A8_SRGB);
        assert_eq!(space, vk::ColorSpaceKHR::SRGB_NONLINEAR);
    }

    #[test]
    fn accepts_rgba_srgb_when_bgra_is_absent() {
        let available = [
            format(
                vk::Format::R8G8B8A8_UNORM,
                vk::ColorSpaceKHR::SRGB_NONLINEAR,
            ),
            format(vk::Format::R8G8B8A8_SRGB, vk::ColorSpaceKHR::SRGB_NONLINEAR),
        ];

        let (chosen, _) = select_format(&available).expect("formats exist");

        assert_eq!(chosen, vk::Format::R8G8B8A8_SRGB);
    }

    #[test]
    fn falls_back_to_the_first_format_when_no_srgb_exists() {
        let available = [format(
            vk::Format::R8G8B8A8_UNORM,
            vk::ColorSpaceKHR::SRGB_NONLINEAR,
        )];

        let (chosen, _) = select_format(&available).expect("formats exist");

        assert_eq!(chosen, vk::Format::R8G8B8A8_UNORM);
    }

    #[test]
    fn no_formats_at_all_is_an_error() {
        assert!(matches!(
            select_format(&[]),
            Err(RhiError::NoSurfaceFormats)
        ));
    }

    #[test]
    fn uses_the_requested_present_mode_when_supported() {
        let available = [vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX];

        assert_eq!(
            select_present_mode(&available, PresentMode::Mailbox),
            vk::PresentModeKHR::MAILBOX
        );
    }

    #[test]
    fn falls_back_to_fifo_when_the_mode_is_unsupported() {
        // FIFO is the only mode Vulkan guarantees, so it is always safe.
        let available = [vk::PresentModeKHR::FIFO];

        assert_eq!(
            select_present_mode(&available, PresentMode::Mailbox),
            vk::PresentModeKHR::FIFO
        );
    }

    #[test]
    fn uses_the_surface_extent_when_it_is_fixed() {
        // Windows reports a concrete extent, and it wins over any request:
        // the surface, not the caller, is authoritative.
        let extent = select_extent(
            &capabilities(2, 8),
            vk::Extent2D {
                width: 800,
                height: 600,
            },
        );

        assert_eq!(extent.width, 1920);
        assert_eq!(extent.height, 1080);
    }

    #[test]
    fn uses_the_request_when_the_surface_defers_the_choice() {
        // Wayland reports u32::MAX, meaning "you decide". Treating that as a
        // real extent produces a 4-billion-pixel swapchain on Linux while
        // working perfectly on Windows.
        let mut caps = capabilities(2, 8);
        caps.current_extent = vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        };

        let extent = select_extent(
            &caps,
            vk::Extent2D {
                width: 800,
                height: 600,
            },
        );

        assert_eq!(extent.width, 800);
        assert_eq!(extent.height, 600);
    }

    #[test]
    fn a_deferred_extent_is_clamped_to_what_the_surface_allows() {
        let mut caps = capabilities(2, 8);
        caps.current_extent = vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        };

        let extent = select_extent(
            &caps,
            vk::Extent2D {
                width: 99_999,
                height: 0,
            },
        );

        assert_eq!(extent.width, 4096, "clamped to max_image_extent");
        assert_eq!(extent.height, 1, "clamped to min_image_extent");
    }

    #[test]
    fn requests_one_more_image_than_the_minimum() {
        assert_eq!(select_image_count(&capabilities(2, 8)), 3);
    }

    #[test]
    fn image_count_is_clamped_to_the_maximum() {
        assert_eq!(select_image_count(&capabilities(3, 3)), 3);
    }

    #[test]
    fn a_maximum_of_zero_means_unlimited() {
        // Zero is not a limit of zero; it means the driver imposes none.
        assert_eq!(select_image_count(&capabilities(4, 0)), 5);
    }
}
