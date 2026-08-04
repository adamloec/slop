//! The renderer — `docs/DESIGN.md` §4.
//!
//! Three things: the loop that turns a swapchain into a stream of frames, the
//! [`Graph`] that derives the barriers between a frame's passes, and the passes
//! that draw a cooked model into HDR and resolve it. The rest of Stage A —
//! shadows, clustered lighting, IBL, the post stack — arrives across
//! `docs/PLAN.md` §9.5 E4–E7.
//!
//! ```ignore
//! let mut renderer = FrameRenderer::new(&device, &surface, size, &config)?;
//!
//! // Each iteration of the application's own loop:
//! if let Some(extent) = renderer.prepare(&surface, window_size)? {
//!     scene.resize(&allocator, extent)?;      // depth buffer, and anything else sized
//! }
//!
//! renderer.render(|frame| {
//!     let mut graph = Graph::new();
//!     let screen = graph.import(&Imported { name: "swapchain", .. });
//!
//!     graph.add(&RenderPass { name: "scene", color: Some((screen, load)), .. },
//!               |pass| scene.draw(pass));
//!
//!     graph.execute(frame.command);
//! })?;
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
mod graph;
mod hdr;
mod lighting;
mod mesh;
mod vertex;
mod view;

pub use error::RenderError;
pub use frame::{Frame, FrameOutcome, FrameRenderer, FrameRendererConfig, Target};
pub use graph::{
    BufferId, ComputePass, DepthTarget, Graph, ImageId, Imported, ImportedBuffer, RenderPass,
};
pub use hdr::{HdrTarget, Tonemap};
pub use lighting::cluster::{ClusterCamera, ClusterGrid, Clusters, sphere_touches_box};
pub use lighting::environment::{DirectionalLight, Environment, default_irradiance, irradiance_of};
pub use lighting::light::{Lights, PointLight};
pub use lighting::shadow::{
    CASCADES, CascadeFit, SPLIT_BLEND, ShadowConfig, Shadows, light_basis, splits,
};
pub use mesh::MeshRenderer;
pub use vertex::VertexBinding;
pub use view::{NO_CLUSTERS, NO_SHADOWS, View};

/// The format the HDR target uses, re-exported so a pipeline drawing into it
/// declares the same one rather than restating it.
pub use hdr::FORMAT as HDR_FORMAT;
