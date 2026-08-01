//! Linear algebra and geometry for the Slop engine.
//!
//! Re-exports `glam` rather than wrapping it, and adds only the engine-specific
//! types that `glam` does not provide: [`Transform`], bounding volumes, frusta,
//! curves, and packing helpers. See `DESIGN.md` §4.
//!
//! Grown on demand — types land here when a consumer needs them, not before.
