//! Device bring-up: window, surface, device, allocator, in the one order Vulkan
//! permits.
//!
//! Every windowed application performs the same six steps before it can draw:
//! create a window, ask it which instance extensions presenting to its display
//! needs, create the instance with those, create a surface from the window,
//! enumerate and select a device *with the surface in hand*, then create the
//! device and its allocator. The order is not stylistic — each step consumes
//! something the previous one produced, and getting it wrong fails late and
//! obscurely (a device chosen without a surface can turn out not to present).
//!
//! Doing that in each application is how the sequence rots into four subtly
//! different versions. `docs/DESIGN.md` §4 puts bring-up in the application
//! layer, and this is it.
//!
//! # Why this owns the window
//!
//! [`window::create_surface`] is `unsafe` for one reason: the window must outlive
//! the surface, and a free function cannot enforce that because the caller owns
//! both. A struct that owns both *can* — the fields below are declared in drop
//! order, so the surface is destroyed before the window it came from. Discharging
//! that obligation once here is what leaves applications with no `unsafe` of
//! their own.

use std::sync::Arc;

use slop_rhi::{
    Allocator, Device, DeviceInfo, DeviceSelection, Instance, InstanceConfig, RhiError, Surface, vk,
};
use thiserror::Error;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use crate::window::{self, WindowConfig, WindowError};

/// Failures bringing up a window and a device.
#[derive(Debug, Error)]
pub enum GpuError {
    /// The window or its surface could not be created.
    #[error(transparent)]
    Window(#[from] WindowError),

    /// Enumeration, selection, device creation, or the allocator failed.
    #[error(transparent)]
    Rhi(#[from] RhiError),
}

/// How to bring up the GPU.
#[derive(Debug, Clone, Default)]
pub struct GpuConfig {
    /// The window to present to.
    pub window: WindowConfig,
    /// Reported to the driver, and to tools like RenderDoc and Nsight. Worth
    /// setting per application: it is how a capture is identified later.
    pub application_name: String,
    /// Which physical device to use. [`DeviceSelection::Automatic`] scores the
    /// candidates; an explicit choice is what a `--gpu` flag turns into.
    pub selection: DeviceSelection,
}

/// A window and the Vulkan objects built from it.
///
/// Held whole rather than destructured, because the drop order of these four is
/// load-bearing and only the type can guarantee it.
pub struct Gpu {
    // Declared in drop order: allocations, then the device that owns them, then
    // the surface, then the window the surface was made from.
    allocator: Arc<Allocator>,
    device: Arc<Device>,
    surface: Surface,
    window: Window,

    /// Every candidate seen, including rejected ones — kept because "which GPU
    /// am I on, and what else was there" is a question worth answering from a
    /// log or a debug overlay rather than by re-enumerating.
    adapters: Vec<DeviceInfo>,
    chosen: usize,
}

impl Gpu {
    /// Create a window and bring up a device that can present to it.
    ///
    /// Must be called from winit's `resumed` callback — see [`window::create`].
    ///
    /// # Errors
    ///
    /// Fails if the window cannot be created, if no enumerated device is usable,
    /// or if device or allocator creation fails.
    pub fn new(event_loop: &ActiveEventLoop, config: &GpuConfig) -> Result<Self, GpuError> {
        let window = window::create(event_loop, &config.window)?;

        // The window has to exist first: which surface extension the instance
        // needs depends on the display it will present to.
        let instance = Arc::new(Instance::new(&InstanceConfig {
            application_name: config.application_name.clone(),
            required_extensions: window::required_instance_extensions(&window)?,
            ..Default::default()
        })?);

        // SAFETY: `window` is moved into the returned `Gpu` below, where it is
        // declared after the surface and so outlives it.
        let surface = unsafe { window::create_surface(&instance, &window) }?;

        // Enumerating *with* the surface is what makes present support part of
        // usability, rather than something discovered later at swapchain
        // creation.
        let adapters = slop_rhi::enumerate(&instance, Some(&surface))?;
        let chosen = slop_rhi::select(&adapters, &config.selection)?;

        let device = Arc::new(Device::new(&instance, &adapters[chosen])?);
        let allocator = Allocator::new(&device)?;

        Ok(Self {
            allocator,
            device,
            surface,
            window,
            adapters,
            chosen,
        })
    }

    /// The window. Applications need this to request redraws and read its size.
    #[must_use]
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// The surface, for swapchain creation and recreation.
    #[must_use]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// The device.
    #[must_use]
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }

    /// The allocator backing every buffer and image on this device.
    #[must_use]
    pub fn allocator(&self) -> &Arc<Allocator> {
        &self.allocator
    }

    /// Every physical device enumeration reported, usable or not.
    #[must_use]
    pub fn adapters(&self) -> &[DeviceInfo] {
        &self.adapters
    }

    /// Which of [`adapters`](Self::adapters) is in use.
    #[must_use]
    pub fn chosen(&self) -> usize {
        self.chosen
    }

    /// The window's current size, in the form Vulkan wants.
    ///
    /// Physical pixels, not logical: the swapchain is sized in pixels, and using
    /// logical ones produces an image that is subtly the wrong size on a scaled
    /// display — the same units mistake that clipped the debug overlay.
    pub fn extent(&self) -> vk::Extent2D {
        let size = self.window.inner_size();

        vk::Extent2D {
            width: size.width,
            height: size.height,
        }
    }

    /// Wait for the device to finish everything in flight.
    ///
    /// Call from the application's `Drop`, before any field holding a pipeline
    /// or a buffer is destroyed. [`Device`]'s own `Drop` waits as well, but by
    /// then the application's fields are already gone — and destroying a
    /// pipeline a pending submission still references is undefined.
    ///
    /// # Errors
    ///
    /// Fails if the device is lost, which is unrecoverable.
    pub fn wait_idle(&self) -> Result<(), RhiError> {
        self.device.wait_idle()
    }
}

/// Hand-written because the Vulkan objects inside are opaque handles whose
/// derived form would be pages of noise. What is worth seeing in a log line is
/// which GPU this is and how big its window is.
impl std::fmt::Debug for Gpu {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let extent = self.extent();

        formatter
            .debug_struct("Gpu")
            .field("adapter", &self.adapters[self.chosen].name)
            .field("width", &extent.width)
            .field("height", &extent.height)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_names_nothing_and_that_is_visible() {
        // Empty rather than "slop": an application that forgets to name itself
        // should be obvious in a capture, not silently indistinguishable from
        // every other one.
        assert!(GpuConfig::default().application_name.is_empty());
    }

    #[test]
    fn default_config_carries_a_usable_window() {
        let config = GpuConfig::default();

        assert!(config.window.width > 0 && config.window.height > 0);
    }
}
