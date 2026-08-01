//! Queue family discovery.
//!
//! Vulkan exposes work submission through *queue families*, each supporting some
//! mix of graphics, compute, transfer, and presentation. `docs/DESIGN.md` §2.2
//! commits to acquiring graphics, compute, and transfer queues up front, because
//! async compute and async transfer are not features that can be retrofitted
//! into a renderer that assumed one queue.

use ash::vk;

/// The queue families the engine uses.
///
/// Families may coincide — on many drivers every capability lives in one family
/// — and the engine must behave correctly either way. Distinctness is an
/// optimization, never an assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFamilies {
    /// Graphics work. Always also supports transfer, per the Vulkan spec.
    pub graphics: u32,
    /// Compute work. Prefers a family without graphics, so compute can overlap
    /// rendering instead of serializing behind it.
    pub compute: u32,
    /// Transfers. Prefers a family with neither graphics nor compute, which on
    /// discrete hardware is the dedicated DMA engine and can move data across
    /// PCIe while both other queues stay busy.
    pub transfer: u32,
    /// Presentation, when a surface was supplied. `None` in headless mode.
    pub present: Option<u32>,
}

impl QueueFamilies {
    /// Find suitable families, or `None` if this device cannot serve the engine.
    ///
    /// Pass a surface when the result must be able to present; `None` selects
    /// for headless use, which `docs/DESIGN.md` §5 requires.
    pub(crate) fn find(
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
        surface: Option<&crate::Surface>,
    ) -> Option<Self> {
        // SAFETY: `device` came from this instance's enumeration, and this query
        // has no other preconditions.
        let families = unsafe { instance.get_physical_device_queue_family_properties(device) };

        let graphics = families.iter().position(|family| {
            family.queue_flags.contains(vk::QueueFlags::GRAPHICS) && family.queue_count > 0
        })? as u32;

        // Dedicated first, shared as fallback. A device with no compute-capable
        // family at all cannot serve us, but the spec guarantees graphics
        // families also support compute, so `graphics` is always a valid answer.
        let compute =
            Self::find_dedicated(&families, vk::QueueFlags::COMPUTE, vk::QueueFlags::GRAPHICS)
                .unwrap_or(graphics);

        let transfer = Self::find_dedicated(
            &families,
            vk::QueueFlags::TRANSFER,
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
        )
        .unwrap_or(graphics);

        let present = match surface {
            None => None,
            Some(surface) => {
                let found = (0..families.len() as u32).find(|&index| {
                    // SAFETY: `index` is within the family count just queried,
                    // and the surface belongs to the same instance as `device`.
                    unsafe {
                        surface
                            .loader()
                            .get_physical_device_surface_support(device, index, surface.handle())
                            .unwrap_or(false)
                    }
                });

                // A surface was requested but nothing can present to it, so this
                // device is unusable for a windowed application.
                Some(found?)
            }
        };

        Some(Self {
            graphics,
            compute,
            transfer,
            present,
        })
    }

    /// A family supporting `wanted` while supporting none of `avoid`.
    fn find_dedicated(
        families: &[vk::QueueFamilyProperties],
        wanted: vk::QueueFlags,
        avoid: vk::QueueFlags,
    ) -> Option<u32> {
        families
            .iter()
            .position(|family| {
                family.queue_count > 0
                    && family.queue_flags.contains(wanted)
                    && !family.queue_flags.intersects(avoid)
            })
            .map(|index| index as u32)
    }

    /// The distinct family indices, for logical device creation.
    ///
    /// Vulkan rejects a `VkDeviceCreateInfo` that names the same family twice,
    /// so deduplicating is a correctness requirement rather than tidiness — and
    /// families coinciding is the normal case on integrated hardware.
    pub(crate) fn distinct(&self) -> Vec<u32> {
        let mut indices = vec![self.graphics, self.compute, self.transfer];

        if let Some(present) = self.present {
            indices.push(present);
        }

        indices.sort_unstable();
        indices.dedup();
        indices
    }

    /// Whether compute has its own family and can genuinely overlap graphics.
    pub fn has_async_compute(&self) -> bool {
        self.compute != self.graphics
    }

    /// Whether transfers have a dedicated family — typically a DMA engine.
    pub fn has_async_transfer(&self) -> bool {
        self.transfer != self.graphics && self.transfer != self.compute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family(flags: vk::QueueFlags) -> vk::QueueFamilyProperties {
        vk::QueueFamilyProperties {
            queue_flags: flags,
            queue_count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn prefers_a_compute_family_without_graphics() {
        let families = [
            family(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE),
            family(vk::QueueFlags::COMPUTE),
        ];

        let found = QueueFamilies::find_dedicated(
            &families,
            vk::QueueFlags::COMPUTE,
            vk::QueueFlags::GRAPHICS,
        );

        assert_eq!(found, Some(1));
    }

    #[test]
    fn prefers_a_transfer_family_without_graphics_or_compute() {
        let families = [
            family(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER),
            family(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER),
            family(vk::QueueFlags::TRANSFER),
        ];

        let found = QueueFamilies::find_dedicated(
            &families,
            vk::QueueFlags::TRANSFER,
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
        );

        assert_eq!(found, Some(2), "the DMA-only family should win");
    }

    #[test]
    fn reports_no_dedicated_family_when_everything_is_combined() {
        // The common case on integrated GPUs. Callers fall back to graphics.
        let families = [family(
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER,
        )];

        assert_eq!(
            QueueFamilies::find_dedicated(
                &families,
                vk::QueueFlags::COMPUTE,
                vk::QueueFlags::GRAPHICS
            ),
            None
        );
    }

    #[test]
    fn empty_families_are_never_selected() {
        // queue_count of zero means the family exists but offers no queues.
        let families = [vk::QueueFamilyProperties {
            queue_flags: vk::QueueFlags::COMPUTE,
            queue_count: 0,
            ..Default::default()
        }];

        assert_eq!(
            QueueFamilies::find_dedicated(
                &families,
                vk::QueueFlags::COMPUTE,
                vk::QueueFlags::GRAPHICS
            ),
            None
        );
    }

    #[test]
    fn distinct_deduplicates_coinciding_families() {
        // Vulkan rejects a create-info naming one family twice, so this is a
        // correctness requirement. Integrated hardware hits it routinely.
        let shared = QueueFamilies {
            graphics: 0,
            compute: 0,
            transfer: 0,
            present: Some(0),
        };

        assert_eq!(shared.distinct(), vec![0]);
    }

    #[test]
    fn distinct_keeps_genuinely_separate_families() {
        let split = QueueFamilies {
            graphics: 0,
            compute: 1,
            transfer: 2,
            present: Some(3),
        };

        assert_eq!(split.distinct(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn shared_families_report_no_async_capability() {
        // The integrated-GPU case: one family does everything, so nothing can
        // genuinely overlap. Correct behaviour, not a failure.
        let shared = QueueFamilies {
            graphics: 0,
            compute: 0,
            transfer: 0,
            present: Some(0),
        };

        assert!(!shared.has_async_compute());
        assert!(!shared.has_async_transfer());
    }

    #[test]
    fn separate_families_report_async_capability() {
        let split = QueueFamilies {
            graphics: 0,
            compute: 1,
            transfer: 2,
            present: Some(0),
        };

        assert!(split.has_async_compute());
        assert!(split.has_async_transfer());
    }
}
