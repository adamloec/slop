//! Shader modules, loaded from cooked SPIR-V.
//!
//! **This module compiles nothing.** It takes bytes that something else
//! produced — `slop-cli cook` today, `slop-asset` at M2 — which is what makes
//! `docs/DESIGN.md` §2.8's "shipping builds never parse a source asset" true by
//! construction rather than by discipline. There is no code path here that could
//! invoke a compiler even if someone wanted one.
//!
//! One cooked module carries every entry point its source declared. Slang emits
//! a vertex and fragment pair into a single SPIR-V module, so a pipeline names
//! the same module twice with different entry point names rather than juggling
//! two artifacts.

use std::sync::Arc;

use ash::vk;

use crate::{Device, RhiError};

/// The first word of any SPIR-V binary.
///
/// Checking it turns three otherwise baffling failures — a text file, a
/// truncated artifact, a binary built for the opposite endianness — into one
/// clear error, instead of handing the driver something it may or may not
/// survive.
const SPIRV_MAGIC: u32 = 0x0723_0203;

/// A compiled shader, ready to be named by a pipeline.
pub struct ShaderModule {
    handle: vk::ShaderModule,
    device: Arc<Device>,
}

impl ShaderModule {
    /// Create from SPIR-V words.
    ///
    /// # Errors
    ///
    /// Fails if the words are not SPIR-V, or the driver rejects them.
    pub fn from_words(device: &Arc<Device>, words: &[u32]) -> Result<Self, RhiError> {
        match words.first() {
            Some(&SPIRV_MAGIC) => {}
            Some(&found) => {
                return Err(RhiError::NotSpirv { found_magic: found });
            }
            None => return Err(RhiError::NotSpirv { found_magic: 0 }),
        }

        let create_info = vk::ShaderModuleCreateInfo::default().code(words);

        // SAFETY: `create_info` borrows `words`, which outlives the call, and the
        // magic number check above establishes this is at least plausibly SPIR-V.
        // The driver validates the rest, and validation layers report anything it
        // tolerates but should not.
        let handle = unsafe { device.raw().create_shader_module(&create_info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
        })
    }

    /// Create from the raw bytes of a cooked artifact.
    ///
    /// SPIR-V is a sequence of 32-bit words, but a file read gives bytes with no
    /// alignment guarantee, so this copies into an aligned buffer rather than
    /// casting. The copy happens once at load time and never in a frame.
    ///
    /// # Errors
    ///
    /// Fails if the byte count is not a multiple of four, the content is not
    /// SPIR-V, or the driver rejects it.
    pub fn from_bytes(device: &Arc<Device>, bytes: &[u8]) -> Result<Self, RhiError> {
        if !bytes.len().is_multiple_of(4) {
            return Err(RhiError::SpirvNotWordAligned {
                length: bytes.len(),
            });
        }

        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();

        Self::from_words(device, &words)
    }

    /// The underlying handle, for pipeline creation.
    pub fn handle(&self) -> vk::ShaderModule {
        self.handle
    }
}

impl Drop for ShaderModule {
    fn drop(&mut self) {
        // A shader module may be destroyed as soon as the pipelines using it are
        // created — Vulkan does not require it to outlive them — so RAII here
        // costs nothing and prevents a leak.
        //
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_shader_module(self.handle, None) };
    }
}

impl std::fmt::Debug for ShaderModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShaderModule").finish_non_exhaustive()
    }
}
