//! The Vulkan feature set the engine requires, declared in exactly one place.
//!
//! `docs/DESIGN.md` §2.1 buys "one GPU feature tier, no capability-tier
//! branching in the renderer" by targeting desktop only. That guarantee is worth
//! nothing unless the tier is stated somewhere singular and checked before a
//! device is accepted — otherwise it degrades into scattered runtime checks,
//! which is precisely the branching the decision exists to avoid.
//!
//! So: a device either supports everything here and is usable, or it is rejected
//! by name. There is no partial support and no fallback path.

use ash::vk;

/// Features required for `docs/DESIGN.md` §2.2's explicit rendering model.
///
/// Returned as the three structs Vulkan splits them across, ready to chain into
/// device creation.
pub(crate) fn required() -> (
    vk::PhysicalDeviceFeatures,
    vk::PhysicalDeviceVulkan12Features<'static>,
    vk::PhysicalDeviceVulkan13Features<'static>,
) {
    let core = vk::PhysicalDeviceFeatures::default()
        // Indirect draws with a GPU-supplied count are the basis of the
        // GPU-driven pipeline in §4.2 stage B.
        .multi_draw_indirect(true)
        .draw_indirect_first_instance(true)
        // Anisotropic filtering is table stakes for the fidelity target.
        .sampler_anisotropy(true)
        // Wireframe, for the debug UI in §10.2.
        .fill_mode_non_solid(true)
        // Non-uniform indexing into resource arrays is what makes a bindless
        // shader able to pick a material per draw.
        .shader_sampled_image_array_dynamic_indexing(true)
        .shader_storage_buffer_array_dynamic_indexing(true);

    let vulkan_12 = vk::PhysicalDeviceVulkan12Features::default()
        // §2.2: timeline semaphores, not fences plus binary semaphores.
        .timeline_semaphore(true)
        // §2.2: the bindless descriptor model. These six together are what
        // "bindless" actually means in Vulkan terms.
        .descriptor_indexing(true)
        .runtime_descriptor_array(true)
        .descriptor_binding_partially_bound(true)
        .descriptor_binding_variable_descriptor_count(true)
        .descriptor_binding_sampled_image_update_after_bind(true)
        .shader_sampled_image_array_non_uniform_indexing(true)
        // Lets shaders hold raw pointers into buffers, which is how GPU-driven
        // passes walk structures the CPU never binds.
        .buffer_device_address(true)
        // Draw counts sourced from a buffer rather than the CPU.
        .draw_indirect_count(true);

    let vulkan_13 = vk::PhysicalDeviceVulkan13Features::default()
        // §2.2: explicit barriers. `synchronization2` is the modern, far less
        // error-prone barrier API.
        .synchronization2(true)
        // Removes render pass and framebuffer objects, which the render graph
        // in §4.2 would otherwise have to cache and invalidate.
        .dynamic_rendering(true);

    (core, vulkan_12, vulkan_13)
}

/// Names of required features this device does not support.
///
/// Empty means usable. The names are the Vulkan spec's own, so a rejection can
/// be looked up directly rather than translated.
pub(crate) fn missing(instance: &ash::Instance, device: vk::PhysicalDevice) -> Vec<&'static str> {
    let mut vulkan_13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut vulkan_12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut supported = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut vulkan_12)
        .push_next(&mut vulkan_13);

    // SAFETY: `device` came from this instance's enumeration, and `supported`
    // is a fully initialized struct with a valid pNext chain whose members
    // outlive the call.
    unsafe { instance.get_physical_device_features2(device, &mut supported) };

    let core = supported.features;
    let mut missing = Vec::new();

    // Compares a required flag against what the device reports, recording the
    // spec's own name when it is absent.
    macro_rules! require {
        ($source:expr, $field:ident) => {
            if $source.$field == vk::FALSE {
                missing.push(stringify!($field));
            }
        };
    }

    require!(core, multi_draw_indirect);
    require!(core, draw_indirect_first_instance);
    require!(core, sampler_anisotropy);
    require!(core, fill_mode_non_solid);
    require!(core, shader_sampled_image_array_dynamic_indexing);
    require!(core, shader_storage_buffer_array_dynamic_indexing);

    require!(vulkan_12, timeline_semaphore);
    require!(vulkan_12, descriptor_indexing);
    require!(vulkan_12, runtime_descriptor_array);
    require!(vulkan_12, descriptor_binding_partially_bound);
    require!(vulkan_12, descriptor_binding_variable_descriptor_count);
    require!(
        vulkan_12,
        descriptor_binding_sampled_image_update_after_bind
    );
    require!(vulkan_12, shader_sampled_image_array_non_uniform_indexing);
    require!(vulkan_12, buffer_device_address);
    require!(vulkan_12, draw_indirect_count);

    require!(vulkan_13, synchronization2);
    require!(vulkan_13, dynamic_rendering);

    missing
}
