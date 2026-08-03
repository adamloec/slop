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
    /// Work submitted to the GPU did not finish in a reasonable time.
    ///
    /// Distinct from a rejected submission: the driver accepted it and then did
    /// not come back, which is a hung or lost device rather than a mistake in
    /// what was asked for.
    #[error("{what} did not complete in time; the device may be hung")]
    Timeout {
        /// What was being waited on.
        what: &'static str,
    },

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

    /// No adapter can run the engine.
    ///
    /// Carries how many were examined so the message distinguishes "no GPU
    /// found at all" from "three were found and all were rejected", which point
    /// at very different problems.
    #[error("no suitable graphics device found among {considered} candidate(s)")]
    NoSuitableDevice {
        /// How many adapters were enumerated.
        considered: usize,
    },

    /// A specifically requested device cannot run the engine.
    #[error("graphics device '{name}' cannot be used: {reason}")]
    DeviceUnsuitable {
        /// The device's reported name.
        name: String,
        /// Why it was rejected, in words a settings UI can show.
        reason: String,
    },

    /// The surface reported no formats at all.
    ///
    /// Distinct from "no *preferred* format": that falls back with a warning.
    /// An empty list means the surface is unusable, which normally indicates the
    /// window was destroyed underneath us.
    #[error("the surface reports no supported formats")]
    NoSurfaceFormats,

    /// The bytes handed to a shader module are not SPIR-V.
    ///
    /// Usually a cooked artifact that was never written, was truncated, or is
    /// actually a text file. Carries what it found so the distinction is
    /// visible.
    #[error("not a SPIR-V module: expected magic 0x07230203, found {found_magic:#010x}")]
    NotSpirv {
        /// The first word actually present.
        found_magic: u32,
    },

    /// SPIR-V byte length is not a multiple of four.
    ///
    /// SPIR-V is a sequence of 32-bit words, so this means a truncated or
    /// corrupt artifact.
    #[error("SPIR-V must be a whole number of 32-bit words, got {length} bytes")]
    SpirvNotWordAligned {
        /// The byte count received.
        length: usize,
    },

    /// The memory allocator could not be constructed.
    ///
    /// Carries the reason as a string rather than the backing allocator's own
    /// error type, so that replacing the allocator is not a breaking change to
    /// this enum.
    #[error("could not create the GPU memory allocator: {reason}")]
    AllocatorUnavailable {
        /// What the allocator reported.
        reason: String,
    },

    /// A suballocation failed.
    ///
    /// Carries the resource name and size because "out of memory" alone is
    /// almost never enough to act on — which resource, and how big, is.
    #[error("could not allocate {size} bytes of GPU memory for '{name}': {reason}")]
    Allocation {
        /// The name the resource was created with.
        name: String,
        /// Bytes requested.
        size: u64,
        /// What the allocator reported.
        reason: String,
    },

    /// Memory was requested from an allocator that has already been destroyed.
    ///
    /// Not reachable through the public API — resources hold an `Arc` to their
    /// allocator — so this indicates a bug in this crate rather than in a
    /// caller.
    #[error("the GPU memory allocator has already been destroyed")]
    AllocatorShutDown,

    /// A mapping was requested for memory the CPU cannot address.
    ///
    /// Means the resource was created in [`MemoryLocation::DeviceOnly`], which
    /// is correct for anything only the GPU touches and wrong for anything being
    /// read back. Getting device-local pixels to the CPU means copying to a
    /// buffer in [`MemoryLocation::Readback`].
    ///
    /// [`MemoryLocation::DeviceOnly`]: crate::MemoryLocation::DeviceOnly
    /// [`MemoryLocation::Readback`]: crate::MemoryLocation::Readback
    #[error("this memory is not host-visible; allocate it for upload or readback to map it")]
    MemoryNotHostVisible,

    /// The device cannot use this format the way the image asked to use it.
    ///
    /// Checked before creation rather than left to the driver, because the
    /// driver's answer is a validation message naming a create-info field. This
    /// one names the format and what it was missing, which is what a caller has
    /// to act on — usually by picking a different format, since a device that
    /// cannot render into `R11G11B10Float` is not going to grow the ability.
    ///
    /// The common cause is a format chosen for one role and reused in another:
    /// an HDR target that renders fine and then has to be sampled, or written by
    /// a compute pass, neither of which the first choice guaranteed.
    #[error("the device cannot use {format:?} for {missing}")]
    FormatUnsupported {
        /// The format that was asked for.
        format: crate::Format,
        /// The feature it lacks, in the engine's vocabulary rather than
        /// Vulkan's — "sampling", "colour attachment".
        missing: &'static str,
    },

    /// A Vulkan call returned a failure code.
    #[error("Vulkan call failed: {0}")]
    Vulkan(#[from] ash::vk::Result),
}
