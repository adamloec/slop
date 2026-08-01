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

use std::sync::Arc;

use slop_app::window::{self, WindowConfig};
use slop_app::winit::application::ApplicationHandler;
use slop_app::winit::event::WindowEvent;
use slop_app::winit::event_loop::{ActiveEventLoop, EventLoop};
use slop_app::winit::window::WindowId;
use slop_rhi::{Device, DeviceSelection, Instance, InstanceConfig};
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
/// locals: the device and surface must be destroyed before the window they came
/// from, and Rust drops fields top to bottom.
// These fields are never read, and that is the point: they exist to keep the
// objects alive and to destroy them in the right order. `expect` rather than
// `allow` so this becomes an error once something does read them, which is the
// signal to delete the attribute.
#[expect(dead_code, reason = "held for RAII and drop ordering, not for reading")]
struct Graphics {
    swapchain: Swapchain,
    device: Arc<Device>,
    surface: Surface,
    // Last, so it outlives everything created from it — the safety condition
    // `window::create_surface` states and cannot enforce.
    window: slop_app::winit::window::Window,
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
    let window = window::create(
        event_loop,
        &WindowConfig {
            title: String::from("slop — window and surface"),
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;

    // Ordering is a Vulkan constraint: the instance must be created already
    // knowing which surface extensions this display needs, so the window comes
    // first.
    let extensions =
        window::required_instance_extensions(&window).map_err(|error| error.to_string())?;
    for extension in &extensions {
        println!(
            "  required instance extension: {}",
            extension.to_string_lossy()
        );
    }

    let instance = Instance::new(&InstanceConfig {
        application_name: String::from("example-window"),
        required_extensions: extensions,
        ..Default::default()
    })
    .map_err(|error| error.to_string())?;
    let instance = Arc::new(instance);

    // SAFETY: `window` is moved into the returned `Graphics` alongside the
    // surface, and is declared after it, so it outlives everything built here.
    let surface =
        unsafe { window::create_surface(&instance, &window) }.map_err(|error| error.to_string())?;

    // Enumerating *with* the surface is what makes present support part of
    // usability, rather than something discovered later at swapchain creation.
    let devices =
        slop_rhi::enumerate(&instance, Some(&surface)).map_err(|error| error.to_string())?;
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
        .map_err(|error| error.to_string())?;

    report(&devices, chosen, &surface);

    let device = Arc::new(Device::new(&instance, &devices[chosen]).map_err(|e| e.to_string())?);

    // `inner_size` is already in physical pixels, which is what a swapchain
    // needs. The logical size passed to WindowConfig is a different number on
    // any display that is not at 100% scaling.
    let size = window.inner_size();
    let swapchain = Swapchain::new(
        &device,
        &surface,
        &SwapchainConfig {
            present_mode: PresentMode::Mailbox,
            extent: slop_rhi::vk::Extent2D {
                width: size.width,
                height: size.height,
            },
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

    Ok(Graphics {
        swapchain,
        device,
        surface,
        window,
    })
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
