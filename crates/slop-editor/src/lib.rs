//! Tooling drawn on top of the game — `docs/DESIGN.md` §4's "egui-based
//! tooling", and §10.2's debug UI layer.
//!
//! Everything that puts an interface over a running engine lives here: the
//! immediate-mode overlay, the renderer that draws it, and the entity inspector.
//!
//! # Why this is not in `slop-app`
//!
//! It was, briefly, and that was a mistake worth recording. `slop-app` is the
//! crate **every game depends on** — main loop, device bring-up, configuration —
//! and it had no features, so a shipping game that wanted a window and a device
//! also linked egui, `egui-winit`, `slop-ecs` and `slop-reflect` whether it drew
//! an interface or not.
//!
//! Splitting them puts the cost where the benefit is. It also fixes the
//! direction of the dependency: `docs/DESIGN.md` §2.12 says the editor *embeds*
//! `slop-app` exactly as a game does, which cannot be true while the editor's
//! code is inside it.
//!
//! # Why the renderer half is here too
//!
//! [`Overlay`] was in `slop-render`, on the reasoning that it is a renderer and
//! knows nothing about windows. Both halves of that are still true — it takes
//! tessellated triangles and never touches an event loop, which is what lets the
//! headless golden tests drive it with no display at all.
//!
//! It lives here anyway, because "what draws the UI" and "what feeds the UI" are
//! one concern split across two types, and a renderer crate that carries a UI
//! backend is a renderer crate with an opinion about UI toolkits. `slop-render`
//! now depends on nothing egui-shaped.
//!
//! # The layering inside this crate
//!
//! - [`overlay`] is the renderer. No windowing, no egui context, no input.
//! - [`debug`] owns the egui context and the winit glue, and drives `overlay`.
//! - [`mod@inspector`] is a widget drawn *into* a `debug` UI, and knows about
//!   neither rendering nor windowing.
//!
//! That order is a strict dependency chain, and it is what keeps the headless
//! tests possible: they use `overlay` alone.
//!
//! It is also why the inspector is behind the `inspector` feature, off by
//! default. Being last in a strict chain means nothing else here needs it, and
//! it is the only part that needs `slop-ecs` and `slop-reflect` — so a frame
//! overlay on a triangle no longer links an ECS and a reflection system to draw
//! one. That is the same complaint this crate's existence answers, reproduced
//! one layer up.

pub mod debug;
#[cfg(feature = "inspector")]
pub mod inspector;
pub mod overlay;

pub use debug::{DebugUi, Declared};
#[cfg(feature = "inspector")]
pub use inspector::{InspectorState, inspector};
pub use overlay::Overlay;

/// Re-exported so consumers declare their interface with the same `egui` this
/// crate was built against.
///
/// Two versions of egui in one binary produce type mismatches that read as
/// nonsense — the same reasoning that re-exports `winit` from `slop-app`.
pub use egui;

use thiserror::Error;

/// Failures setting up or drawing the editor's interface.
#[derive(Debug, Error)]
pub enum EditorError {
    /// The overlay shader does not describe what the overlay writes.
    ///
    /// Reflection checks the shader side; the buffer formats are stated in
    /// [`overlay`]. This is the two disagreeing, or the bindless heap being
    /// full — both of which produce an interface that is present and wrong
    /// rather than an error, if left unchecked.
    #[error("the debug overlay could not be set up: {what}")]
    Layout {
        /// What was wrong.
        what: &'static str,
    },

    /// A cooked artifact the interface needs could not be read.
    ///
    /// Almost always means nothing has been cooked yet.
    ///
    /// Names the logical path and keeps the cause typed, which is
    /// [`slop_render::RenderError::Asset`]'s shape and was already the right one
    /// when this was written. It previously flattened a [`slop_asset::VfsError`]
    /// through `to_string()` — the same loss `CONVENTIONS.md` §6 rules out,
    /// except that §6 does not bite in an example and this is a library.
    ///
    /// Its `Display` also used to end with ``Run `cargo run -p slop-cli --
    /// cook` first``. A library crate naming a binary and an invocation is the
    /// §5.1 line — only the application layer knows how it was launched —
    /// crossed inside an error message, and it becomes wrong the moment
    /// `DESIGN.md` §2.12's editor cooks assets itself, which is the whole
    /// reason `slop-cook` was extracted. Telling a person what to run is the
    /// caller's business, and the caller can still do it: the logical path is
    /// right here on the variant.
    #[error("'{logical}' could not be read; it may not be cooked yet")]
    NotCooked {
        /// The logical path that was missing.
        logical: String,
        /// Why the read failed.
        #[source]
        source: slop_asset::VfsError,
    },

    /// The cooked bytes were read but are not what they claim to be.
    #[error("'{logical}' is cooked but malformed")]
    Malformed {
        /// The logical path.
        logical: String,
        /// What the decoder said.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A GPU object could not be created, or an upload failed.
    #[error(transparent)]
    Rhi(#[from] slop_rhi::RhiError),
}
