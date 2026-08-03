//! Vulkan loader, instance, and validation wiring.
//!
//! The instance is the root object everything else hangs off. It deliberately
//! knows nothing about windows: surface extensions are supplied by the caller,
//! so the same code path serves a windowed application and the headless mode
//! `docs/DESIGN.md` §5 requires for golden images and deterministic replay.

use std::ffi::{CStr, CString, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ash::{Entry, vk};
use slop_core::diagnostics::tracing::{debug, error, info, trace, warn};

use crate::RhiError;

/// The Vulkan version the engine targets.
///
/// 1.3 rather than 1.4: everything `docs/DESIGN.md` §2.2 commits to is core in
/// 1.3 — timeline semaphores and descriptor indexing from 1.2, dynamic rendering
/// and synchronization2 from 1.3 — so requiring 1.4 would narrow the supported
/// hardware without buying a feature we need.
pub(crate) const REQUIRED_API_VERSION: u32 = vk::API_VERSION_1_3;

/// The Khronos validation layer, shipped with the Vulkan SDK.
const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

/// Whether to load validation layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Validation {
    /// Enable, and fail construction if the layer is missing.
    Enabled,
    /// Do not enable.
    Disabled,
    /// Enable in debug builds, disable in release. The default.
    #[default]
    Automatic,
}

impl Validation {
    fn wanted(self) -> bool {
        match self {
            Self::Enabled => true,
            Self::Disabled => false,
            // Validation costs a great deal of CPU per call; it belongs in
            // development builds and nowhere near a shipping frame loop.
            Self::Automatic => cfg!(debug_assertions),
        }
    }
}

/// How to build an [`Instance`].
#[derive(Debug, Clone)]
pub struct InstanceConfig {
    /// Reported to the driver, which some vendors use to apply per-application
    /// workarounds.
    pub application_name: String,
    /// Instance extensions the caller requires.
    ///
    /// The windowing layer supplies surface extensions here. Leaving this empty
    /// produces a headless-capable instance.
    pub required_extensions: Vec<CString>,
    /// Whether to load validation layers.
    pub validation: Validation,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            application_name: String::from("slop"),
            required_extensions: Vec::new(),
            validation: Validation::default(),
        }
    }
}

/// A loaded Vulkan instance, plus the debug messenger when validation is on.
pub struct Instance {
    // Field order is the drop order, and it matters: the messenger must be
    // destroyed before the instance that created it, and the entry must outlive
    // both because it owns the loaded library.
    debug: Option<DebugMessenger>,
    raw: ash::Instance,
    entry: Entry,
    validation_enabled: bool,
    /// How many validation errors the messenger has seen. Declared after `debug`
    /// so it outlives the callback holding a pointer to it.
    ///
    /// The one atomic in this crate outside a test, and it is not ambient state:
    /// it belongs to this instance and is reachable only through it. A
    /// `extern "system"` callback has no other channel — the `user_data` pointer
    /// is what Vulkan gives us, so the count has to live behind one.
    errors: Arc<AtomicU64>,
}

struct DebugMessenger {
    loader: ash::ext::debug_utils::Instance,
    handle: vk::DebugUtilsMessengerEXT,
}

impl Instance {
    /// Load the Vulkan loader and create an instance.
    ///
    /// # Errors
    ///
    /// Fails if no loader is present, the loader reports an API version below
    /// Vulkan 1.3, a required extension is missing, or validation was demanded
    /// via [`Validation::Enabled`] and the layer is not installed.
    pub fn new(config: &InstanceConfig) -> Result<Self, RhiError> {
        // SAFETY: `Entry::load` dynamically loads the platform Vulkan library.
        // It is unsafe because loading arbitrary code cannot be checked; the
        // library named is the platform loader and nothing derived from input.
        let entry = unsafe { Entry::load() }.map_err(RhiError::LoaderUnavailable)?;

        Self::check_api_version(&entry)?;

        let extensions = Self::resolve_extensions(&entry, config)?;
        let validation_enabled = Self::resolve_validation(&entry, config.validation)?;

        let layers: Vec<*const i8> = if validation_enabled {
            vec![VALIDATION_LAYER.as_ptr()]
        } else {
            Vec::new()
        };
        let extension_ptrs: Vec<*const i8> = extensions.iter().map(|name| name.as_ptr()).collect();

        let application_name =
            CString::new(config.application_name.as_str()).unwrap_or_else(|_| c"slop".into());
        let application_info = vk::ApplicationInfo::default()
            .application_name(&application_name)
            .engine_name(c"slop")
            .api_version(REQUIRED_API_VERSION);

        // **Synchronization validation, which the core layer does not do.**
        //
        // The layer checks that structures are well-formed and that objects are
        // used in valid states. It does *not* check that a write is ordered
        // against the read that follows it — that is opt-in, and without it a
        // missing or wrong barrier is silent.
        //
        // Measured, not assumed: a compute pass writing a storage buffer, then a
        // transfer reading it with a deliberately *wrong* source state, passed
        // with the layer reporting nothing. The GPU produced the right answer,
        // which is what a race does until it does not.
        //
        // `docs/DESIGN.md` §2.2 commits to explicit barriers, so this is the
        // check that the commitment is met. It matters more before `docs/PLAN.md`
        // §9.5's render graph than after: today every barrier is hand-written,
        // and afterwards this is what says the graph derived them correctly.
        //
        // Switched on through `VK_EXT_layer_settings`. Its predecessor,
        // `VK_EXT_validation_features`, is deprecated and absent from SDK 1.4 —
        // chaining that one instead is accepted and silently does nothing.
        // `values` is raw bytes, so the `VkBool32` goes in as its native-endian
        // representation rather than as a typed slice.
        let sync_on = vk::TRUE.to_ne_bytes();
        let sync_setting = [vk::LayerSettingEXT::default()
            .layer_name(VALIDATION_LAYER)
            .setting_name(c"validate_sync")
            .ty(vk::LayerSettingTypeEXT::BOOL32)
            .values(&sync_on)];
        let mut layer_settings = vk::LayerSettingsCreateInfoEXT::default().settings(&sync_setting);

        let settings_available = extensions
            .iter()
            .any(|name| name.as_c_str() == ash::ext::layer_settings::NAME);

        let mut create_info = vk::InstanceCreateInfo::default()
            .application_info(&application_info)
            .enabled_extension_names(&extension_ptrs)
            .enabled_layer_names(&layers);

        if validation_enabled && settings_available {
            create_info = create_info.push_next(&mut layer_settings);
        }

        // SAFETY: `create_info` borrows `application_info`, `extension_ptrs`,
        // `layers` and — when validation is on — `layer_settings` and everything
        // it points at, all of which outlive this call. The name pointers come
        // from `CString`s owned by `extensions` and by `VALIDATION_LAYER`, which
        // is 'static.
        let raw = unsafe { entry.create_instance(&create_info, None) }?;

        let errors = Arc::new(AtomicU64::new(0));

        let debug = if validation_enabled {
            Some(DebugMessenger::new(&entry, &raw, &errors)?)
        } else {
            None
        };

        info!(
            api_version = "1.3",
            validation = validation_enabled,
            extensions = extension_ptrs.len(),
            "created Vulkan instance"
        );

        Ok(Self {
            debug,
            raw,
            entry,
            validation_enabled,
            errors,
        })
    }

    /// How many validation errors this instance has reported since creation.
    ///
    /// Always zero when validation is disabled, which is every release build —
    /// so this is an assertion a test makes, not a branch a frame takes.
    ///
    /// It exists because validation output otherwise only reaches `tracing`,
    /// where a test cannot see it. A suite that renders correct-looking images
    /// while the layer reports a use-after-free is reporting success at exactly
    /// the moment it should not, which `docs/PLAN.md` §3.1 already records
    /// learning once from golden tests that skipped on setup failure.
    pub fn validation_errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// The underlying `ash` instance.
    pub fn raw(&self) -> &ash::Instance {
        &self.raw
    }

    /// The loader entry point, needed to construct extension loaders.
    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    /// Whether validation layers are active.
    pub fn validation_enabled(&self) -> bool {
        self.validation_enabled
    }

    fn check_api_version(entry: &Entry) -> Result<(), RhiError> {
        // `None` means a Vulkan 1.0 loader, which predates the query itself.
        //
        // SAFETY: querying the instance version on a loaded entry takes no
        // handles and has no preconditions beyond the loader being present,
        // which `Entry::load` established.
        let found =
            unsafe { entry.try_enumerate_instance_version() }?.unwrap_or(vk::API_VERSION_1_0);

        if found < REQUIRED_API_VERSION {
            return Err(RhiError::ApiVersionTooOld {
                required_major: vk::api_version_major(REQUIRED_API_VERSION),
                required_minor: vk::api_version_minor(REQUIRED_API_VERSION),
                found_major: vk::api_version_major(found),
                found_minor: vk::api_version_minor(found),
            });
        }

        Ok(())
    }

    fn resolve_extensions(
        entry: &Entry,
        config: &InstanceConfig,
    ) -> Result<Vec<CString>, RhiError> {
        // SAFETY: enumerating extension properties on a loaded entry with a null
        // layer name is always valid.
        let mut available = unsafe { entry.enumerate_instance_extension_properties(None) }?;

        // **A null layer name lists only what the drivers provide.** Extensions
        // implemented *by a layer* are invisible until that layer is named, and
        // `VK_EXT_layer_settings` is one of them — the validation layer supplies
        // it. Checking only the driver list reports it as unavailable on a
        // machine where the SDK is installed and working, which is what happened
        // here before this second enumeration existed.
        if config.validation.wanted() {
            // SAFETY: as above, with a layer name the loader may or may not
            // know; an unknown layer yields an empty list rather than an error.
            let from_layer =
                unsafe { entry.enumerate_instance_extension_properties(Some(VALIDATION_LAYER)) };

            // Ignored rather than propagated: the layer being absent is what
            // `resolve_validation` reports, and reporting it twice from two
            // places would give the same condition two different messages.
            if let Ok(from_layer) = from_layer {
                available.extend(from_layer);
            }
        }

        let available: Vec<&CStr> = available
            .iter()
            .filter_map(|property| property.extension_name_as_c_str().ok())
            .collect();

        let mut wanted = config.required_extensions.clone();

        // Debug utils carries the validation messages; without it validation
        // would be enabled but silent.
        if config.validation.wanted() {
            wanted.push(ash::ext::debug_utils::NAME.into());
        }

        for name in &wanted {
            if !available.contains(&name.as_c_str()) {
                return Err(RhiError::MissingInstanceExtension(
                    name.to_string_lossy().into_owned(),
                ));
            }
        }

        // Optional, and checked rather than required: it turns on
        // synchronization validation (see `Instance::new`), which is a
        // development aid rather than something the engine needs to run. Absent
        // on older SDKs — and its predecessor, `VK_EXT_validation_features`, is
        // absent on newer ones, having been deprecated in favour of this.
        // Requiring either would make the engine refuse to start on one half of
        // the installed base to gain a check.
        if config.validation.wanted() {
            let settings: CString = ash::ext::layer_settings::NAME.into();

            if available.contains(&settings.as_c_str()) {
                wanted.push(settings);
            } else {
                warn!(
                    "VK_EXT_layer_settings is unavailable; synchronization validation is off \
                     and a missing barrier will not be reported"
                );
            }
        }

        Ok(wanted)
    }

    fn resolve_validation(entry: &Entry, validation: Validation) -> Result<bool, RhiError> {
        if !validation.wanted() {
            return Ok(false);
        }

        // SAFETY: enumerating layer properties on a loaded entry is always valid.
        let layers = unsafe { entry.enumerate_instance_layer_properties() }?;
        let present = layers
            .iter()
            .filter_map(|layer| layer.layer_name_as_c_str().ok())
            .any(|name| name == VALIDATION_LAYER);

        match (present, validation) {
            (true, _) => Ok(true),
            // Explicitly requested and missing is an error, not a downgrade:
            // silently continuing would mean debugging undefined behaviour with
            // the tool that reports it switched off.
            (false, Validation::Enabled) => Err(RhiError::ValidationUnavailable),
            // Automatic is a preference, so falling back is correct — a machine
            // without the SDK should still be able to run a debug build.
            (false, _) => {
                warn!(
                    "validation layers unavailable; install the Vulkan SDK for \
                     validation in debug builds"
                );
                Ok(false)
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Teardown is logged as well as construction. A log that simply stops
        // is indistinguishable from a crash, and "did it shut down cleanly" is
        // the first question asked of a log from someone else's machine.
        //
        // `debug` rather than `info`: an application already knows it is
        // exiting, so this is engine-developer detail rather than a lifecycle
        // event a user cares about (`docs/CONVENTIONS.md` §13).
        debug!("destroying Vulkan instance");

        if let Some(debug) = self.debug.take() {
            // SAFETY: the messenger was created from `self.raw`, which is still
            // alive, and this is the only place it is destroyed.
            unsafe {
                debug
                    .loader
                    .destroy_debug_utils_messenger(debug.handle, None);
            }
        }

        // SAFETY: every child object is destroyed by this point — the messenger
        // above, and devices, which hold an `Arc` to us and therefore cannot
        // outlive this call.
        unsafe { self.raw.destroy_instance(None) };
    }
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("validation_enabled", &self.validation_enabled)
            .finish_non_exhaustive()
    }
}

impl DebugMessenger {
    fn new(
        entry: &Entry,
        instance: &ash::Instance,
        errors: &Arc<AtomicU64>,
    ) -> Result<Self, RhiError> {
        let loader = ash::ext::debug_utils::Instance::new(entry, instance);

        let create_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                    | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                    | vk::DebugUtilsMessageSeverityFlagsEXT::INFO
                    | vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(debug_callback))
            // Points at the `AtomicU64` inside the caller's `Arc`, which
            // `Instance` declares after `debug` and therefore outlives this
            // messenger. The callback only ever reads it as `&AtomicU64`.
            .user_data(Arc::as_ptr(errors).cast::<c_void>().cast_mut());

        // SAFETY: `create_info` is fully initialized and the callback has the
        // signature Vulkan requires. `loader` borrows `instance`, which outlives
        // the messenger because `Instance`'s drop order destroys this first.
        let handle = unsafe { loader.create_debug_utils_messenger(&create_info, None) }?;

        Ok(Self { loader, handle })
    }
}

/// Routes validation output into `tracing` rather than stdout, so it obeys the
/// same filtering as everything else and appears in captured logs.
///
/// Severity maps deliberately: Vulkan's INFO is chatty enough to be `debug`
/// here, keeping `docs/CONVENTIONS.md` §13's rule that `info` stays meaningful.
unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    kind: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    user_data: *mut c_void,
) -> vk::Bool32 {
    // SAFETY: Vulkan guarantees `data` points to a valid callback-data struct
    // for the duration of this call.
    let data = unsafe { &*data };

    let message = if data.p_message.is_null() {
        "<no message>"
    } else {
        // SAFETY: when non-null, Vulkan guarantees a NUL-terminated string valid
        // for this call.
        unsafe { CStr::from_ptr(data.p_message) }
            .to_str()
            .unwrap_or("<invalid utf-8>")
    };

    let kind = match kind {
        vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION => "validation",
        vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE => "performance",
        _ => "general",
    };

    if severity == vk::DebugUtilsMessageSeverityFlagsEXT::ERROR && !user_data.is_null() {
        // SAFETY: `user_data` is the pointer `DebugMessenger::new` took from an
        // `Arc<AtomicU64>` owned by the `Instance` that created this messenger.
        // That `Arc` is declared after `debug`, so it is still alive for as long
        // as the messenger can call back into here.
        unsafe { &*user_data.cast::<AtomicU64>() }.fetch_add(1, Ordering::Relaxed);
    }

    match severity {
        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR => error!(kind, "{message}"),
        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING => warn!(kind, "{message}"),
        vk::DebugUtilsMessageSeverityFlagsEXT::INFO => debug!(kind, "{message}"),
        _ => trace!(kind, "{message}"),
    }

    // Always false. Returning true aborts the offending call, which is a
    // debugging aid rather than something an engine should do to its own frames.
    vk::FALSE
}
