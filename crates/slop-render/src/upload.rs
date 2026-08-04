//! A batch of transfers, recorded into one command buffer and submitted once.
//!
//! What every "get this onto the GPU" path here shares: a mesh's vertices, a
//! material's textures, an environment's cube faces. Extracted from
//! [`mesh`](crate::mesh) when the environment upload became its second caller —
//! the alternative was a second copy of a type whose whole purpose is not doing
//! the same work twice.
//!
//! # What this shape replaces
//!
//! Submitting and blocking **per resource**: a queue submit and a full
//! `wait_idle` for each vertex buffer, each index buffer and each texture, with
//! a staging allocation created and freed around every one. Sponza is 103
//! primitives and 25 materials, so that was several hundred round trips to move
//! data that could travel together.
//!
//! Staging buffers live in the batch rather than being freed by the call that
//! made them, because the copies reading them have not run yet. That is the whole
//! reason the per-resource version had to block: it had nowhere to keep them.
//!
//! # Still one blocking submit at the end
//!
//! `docs/PLAN.md` §6.1 records the real answer — an async transfer queue with a
//! staging ring — and this is not it. It is the same blunt instrument, used once
//! instead of hundreds of times.

use std::sync::Arc;

use slop_rhi::{
    Allocator, Buffer, BufferConfig, BufferUsage, CommandBuffer, CommandPool, Device,
    MemoryLocation,
};

use crate::RenderError;

/// An open batch of transfers.
pub(crate) struct Uploads {
    pub(crate) command: CommandBuffer,
    staging: Vec<Buffer>,
    /// Declared after `command`, since the pool must outlive the buffer it
    /// allocated.
    _pool: CommandPool,
}

impl Uploads {
    /// Open a batch and begin recording.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the pool or the command buffer cannot be created.
    pub(crate) fn new(device: &Arc<Device>) -> Result<Self, RenderError> {
        let pool = CommandPool::new(device, device.queue_families().graphics)?;
        let command = pool
            .allocate(1)?
            .pop()
            .expect("one command buffer was requested");

        command.begin()?;

        Ok(Self {
            command,
            staging: Vec::new(),
            _pool: pool,
        })
    }

    /// Copy `bytes` into a fresh host-visible buffer and keep it alive.
    ///
    /// `name` reaches validation messages and allocator reports, which is the
    /// only way to tell one of several hundred staging buffers from another when
    /// one of them is the problem.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the buffer cannot be allocated or mapped.
    pub(crate) fn stage(
        &mut self,
        allocator: &Arc<Allocator>,
        name: &str,
        bytes: &[u8],
    ) -> Result<&Buffer, RenderError> {
        let mut staging = Buffer::new(
            allocator,
            &BufferConfig {
                name,
                size: bytes.len() as u64,
                usage: BufferUsage::TRANSFER_SRC,
                location: MemoryLocation::Upload,
            },
        )?;

        staging.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);
        self.staging.push(staging);

        Ok(self
            .staging
            .last()
            .expect("a staging buffer was just pushed"))
    }

    /// Submit everything recorded and block until the GPU has it.
    ///
    /// Consumes the batch, so the staging buffers are freed after the wait and
    /// not before it.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if recording cannot end or the submit fails.
    pub(crate) fn finish(self, device: &Arc<Device>) -> Result<(), RenderError> {
        self.command.end()?;
        slop_rhi::submit_recorded_and_wait(device, &self.command)?;

        Ok(())
    }
}

impl std::fmt::Debug for Uploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uploads")
            .field("staging", &self.staging.len())
            .finish_non_exhaustive()
    }
}
