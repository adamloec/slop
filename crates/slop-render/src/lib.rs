//! The renderer — `docs/DESIGN.md` §4.
//!
//! Today it owns one thing: the loop that turns a swapchain into a stream of
//! frames. The render graph, the material system and the passes that make up
//! Stage A arrive at M3 (`docs/PLAN.md` §9).
//!
//! ```ignore
//! let mut renderer = FrameRenderer::new(&device, &surface, size, &config)?;
//!
//! // Each iteration of the application's own loop:
//! if let Some(extent) = renderer.prepare(&surface, window_size)? {
//!     scene.resize(&allocator, extent)?;      // depth buffer, and anything else sized
//! }
//! renderer.render(|frame| scene.record(frame.command, frame.target, frame.number))?;
//! ```
//!
//! # It is not a framework
//!
//! There is no `run()` here, and no trait to implement. `docs/DESIGN.md` §1.2
//! principle 4 is explicit that the engine supplies pieces rather than a shape
//! to sit inside, so the event loop stays in the application, where the platform
//! already put it. What this crate owns is the part that is genuinely hard to
//! get right — swapchain lifetime, frames in flight, and the synchronisation
//! between them.
//!
//! Two calls rather than one, and the split is deliberate. Resources that must
//! match the target — a depth buffer, most obviously — have to be resized
//! *before* the frame that uses them is recorded, so [`FrameRenderer::prepare`]
//! reports a new size and [`FrameRenderer::render`] consumes it. Folding them
//! together would either hide the resize or hand back a frame already recorded
//! against a stale attachment.
//!
//! # Where this came from
//!
//! This loop existed twice before this crate did, copied between
//! `examples/cube` and `examples/triangle`, and `docs/PLAN.md` §6.1 recorded a
//! third copy as the signal to extract it. It was **rewritten** against those
//! two rather than lifted from them (§9.1): they were debugged into working
//! against real validation output and are trustworthy about *what the loop must
//! do*, while being example-grade about everything else. Three things did not
//! survive:
//!
//! - `Result<(), String>` throughout, where a library owes typed errors
//!   (`docs/CONVENTIONS.md` §6).
//! - `FRAMES_IN_FLIGHT` as a `const`, which is a caller's decision.
//! - `present.unwrap_or(graphics)`, which silently violates the spec on a device
//!   whose queue families differ — now [`RenderError::NoPresentQueue`].
//!
//! The two golden images are what say the rewrite is equivalent: both examples
//! render through this crate and neither reference moved.

mod error;
mod frame;
mod mesh;
mod overlay;
mod vertex;

pub use error::RenderError;
pub use frame::{Frame, FrameOutcome, FrameRenderer, FrameRendererConfig, Target};
pub use mesh::MeshRenderer;
pub use overlay::Overlay;
pub use vertex::VertexBinding;
