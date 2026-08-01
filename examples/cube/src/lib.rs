//! The M0 exit criterion: a lit, textured, rotating cube.
//!
//! A library as well as a binary because the windowed demo and the headless
//! golden test render the same scene. Without this split they would be the same
//! two hundred lines written twice, and the copy the test exercises would drift
//! from the copy a human looks at — which is precisely the failure a golden
//! image is supposed to prevent.
//!
//! [`Scene`] renders into a colour target the caller supplies, and does not know
//! whether that came from a swapchain or from an offscreen image. That is the
//! smallest seam that lets both consumers exist, and it is roughly the shape
//! `slop-render` generalizes at M3 (`docs/PLAN.md` §4.1-D) — a renderer that
//! takes a target rather than owning a window.
//!
//! # What it exercises
//!
//! Everything M0 built, at once: staged vertex, index and texture uploads; the
//! bindless heap; depth testing under reverse-Z; push constants; and the
//! projection conventions. That is the point of the cube — `docs/PLAN.md` §4
//! calls it "deliberately unambitious; its job is integration, not looks".
//!
//! # Determinism
//!
//! Rotation is driven by a **frame counter**, never by a clock
//! (`docs/DESIGN.md` §2.14). Frame *n* looks identical on every run and every
//! machine, which is what makes a golden image of a moving object possible at
//! all. A wall-clock rotation would make this scene untestable.

pub mod mesh;

mod scene;

pub use scene::{PushConstants, Scene, Target};
