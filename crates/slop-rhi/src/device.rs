//! The logical device and its queues.
//!
//! A [`Device`] is the handle everything else in the RHI is created from. It
//! holds an [`Arc<Instance>`] because Vulkan requires the instance to outlive
//! every device made from it, and encoding that in the type system is the only
//! way to make the ordering impossible to get wrong.
//!
//! That `Arc` is not a violation of `docs/DESIGN.md` §2.6's handles-not-pointers
//! rule. It is not modelling a graph of engine data; it is expressing an FFI
//! resource lifetime that Rust cannot otherwise see, and the instance is
//! immutable once created.

use std::sync::Arc;

use ash::vk;
use slop_core::diagnostics::tracing::info;

use crate::{DeviceInfo, Instance, QueueFamilies, RhiError, features};

/// The queues the engine submits through.
///
/// Handles may be equal when families coincide, which is normal on integrated
/// hardware. Submitting to the same underlying queue from two of these is
/// correct but serializes, so `has_async_*` on [`QueueFamilies`] is what tells
/// the scheduler whether overlap is real.
#[derive(Debug, Clone, Copy)]
pub struct Queues {
    /// Rendering work.
    pub graphics: vk::Queue,
    /// Compute work, ideally overlapping graphics.
    pub compute: vk::Queue,
    /// Uploads and downloads, ideally the DMA engine.
    pub transfer: vk::Queue,
    /// Presentation. `None` in headless mode.
    pub present: Option<vk::Queue>,
}

/// A logical device: the connection to one physical adapter.
pub struct Device {
    // Drop order: `raw` must be destroyed before the instance is released, so
    // it is declared first. See the module docs.
    raw: ash::Device,
    instance: Arc<Instance>,
    physical: vk::PhysicalDevice,
    families: QueueFamilies,
    queues: Queues,
}

impl Device {
    /// Create a logical device for the adapter described by `info`.
    ///
    /// # Errors
    ///
    /// Fails if `info` describes an unusable adapter, or if device creation is
    /// rejected by the driver.
    pub fn new(instance: &Arc<Instance>, info: &DeviceInfo) -> Result<Self, RhiError> {
        let families = info
            .queue_families()
            .ok_or_else(|| RhiError::DeviceUnsuitable {
                name: info.name.clone(),
                reason: info
                    .rejection
                    .as_ref()
                    .map_or_else(|| String::from("no queue families"), ToString::to_string),
            })?;

        // One queue per distinct family, all at equal priority. Priorities are a
        // hint drivers largely ignore, and pretending otherwise would be
        // encoding a scheduling policy we cannot actually enforce.
        let priorities = [1.0_f32];
        let queue_infos: Vec<vk::DeviceQueueCreateInfo<'_>> = families
            .distinct()
            .into_iter()
            .map(|family| {
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities)
            })
            .collect();

        let (core, mut vulkan_12, mut vulkan_13) = features::required();

        // The swapchain extension is enabled exactly when there is something to
        // present to, and never otherwise.
        //
        // `VK_KHR_swapchain` depends on the instance-level `VK_KHR_surface`, so
        // requesting it on a headless instance is a spec violation — one that
        // permissive drivers accept and stricter ones reject, which is the worst
        // kind. A present family exists if and only if a surface was supplied
        // during enumeration, so it is the exact condition.
        let extensions: Vec<*const i8> = if families.present.is_some() {
            vec![ash::khr::swapchain::NAME.as_ptr()]
        } else {
            Vec::new()
        };

        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .features(core)
            .push_next(&mut vulkan_12)
            .push_next(&mut vulkan_13);

        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&extensions)
            .push_next(&mut features2);

        // SAFETY: `info.handle()` came from this instance's enumeration, and
        // every borrowed structure — queue infos, priorities, extension names,
        // and the feature chain — outlives this call. The requested features
        // were verified supported during selection.
        let raw = unsafe {
            instance
                .raw()
                .create_device(info.handle(), &create_info, None)
        }?;

        // SAFETY: each family index came from `distinct()`, which sourced them
        // from this device's own family enumeration, and queue 0 exists in every
        // family we requested.
        let queues = unsafe {
            Queues {
                graphics: raw.get_device_queue(families.graphics, 0),
                compute: raw.get_device_queue(families.compute, 0),
                transfer: raw.get_device_queue(families.transfer, 0),
                present: families
                    .present
                    .map(|family| raw.get_device_queue(family, 0)),
            }
        };

        info!(
            device = %info.name,
            graphics_family = families.graphics,
            compute_family = families.compute,
            transfer_family = families.transfer,
            present_family = ?families.present,
            "created logical device"
        );

        Ok(Self {
            raw,
            instance: Arc::clone(instance),
            physical: info.handle(),
            families,
            queues,
        })
    }

    /// The underlying `ash` device.
    pub fn raw(&self) -> &ash::Device {
        &self.raw
    }

    /// The instance this device was created from.
    pub fn instance(&self) -> &Arc<Instance> {
        &self.instance
    }

    /// The adapter this device drives.
    pub fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical
    }

    /// The queue families in use.
    pub fn queue_families(&self) -> QueueFamilies {
        self.families
    }

    /// The queues to submit through.
    pub fn queues(&self) -> Queues {
        self.queues
    }

    /// Block until the device is idle.
    ///
    /// Only for shutdown and for tests. Waiting on the whole device in a frame
    /// loop discards the pipelining `docs/DESIGN.md` §2.9 exists to enable.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn wait_idle(&self) -> Result<(), RhiError> {
        // SAFETY: the device is alive, and no other thread may submit during
        // this call because it takes `&self` on a type that is not `Sync`
        // through any interior mutability.
        unsafe { self.raw.device_wait_idle() }?;

        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Outstanding GPU work referencing objects this device owns must finish
        // before any of it is destroyed. Skipping this is the classic
        // shutdown-crash that only reproduces under load.
        //
        // SAFETY: the device is still alive here.
        let _ = unsafe { self.raw.device_wait_idle() };

        // SAFETY: every child object is destroyed by this point, and the
        // instance outlives this call because we hold an `Arc` to it.
        unsafe { self.raw.destroy_device(None) };
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("families", &self.families)
            .finish_non_exhaustive()
    }
}
