//! Physical device enumeration, scoring, and selection.
//!
//! Enumeration is **public API**, not an internal step. `docs/DESIGN.md` §7
//! makes the engine a platform, and a game built on it needs to render a GPU
//! picker in its graphics settings — which means listing every adapter, saying
//! which are usable, and saying *why* the others are not so the UI can grey them
//! out with a reason rather than hiding them.
//!
//! # Selection is by UUID, not index
//!
//! A player's saved choice must survive adding a GPU, removing one, or a driver
//! update reordering them. Indices do not; `deviceUUID` does. A saved UUID that
//! no longer resolves falls back to automatic selection with a warning, because
//! swapping a graphics card must not prevent a game from launching.

use ash::vk;
use slop_core::diagnostics::tracing::{info, warn};

use crate::{Instance, QueueFamilies, RhiError};

/// Broad category of adapter, as the driver reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A separate card with its own memory. What we want.
    Discrete,
    /// Shares memory with the CPU. Usable, far slower.
    Integrated,
    /// A virtualized or paravirtualized adapter.
    Virtual,
    /// A software rasterizer — lavapipe, SwiftShader. Correct but very slow;
    /// this is what CI golden images render on (`docs/PLAN.md` §4.1-G).
    Cpu,
    /// The driver reported something else.
    Other,
}

impl DeviceKind {
    fn from_vk(kind: vk::PhysicalDeviceType) -> Self {
        match kind {
            vk::PhysicalDeviceType::DISCRETE_GPU => Self::Discrete,
            vk::PhysicalDeviceType::INTEGRATED_GPU => Self::Integrated,
            vk::PhysicalDeviceType::VIRTUAL_GPU => Self::Virtual,
            vk::PhysicalDeviceType::CPU => Self::Cpu,
            _ => Self::Other,
        }
    }

    /// Ranking used by automatic selection. Higher wins.
    fn rank(self) -> u32 {
        match self {
            Self::Discrete => 4,
            Self::Integrated => 3,
            Self::Virtual => 2,
            Self::Other => 1,
            // Last by a wide margin: a software rasterizer is correct but
            // thousands of times too slow to pick by accident.
            Self::Cpu => 0,
        }
    }
}

/// Why a device cannot be used.
///
/// Carried so a settings UI can explain a greyed-out entry, and so a log says
/// what was wrong rather than that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The device does not support the Vulkan version the engine requires.
    ApiVersionTooOld {
        /// Major version the device reports.
        major: u32,
        /// Minor version the device reports.
        minor: u32,
    },
    /// No queue family combination satisfies the engine.
    NoSuitableQueues,
    /// A surface was supplied and this device cannot present to it. Common for
    /// the integrated GPU on a machine whose display is wired to the discrete
    /// one.
    CannotPresent,
    /// The device lacks features the engine requires.
    ///
    /// Names are the Vulkan spec's own, so a report can be looked up directly.
    /// `docs/DESIGN.md` §2.1 buys one feature tier with no capability
    /// branching, which only holds if devices below the tier are rejected here
    /// rather than worked around later.
    MissingFeatures(Vec<&'static str>),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiVersionTooOld { major, minor } => {
                write!(f, "supports only Vulkan {major}.{minor}")
            }
            Self::NoSuitableQueues => write!(f, "no suitable queue families"),
            Self::CannotPresent => write!(f, "cannot present to this window"),
            Self::MissingFeatures(names) => {
                write!(f, "missing required features: {}", names.join(", "))
            }
        }
    }
}

/// Everything needed to describe an adapter to a player, and to select it.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Human-readable name, as the driver reports it.
    pub name: String,
    /// Discrete, integrated, software, and so on.
    pub kind: DeviceKind,
    /// Stable across runs, driver updates, and reordering. **This is what a
    /// game persists when a player picks a GPU.**
    pub uuid: [u8; vk::UUID_SIZE],
    /// PCI vendor identifier.
    pub vendor_id: u32,
    /// PCI device identifier.
    pub device_id: u32,
    /// Total device-local memory in bytes. Approximates VRAM.
    pub device_local_memory: u64,
    /// Vulkan version the device supports.
    pub api_version: u32,
    /// `None` when usable; `Some(reason)` when not.
    pub rejection: Option<Rejection>,

    handle: vk::PhysicalDevice,
    queue_families: Option<QueueFamilies>,
}

impl DeviceInfo {
    /// Whether this adapter can run the engine.
    pub fn is_usable(&self) -> bool {
        self.rejection.is_none()
    }

    /// Device-local memory in whole mebibytes, for display.
    pub fn memory_mib(&self) -> u64 {
        self.device_local_memory / (1024 * 1024)
    }

    /// The underlying handle. Only meaningful with the instance it came from.
    pub fn handle(&self) -> vk::PhysicalDevice {
        self.handle
    }

    /// Queue families, when the device is usable.
    pub fn queue_families(&self) -> Option<QueueFamilies> {
        self.queue_families
    }

    /// Automatic-selection score. Only compared between usable devices.
    fn score(&self) -> u64 {
        // Kind dominates: a discrete card with less memory still beats an
        // integrated one with more, because the memory an iGPU reports is
        // system RAM it shares with everything else.
        let kind = u64::from(self.kind.rank()) << 32;

        kind | self.memory_mib().min(u64::from(u32::MAX))
    }
}

/// How to choose among the available adapters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceSelection {
    /// Highest-scoring usable device. The default.
    #[default]
    Automatic,
    /// A specific device by stable UUID — what a game persists from its
    /// graphics settings. Falls back to [`Automatic`](Self::Automatic) with a
    /// warning if that device is no longer present, so swapping a GPU cannot
    /// prevent launching.
    ByUuid([u8; vk::UUID_SIZE]),
    /// A specific device by enumeration index.
    ///
    /// For developer tooling and test harnesses, not for player-facing
    /// settings — indices shift when hardware or drivers change. Unlike
    /// [`ByUuid`](Self::ByUuid) this does **not** fall back, because a test
    /// pinning a device wants to fail loudly rather than silently measure a
    /// different one.
    ByIndex(usize),
}

/// List every adapter the instance can see, usable or not.
///
/// `surface` is `(loader, handle)` when the result must be able to present;
/// `None` enumerates for headless use.
///
/// # Errors
///
/// Fails only if enumeration itself fails. Individual unusable devices are
/// reported through [`DeviceInfo::rejection`], not as errors.
pub fn enumerate(
    instance: &Instance,
    surface: Option<(&ash::khr::surface::Instance, vk::SurfaceKHR)>,
) -> Result<Vec<DeviceInfo>, RhiError> {
    // SAFETY: the instance is alive for the duration of this call.
    let handles = unsafe { instance.raw().enumerate_physical_devices() }?;

    Ok(handles
        .into_iter()
        .map(|handle| describe(instance, handle, surface))
        .collect())
}

fn describe(
    instance: &Instance,
    handle: vk::PhysicalDevice,
    surface: Option<(&ash::khr::surface::Instance, vk::SurfaceKHR)>,
) -> DeviceInfo {
    let mut id_properties = vk::PhysicalDeviceIDProperties::default();
    let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id_properties);

    // SAFETY: `properties2` is fully initialized with a valid pNext chain, and
    // `handle` came from this instance.
    unsafe {
        instance
            .raw()
            .get_physical_device_properties2(handle, &mut properties2);
    }

    let properties = properties2.properties;

    // SAFETY: same preconditions as above.
    let memory = unsafe { instance.raw().get_physical_device_memory_properties(handle) };

    // Sum the heaps flagged device-local. On a discrete card this is VRAM; on an
    // integrated one it is a slice of system RAM, which is why kind outranks
    // memory in scoring.
    let device_local_memory = memory.memory_heaps[..memory.memory_heap_count as usize]
        .iter()
        .filter(|heap| heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .map(|heap| heap.size)
        .sum();

    let name = properties
        .device_name_as_c_str()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from("<unnamed device>"));

    let queue_families = QueueFamilies::find(instance.raw(), handle, surface);

    // Ordered deliberately. The version check comes first because the feature
    // query below relies on Vulkan 1.2 and 1.3 structures the device would not
    // understand otherwise, and reporting "missing features" for a device that
    // is simply too old would be a misleading diagnosis.
    let rejection = if properties.api_version < crate::REQUIRED_API_VERSION {
        Some(Rejection::ApiVersionTooOld {
            major: vk::api_version_major(properties.api_version),
            minor: vk::api_version_minor(properties.api_version),
        })
    } else if queue_families.is_none() {
        Some(if surface.is_some() {
            Rejection::CannotPresent
        } else {
            Rejection::NoSuitableQueues
        })
    } else {
        match crate::features::missing(instance.raw(), handle) {
            missing if missing.is_empty() => None,
            missing => Some(Rejection::MissingFeatures(missing)),
        }
    };

    DeviceInfo {
        name,
        kind: DeviceKind::from_vk(properties.device_type),
        uuid: id_properties.device_uuid,
        vendor_id: properties.vendor_id,
        device_id: properties.device_id,
        device_local_memory,
        api_version: properties.api_version,
        rejection,
        handle,
        queue_families,
    }
}

/// Resolve a selection against an enumeration.
///
/// Logs which device won and what it beat, because a user reporting "the game is
/// slow" is only diagnosable if the log says which adapter it actually ran on
/// (`docs/CONVENTIONS.md` §13).
///
/// # Errors
///
/// Fails if no usable device exists, or if [`DeviceSelection::ByIndex`] names a
/// device that is absent or unusable.
pub fn select(devices: &[DeviceInfo], selection: &DeviceSelection) -> Result<usize, RhiError> {
    let chosen = match selection {
        DeviceSelection::ByIndex(index) => {
            let device = devices.get(*index).ok_or(RhiError::NoSuitableDevice {
                considered: devices.len(),
            })?;

            if let Some(rejection) = &device.rejection {
                return Err(RhiError::DeviceUnsuitable {
                    name: device.name.clone(),
                    reason: rejection.to_string(),
                });
            }

            *index
        }
        DeviceSelection::ByUuid(uuid) => {
            match devices
                .iter()
                .position(|device| device.uuid == *uuid && device.is_usable())
            {
                Some(index) => index,
                None => {
                    // The saved GPU is gone or no longer usable. Falling back is
                    // the whole point of storing a preference rather than a
                    // requirement.
                    warn!("the saved graphics device is unavailable; selecting automatically");
                    best(devices)?
                }
            }
        }
        DeviceSelection::Automatic => best(devices)?,
    };

    log_choice(devices, chosen);
    Ok(chosen)
}

fn best(devices: &[DeviceInfo]) -> Result<usize, RhiError> {
    devices
        .iter()
        .enumerate()
        .filter(|(_, device)| device.is_usable())
        .max_by_key(|(_, device)| device.score())
        .map(|(index, _)| index)
        .ok_or(RhiError::NoSuitableDevice {
            considered: devices.len(),
        })
}

fn log_choice(devices: &[DeviceInfo], chosen: usize) {
    let device = &devices[chosen];

    info!(
        device = %device.name,
        kind = ?device.kind,
        vram_mib = device.memory_mib(),
        async_compute = device.queue_families().is_some_and(|q| q.has_async_compute()),
        async_transfer = device.queue_families().is_some_and(|q| q.has_async_transfer()),
        "selected physical device"
    );

    for (index, other) in devices.iter().enumerate() {
        if index == chosen {
            continue;
        }

        match &other.rejection {
            Some(reason) => info!(device = %other.name, %reason, "rejected device"),
            None => info!(device = %other.name, kind = ?other.kind, "passed over device"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, kind: DeviceKind, mib: u64, uuid: u8) -> DeviceInfo {
        DeviceInfo {
            name: String::from(name),
            kind,
            uuid: [uuid; vk::UUID_SIZE],
            vendor_id: 0,
            device_id: 0,
            device_local_memory: mib * 1024 * 1024,
            api_version: vk::API_VERSION_1_3,
            rejection: None,
            handle: vk::PhysicalDevice::null(),
            queue_families: Some(QueueFamilies {
                graphics: 0,
                compute: 0,
                transfer: 0,
                present: None,
            }),
        }
    }

    fn unusable(name: &str, uuid: u8) -> DeviceInfo {
        DeviceInfo {
            rejection: Some(Rejection::CannotPresent),
            queue_families: None,
            ..device(name, DeviceKind::Discrete, 16384, uuid)
        }
    }

    #[test]
    fn discrete_beats_integrated_even_with_less_memory() {
        // The case that matters on this machine, and the one a naive
        // memory-only score gets wrong: an iGPU reports shared system RAM.
        let devices = [
            device("UHD 770", DeviceKind::Integrated, 32768, 1),
            device("RTX 5090", DeviceKind::Discrete, 8192, 2),
        ];

        let chosen = select(&devices, &DeviceSelection::Automatic).expect("one is usable");

        assert_eq!(devices[chosen].name, "RTX 5090");
    }

    #[test]
    fn more_memory_wins_within_the_same_kind() {
        let devices = [
            device("small", DeviceKind::Discrete, 8192, 1),
            device("large", DeviceKind::Discrete, 24576, 2),
        ];

        let chosen = select(&devices, &DeviceSelection::Automatic).expect("both usable");

        assert_eq!(devices[chosen].name, "large");
    }

    #[test]
    fn a_software_rasterizer_is_never_chosen_over_real_hardware() {
        let devices = [
            device("lavapipe", DeviceKind::Cpu, 65536, 1),
            device("UHD 770", DeviceKind::Integrated, 2048, 2),
        ];

        let chosen = select(&devices, &DeviceSelection::Automatic).expect("both usable");

        assert_eq!(devices[chosen].name, "UHD 770");
    }

    #[test]
    fn unusable_devices_are_never_selected() {
        let devices = [
            unusable("headless card", 1),
            device("UHD 770", DeviceKind::Integrated, 2048, 2),
        ];

        let chosen = select(&devices, &DeviceSelection::Automatic).expect("one is usable");

        assert_eq!(devices[chosen].name, "UHD 770");
    }

    #[test]
    fn no_usable_device_is_an_error() {
        let devices = [unusable("a", 1), unusable("b", 2)];

        assert!(matches!(
            select(&devices, &DeviceSelection::Automatic),
            Err(RhiError::NoSuitableDevice { considered: 2 })
        ));
    }

    #[test]
    fn a_saved_uuid_selects_that_device_over_the_better_one() {
        // The player's explicit choice must win, even when it is not what
        // automatic selection would pick.
        let devices = [
            device("RTX 5090", DeviceKind::Discrete, 32768, 1),
            device("UHD 770", DeviceKind::Integrated, 2048, 2),
        ];

        let chosen = select(&devices, &DeviceSelection::ByUuid([2; vk::UUID_SIZE]))
            .expect("the saved device is present");

        assert_eq!(devices[chosen].name, "UHD 770");
    }

    #[test]
    fn a_saved_uuid_that_vanished_falls_back_instead_of_failing() {
        // The player swapped their graphics card. The game must still launch.
        let devices = [device("RTX 5090", DeviceKind::Discrete, 32768, 1)];

        let chosen = select(&devices, &DeviceSelection::ByUuid([99; vk::UUID_SIZE]))
            .expect("must fall back rather than fail");

        assert_eq!(devices[chosen].name, "RTX 5090");
    }

    #[test]
    fn a_saved_uuid_naming_an_unusable_device_falls_back() {
        let devices = [
            unusable("old card", 7),
            device("RTX 5090", DeviceKind::Discrete, 32768, 1),
        ];

        let chosen =
            select(&devices, &DeviceSelection::ByUuid([7; vk::UUID_SIZE])).expect("must fall back");

        assert_eq!(devices[chosen].name, "RTX 5090");
    }

    #[test]
    fn by_index_does_not_fall_back() {
        // Unlike ByUuid: a test harness pinning a device wants to fail loudly
        // rather than silently measure a different one.
        let devices = [device("only", DeviceKind::Discrete, 1024, 1)];

        assert!(matches!(
            select(&devices, &DeviceSelection::ByIndex(5)),
            Err(RhiError::NoSuitableDevice { .. })
        ));
    }

    #[test]
    fn by_index_naming_an_unusable_device_reports_the_reason() {
        let devices = [unusable("headless card", 1)];

        match select(&devices, &DeviceSelection::ByIndex(0)) {
            Err(RhiError::DeviceUnsuitable { name, reason }) => {
                assert_eq!(name, "headless card");
                assert_eq!(reason, "cannot present to this window");
            }
            other => panic!("expected DeviceUnsuitable, got {other:?}"),
        }
    }
}
