//! Proves the window → surface → device chain connects end to end.
//!
//! This is M0 task E's exit criterion in executable form. It opens a window,
//! creates a Vulkan instance with the surface extensions that window's display
//! requires, builds a surface from it, selects an adapter that can actually
//! present to that surface, and creates a logical device. Then it reports what
//! it found and exits — there is nothing to draw yet, and a window that lingers
//! with nothing in it would be less informative than the log.
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
use slop_rhi::{DeviceInfo, Surface};

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
    device: Device,
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

        // The chain is proven; there is nothing to render yet.
        event_loop.exit();
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

    let device = Device::new(&instance, &devices[chosen]).map_err(|error| error.to_string())?;

    println!("\nwindow, surface and device all created successfully.");

    Ok(Graphics {
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
