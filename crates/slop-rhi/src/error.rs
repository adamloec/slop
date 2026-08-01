//! Failures the RHI can report.
//!
//! Typed variants rather than a string, so callers can distinguish "no Vulkan
//! driver on this machine" — which an application may want to report as a
//! friendly message — from "the driver rejected our create info", which is a
//! bug in us.

use thiserror::Error;

/// Anything the render hardware interface can fail at.
#[derive(Debug, Error)]
pub enum RhiError {
    /// The Vulkan loader itself could not be found or loaded.
    ///
    /// Almost always a missing or broken driver installation rather than
    /// anything the engine did.
    #[error("could not load the Vulkan loader; is a GPU driver installed?")]
    LoaderUnavailable(#[source] ash::LoadingError),

    /// The loader reports an API version below what the engine requires.
    #[error(
        "Vulkan {required_major}.{required_minor} is required, but the loader reports \
         {found_major}.{found_minor}; update the GPU driver"
    )]
    ApiVersionTooOld {
        /// Major version the engine requires.
        required_major: u32,
        /// Minor version the engine requires.
        required_minor: u32,
        /// Major version the loader reports.
        found_major: u32,
        /// Minor version the loader reports.
        found_minor: u32,
    },

    /// A required instance extension is not offered by the driver.
    #[error("required Vulkan instance extension is unavailable: {0}")]
    MissingInstanceExtension(String),

    /// Validation was explicitly requested but the layer is not installed.
    ///
    /// Deliberately an error rather than a silent downgrade: a developer who
    /// asked for validation and did not get it would otherwise debug undefined
    /// behaviour with the one tool that reports it switched off.
    #[error(
        "validation layers were requested but VK_LAYER_KHRONOS_validation is not installed; \
         install the Vulkan SDK or construct with validation disabled"
    )]
    ValidationUnavailable,

    /// A Vulkan call returned a failure code.
    #[error("Vulkan call failed: {0}")]
    Vulkan(#[from] ash::vk::Result),
}
