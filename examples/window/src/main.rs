//! Proves the window → surface → device chain connects end to end.
//!
//! This is M0 task E's exit criterion in executable form. It opens a window,
//! creates a Vulkan instance with the surface extensions that window's display
//! requires, builds a surface from it, selects an adapter that can actually
//! present to that surface, creates a logical device, and builds a swapchain.
//! It reports what it found, then stays open until closed.
//!
//! **Nothing is drawn.** The swapchain exists but no frame is ever recorded or
//! presented, so the window's contents are undefined — usually black, possibly
//! whatever the compositor left there. Rendering is task F.
//!
//! Run with `cargo run -p example-window`, or `SLOP_LOG=debug` for more.
//!
//! Note this file owns `main()` and drives the event loop itself, per
//! `docs/DESIGN.md` §1.2 principle 4. The engine supplies pieces; it does not
//! supply a framework to sit inside.

use slop_app::gpu::{Gpu, GpuConfig};
use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_rhi::{DeviceInfo, PresentMode, Surface, Swapchain, SwapchainConfig};

fn main() {
    slop_app::logging::init();

    let event_loop = EventLoop::new().expect("an event loop must be creatable");
    let mut app = App::default();

    event_loop.run_app(&mut app).expect("the event loop failed");

    if let Some(error) = app.failure {
        eprintln!("setup failed: {error}");
        std::process::exit(1);
    }
}

/// Live Vulkan objects, in drop order.
///
/// Field order matters and is the reason this is a struct rather than loose
/// locals: the swapchain must be destroyed before the device, surface and window
/// it came from, and Rust drops fields top to bottom. Those last three are
/// [`Gpu`]'s problem — it exists so that ordering is stated once rather than
/// re-derived by every application.
// The swapchain is never read, and that is the point: it exists to keep the
// object alive and to destroy it at the right moment. `expect` rather than
// `allow` so this becomes an error once something does read it, which is the
// signal to delete the attribute.
#[expect(dead_code, reason = "held for RAII and drop ordering, not for reading")]
struct Graphics {
    swapchain: Swapchain,
    gpu: Gpu,
}

impl Drop for Graphics {
    fn drop(&mut self) {
        slop_core::diagnostics::tracing::info!("shutting down");

        // Nothing is ever submitted here, so there is genuinely no work to wait
        // for — but the pattern is the point. `Device::drop` waits too, and that
        // is always too late for fields declared before it, so an owner of
        // Vulkan objects waits in its own `Drop`. Examples get copied.
        if let Err(error) = self.gpu.wait_idle() {
            slop_core::diagnostics::tracing::error!(%error, "device did not go idle");
        }
    }
}

#[derive(Default)]
struct App {
    graphics: Option<Graphics>,
    failure: Option<String>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // `resumed` can fire more than once on some platforms.
        if self.graphics.is_some() {
            return;
        }

        match setup(event_loop) {
            Ok(graphics) => self.graphics = Some(graphics),
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
                return;
            }
        }

        // The window stays open until it is closed. Nothing draws into the
        // swapchain yet, so its contents are undefined — expect whatever the
        // compositor had there, or black. That is correct for M0 task E; the
        // first render is task F.
        println!("\nwindow is open — close it to exit.");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }
}

fn setup(event_loop: &ActiveEventLoop) -> Result<Graphics, String> {
    // Window, instance, surface, adapter selection and device, in the one order
    // Vulkan permits — see `slop_app::gpu`, which is where that order now lives
    // so that four examples cannot each drift a different way.
    let gpu = Gpu::new(
        event_loop,
        &GpuConfig {
            window: WindowConfig {
                title: String::from("slop — window and surface"),
                ..Default::default()
            },
            application_name: String::from("example-window"),
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;

    // Queried again after the fact purely to print it. This is the list the
    // instance was created with, and asking a window for it is a pure function
    // of its display handle — so reporting it here says the same thing the old
    // inline version did, without duplicating bring-up to get at it.
    match window::required_instance_extensions(gpu.window()) {
        Ok(extensions) => {
            for extension in &extensions {
                println!(
                    "  required instance extension: {}",
                    extension.to_string_lossy()
                );
            }
        }
        Err(error) => println!("  required instance extensions unavailable: {error}"),
    }

    report(gpu.adapters(), gpu.chosen(), gpu.surface());

    // `Gpu::extent` is in physical pixels, which is what a swapchain needs. The
    // logical size passed to WindowConfig is a different number on any display
    // that is not at 100% scaling.
    let size = gpu.extent();
    let swapchain = Swapchain::new(
        gpu.device(),
        gpu.surface(),
        &SwapchainConfig {
            present_mode: PresentMode::Mailbox,
            extent: size,
        },
    )
    .map_err(|error| error.to_string())?;

    println!(
        "\nswapchain: {}x{}, {} images, {:?}, {:?}",
        swapchain.extent().width,
        swapchain.extent().height,
        swapchain.images().len(),
        swapchain.format(),
        swapchain.present_mode(),
    );
    println!(
        "requested {}x{} logical, surface reported {}x{} physical",
        WindowConfig::default().width,
        WindowConfig::default().height,
        size.width,
        size.height,
    );

    println!("\nwindow, surface, device and swapchain all created successfully.");

    Ok(Graphics { swapchain, gpu })
}

fn report(devices: &[DeviceInfo], chosen: usize, surface: &Surface) {
    println!("\nadapters:");
    for (index, device) in devices.iter().enumerate() {
        let marker = if index == chosen { "->" } else { "  " };
        let status = device
            .rejection
            .as_ref()
            .map_or_else(|| String::from("usable"), ToString::to_string);

        println!(
            "{marker} {} — {:?}, {} MiB, {status}",
            device.name,
            device.kind,
            device.memory_mib()
        );
    }

    let selected = &devices[chosen];

    if let Some(families) = selected.queue_families() {
        println!(
            "\nqueue families: graphics {}, compute {}, transfer {}, present {:?}",
            families.graphics, families.compute, families.transfer, families.present
        );
        println!(
            "async compute: {}, async transfer: {}",
            families.has_async_compute(),
            families.has_async_transfer()
        );
    }

    match surface.capabilities(selected.handle()) {
        Ok(capabilities) => println!(
            "\nsurface: {}x{}, {} to {} images",
            capabilities.current_extent.width,
            capabilities.current_extent.height,
            capabilities.min_image_count,
            capabilities.max_image_count,
        ),
        Err(error) => println!("\nsurface capabilities unavailable: {error}"),
    }

    if let Ok(formats) = surface.formats(selected.handle()) {
        println!("surface formats: {}", formats.len());
        for format in formats.iter().take(4) {
            println!("  {:?} / {:?}", format.format, format.color_space);
        }
    }

    if let Ok(modes) = surface.present_modes(selected.handle()) {
        let names: Vec<String> = modes.iter().map(|mode| format!("{mode:?}")).collect();
        println!("present modes: {}", names.join(", "));
    }
}
