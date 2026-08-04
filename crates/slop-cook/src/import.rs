//! Reading a source asset and producing the cooked artifact for it.
//!
//! ```text
//! source asset  →  import  →  cook  →  runtime format
//! ```
//!
//! One module per source kind, each with the same shape: walk the tree for the
//! files it recognises, build a [`CacheKey`](slop_asset::CacheKey) from
//! everything that decides the artifact's bytes, skip whatever is already
//! current, and write the rest.
//!
//! # Why a directory
//!
//! `docs/CONVENTIONS.md` promotes a module to a directory when three or more
//! files share a subject that is already a name in the crate. Four do, and
//! "import" is the crate's own word for this stage — it is the second box in the
//! diagram above and in the crate's documentation. Before this they were
//! `shader_import.rs`, `gltf_import.rs` and friends at the crate root, where the
//! shared suffix was carrying the grouping that a directory should.
//!
//! What deliberately stayed outside: [`sources`](crate::sources),
//! [`geometry`](crate::geometry), [`reflection`](crate::reflection),
//! [`panorama`](crate::panorama) and [`cube`](crate::cube). None of them imports
//! anything — they are the concepts an importer is written in terms of, and
//! folding them in would make this a directory of "files to do with cooking",
//! which is the split-by-kind that the same rule bans.

pub(crate) mod environment;
pub(crate) mod gltf;
pub(crate) mod shader;
pub(crate) mod texture;

/// What a cook run did.
///
/// Every importer returns one and [`crate::all`] sums them, which is why it lives
/// here rather than with any one kind. It was defined in the shader importer for
/// as long as that was the only importer there was, and the other three reached
/// across for it — `use crate::shader_import::Summary` in a file about glTF being
/// the visible symptom of a type outliving the module it was named in.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Artifacts compiled this run.
    pub cooked: usize,
    /// Artifacts already up to date.
    pub skipped: usize,
}
