//! The debug UI — immediate mode, and both of its halves in one place.
//!
//! `docs/DESIGN.md` §10.2. A debug UI is re-declared every frame from current
//! state, which is what immediate mode means: there is no widget tree to keep
//! synchronised, so it cannot fall out of sync with the engine it reports on.
//!
//! # Two halves, one type
//!
//! The overlay is two things that must not be one. [`Overlay`]
//! is a renderer: it takes tessellated triangles and a texture atlas and draws
//! them, and it knows nothing about windows. What feeds it — reading window
//! events, turning them into egui input, handing egui's platform output back to
//! the window — is pure windowing glue.
//!
//! Keeping those as separate *types* is what lets the overlay be drawn by
//! something with no window at all, which is exactly what the cube's headless
//! golden test does: it drives `Overlay` directly, with no event loop and no
//! display.
//!
//! But every application wires them together identically — atlas into the
//! bindless heap, upload deltas before the frame opens, tessellate, draw last.
//! `DebugUi` is that wiring, and it is why an application needs four calls
//! rather than forty lines.
//!
//! # The one ordering rule
//!
//! [`DebugUi::run`] must be called **before** the frame is recorded, and
//! [`DebugUi::draw`] **inside** it. Uploading a texture waits on the GPU, and
//! nothing inside a recorded frame may block on it. Getting this wrong shows up
//! as a font atlas that is one frame late — which looks like the UI failing to
//! appear at all on the first frame, because the atlas *arrives* in the first
//! frame's delta.

use std::sync::Arc;

use slop_asset::Vfs;
use slop_render::Frame;
use slop_rhi::{Allocator, BindlessHeap, Device, Format, ShaderModule};
use winit::event::WindowEvent;
use winit::window::Window;

use crate::{EditorError, Overlay};

/// Where the overlay's cooked shader lives.
const SHADER: &str = "shaders/passes/overlay.spv";

/// Where its reflection lives.
const REFLECTION: &str = "shaders/passes/overlay.refl";

/// One frame's worth of declared UI, ready to draw.
///
/// Separate from [`DebugUi`] so that declaring the UI and drawing it are two
/// statements with the frame's recording between them — which is the ordering
/// the module docs describe, made visible in the types.
pub struct Declared {
    /// Tessellated triangles, in points rather than pixels.
    primitives: Vec<egui::ClippedPrimitive>,
    /// Logical pixels per point, from the display's scale factor.
    pixels_per_point: f32,
    /// Atlas changes egui made this frame.
    textures: egui::TexturesDelta,
}

impl std::fmt::Debug for Declared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Declared")
            .field("primitives", &self.primitives.len())
            .field("pixels_per_point", &self.pixels_per_point)
            .finish_non_exhaustive()
    }
}

/// The debug UI: egui's state, the winit glue, and the renderer that draws it.
pub struct DebugUi {
    overlay: Overlay,
    context: egui::Context,
    winit: egui_winit::State,
}

impl DebugUi {
    /// Build the debug UI, loading its shader through `vfs`.
    ///
    /// The overlay's font atlas is inserted into `heap`, so the heap must be the
    /// same one bound when [`draw`](Self::draw) is called.
    ///
    /// # Errors
    ///
    /// Fails if the overlay shader has not been cooked, if it is malformed, or
    /// if the renderer rejects it.
    pub fn new(
        window: &Window,
        device: &Arc<Device>,
        heap: &mut BindlessHeap,
        vfs: &Vfs,
        color_format: Format,
    ) -> Result<Self, EditorError> {
        let bytes = vfs.read(SHADER).map_err(|why| EditorError::NotCooked {
            what: String::from(SHADER),
            why: why.to_string(),
        })?;
        let module = ShaderModule::from_bytes(device, &bytes)?;

        let bytes = vfs.read(REFLECTION).map_err(|why| EditorError::NotCooked {
            what: String::from(REFLECTION),
            why: why.to_string(),
        })?;
        let reflection =
            slop_asset::Reflection::read(&bytes).map_err(|why| EditorError::Malformed {
                what: String::from(REFLECTION),
                why: why.to_string(),
            })?;

        let overlay = Overlay::new(device, heap, &module, &reflection, color_format)?;

        let context = egui::Context::default();
        let winit = egui_winit::State::new(
            context.clone(),
            context.viewport_id(),
            window,
            None,
            None,
            None,
        );

        Ok(Self {
            overlay,
            context,
            winit,
        })
    }

    /// Feed a window event to the UI.
    ///
    /// Returns whether egui consumed it. A caller that also reads input should
    /// respect that: a click that lands on a UI window must not also move the
    /// camera behind it.
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.winit.on_window_event(window, event).consumed
    }

    /// Declare this frame's UI and tessellate it.
    ///
    /// **Call before recording the frame**, not inside it — see the module docs.
    /// `declare` receives the context to build widgets on.
    ///
    /// `FnMut` rather than `FnOnce`, and that is not a technicality: egui may run
    /// the closure **more than once for a single frame**. A window whose size
    /// depends on its contents is measured on one pass and placed on the next, so
    /// a closure that could only run once would make self-sizing windows
    /// impossible. Anything expensive in here runs that many times.
    pub fn run(&mut self, window: &Window, mut declare: impl FnMut(&egui::Context)) -> Declared {
        let input = self.winit.take_egui_input(window);

        // The context is cloned into the closure rather than borrowed, because
        // `run_ui` holds its own borrow for the duration.
        let context = self.context.clone();
        let output = self.context.run_ui(input, |_| declare(&context));

        self.winit
            .handle_platform_output(window, output.platform_output);

        Declared {
            primitives: self
                .context
                .tessellate(output.shapes, output.pixels_per_point),
            pixels_per_point: output.pixels_per_point,
            textures: output.textures_delta,
        }
    }

    /// Upload this frame's atlas changes.
    ///
    /// Separate from [`draw`](Self::draw) and called **outside** the recorded
    /// frame, because uploading waits on the GPU.
    ///
    /// # Errors
    ///
    /// Fails if a texture upload fails.
    pub fn upload(
        &mut self,
        heap: &mut BindlessHeap,
        allocator: &Arc<Allocator>,
        declared: &Declared,
    ) -> Result<(), EditorError> {
        self.overlay
            .update_textures(heap, allocator, &declared.textures)?;

        Ok(())
    }

    /// Draw the UI into `frame`, in a pass of its own.
    ///
    /// Call **last**, after the scene: the overlay loads the colour attachment
    /// rather than clearing it, so it composites over whatever is already there.
    ///
    /// # Errors
    ///
    /// Fails if recording the overlay's draws fails.
    pub fn draw(
        &mut self,
        heap: &BindlessHeap,
        allocator: &Arc<Allocator>,
        frame: &Frame<'_>,
        declared: &Declared,
    ) -> Result<(), EditorError> {
        self.overlay.draw(
            heap,
            allocator,
            frame,
            &declared.primitives,
            declared.pixels_per_point,
        )?;

        Ok(())
    }
}

impl std::fmt::Debug for DebugUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DebugUi").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cooked_paths_are_the_ones_the_cooker_writes() {
        // Stated as constants in two crates. If the cooker's naming changes, the
        // failure is a runtime "not cooked" that reads like a missing `cook`
        // run, which sends someone down the wrong path entirely.
        assert!(SHADER.starts_with("shaders/passes/"));
        assert_eq!(
            SHADER.trim_end_matches(".spv"),
            REFLECTION.trim_end_matches(".refl"),
            "the shader and its reflection must name the same pass"
        );
    }

    #[test]
    fn a_missing_shader_says_what_to_run() {
        let failure = EditorError::NotCooked {
            what: String::from(SHADER),
            why: String::from("no such file"),
        };

        // The remedy belongs in the message: this is the first error a fresh
        // clone hits, and "no such file" alone does not say that a build step
        // was skipped.
        assert!(failure.to_string().contains("cook"), "{failure}");
    }
}
