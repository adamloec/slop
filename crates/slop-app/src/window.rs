//! Window creation, and the seam where `winit` meets the RHI.
//!
//! This is the only place in the engine that knows both a windowing library and
//! Vulkan exist. `slop-rhi` takes raw handles and has no opinion about `winit`;
//! `winit` knows nothing about Vulkan. Joining them is an application-layer
//! concern, which is what keeps the RHI usable headless and leaves room for an
//! embedder that already owns its windows.
//!
//! # What this deliberately does not do
//!
//! It does not own an event loop or a main loop. `docs/DESIGN.md` §1.2 principle
//! 4 says the game owns `main()`, so the caller implements winit's
//! [`ApplicationHandler`](winit::application::ApplicationHandler) and drives the
//! loop itself. Wrapping that here would make the engine a framework, and the
//! loop's eventual shape depends on the renderer, which does not exist yet —
//! the same reasoning that keeps the M0 RHI thin (`docs/PLAN.md` §4.1-D).

use std::sync::Arc;

use slop_rhi::{Instance, RhiError, Surface};
use thiserror::Error;
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

/// Failures creating a window or its surface.
#[derive(Debug, Error)]
pub enum WindowError {
    /// The OS refused to create the window.
    #[error("the operating system could not create a window")]
    Os(#[from] winit::error::OsError),

    /// The window could not supply raw handles.
    ///
    /// In practice this means the window is already being destroyed.
    #[error("the window could not provide a display or window handle")]
    Handle(#[from] HandleError),

    /// Vulkan surface creation failed.
    #[error(transparent)]
    Rhi(#[from] RhiError),
}

/// How to create a window.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Title bar text.
    pub title: String,
    /// Initial width in logical pixels — the OS scales for display DPI.
    pub width: u32,
    /// Initial height in logical pixels.
    pub height: u32,
    /// Whether the user may resize. Resizing forces swapchain recreation, so
    /// this is worth being able to turn off while debugging.
    pub resizable: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: String::from("slop"),
            width: 1280,
            height: 720,
            resizable: true,
        }
    }
}

/// Create a window.
///
/// Must be called from winit's `resumed` callback, which is where an
/// [`ActiveEventLoop`] exists. That is a winit 0.30 requirement, not ours: on
/// some platforms a window cannot legally be created before the application is
/// resumed.
///
/// # Errors
///
/// Fails if the operating system refuses.
pub fn create(event_loop: &ActiveEventLoop, config: &WindowConfig) -> Result<Window, WindowError> {
    let attributes = Window::default_attributes()
        .with_title(&config.title)
        .with_inner_size(LogicalSize::new(config.width, config.height))
        .with_resizable(config.resizable);

    Ok(event_loop.create_window(attributes)?)
}

/// Instance extensions needed to present to this window's display.
///
/// Must be fed into [`InstanceConfig`](slop_rhi::InstanceConfig) *before* the
/// instance is created, which means a window has to exist first. That ordering —
/// window, then instance, then surface — is a Vulkan constraint, not an
/// arbitrary one.
///
/// # Errors
///
/// Fails if the window cannot supply a display handle, or the platform's surface
/// extension is unavailable.
pub fn required_instance_extensions(
    window: &Window,
) -> Result<Vec<std::ffi::CString>, WindowError> {
    let display = window.display_handle()?.as_raw();

    Ok(slop_rhi::required_surface_extensions(display)?)
}

/// Create a Vulkan surface for a window.
///
/// # Safety
///
/// `window` must outlive the returned [`Surface`]. Vulkan cannot detect a
/// surface outliving its window, and this signature cannot express the
/// relationship because the window is owned by the caller — keep them together,
/// and drop the surface first.
///
/// # Errors
///
/// Fails if the window cannot supply handles, or the instance was created
/// without the extensions [`required_instance_extensions`] reports.
pub unsafe fn create_surface(
    instance: &Arc<Instance>,
    window: &Window,
) -> Result<Surface, WindowError> {
    let display = window.display_handle()?.as_raw();
    let handle = window.window_handle()?.as_raw();

    // SAFETY: forwarding this function's own contract — the caller guarantees
    // `window` outlives the surface.
    Ok(unsafe { Surface::new(instance, display, handle) }?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_a_sane_window() {
        let config = WindowConfig::default();

        assert!(config.width > 0 && config.height > 0);
        assert!(!config.title.is_empty());
    }
}
