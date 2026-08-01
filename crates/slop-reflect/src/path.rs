//! Type identity that survives a rebuild, a process, and a file.
//!
//! # Why not `std::any::TypeId`
//!
//! Two independent reasons, either of which alone would be enough.
//!
//! **It cannot name a runtime type.** `docs/DESIGN.md` §2.4 requires types that
//! arrive as *data* — a WASM guest declaring `struct Inventory` the host was
//! never compiled against. `TypeId::of::<Inventory>()` cannot be written,
//! because there is no `Inventory` in the host's type system.
//!
//! **It is not stable across compilations.** The standard library documents
//! this explicitly. Rebuild the engine and every `TypeId` may change, so
//! anything written to a scene file, a save game, or a network packet keyed on
//! one is unreadable by the next build. A purely compile-time reflection system
//! would still need a stable key for those; the runtime requirement only
//! promotes it from secondary to primary.
//!
//! # What identity is, here
//!
//! A [`TypePath`] — `"slop_math::Transform"` — is the truth. It is what gets
//! written to files, what a guest module declares, and what a human reads in an
//! error message.
//!
//! A [`TypeId`] is a 64-bit hash of that path, and exists only as a cheap `Copy`
//! key for hash maps and archetype signatures. **Nothing durable is keyed on
//! it.** Serialization writes the path, so a hash collision can never corrupt a
//! file — it can only be an in-memory ambiguity, which [`crate::TypeRegistry`]
//! rejects at registration rather than tolerating.

use std::fmt;

/// A type's canonical path, unique across the engine and every loaded module.
///
/// Conventionally the Rust module path — `slop_math::Transform` — but nothing
/// enforces that, because a guest module's types have no Rust path. What matters
/// is that it is stable for the life of any data written against it: renaming a
/// type is a migration, not a refactor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypePath(String);

impl TypePath {
    /// Wrap a path string.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// The full path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The final segment — `Transform` from `slop_math::Transform`.
    ///
    /// For display only. Two modules may perfectly well both define a
    /// `Transform`, so this is never an identity.
    pub fn short_name(&self) -> &str {
        self.0.rsplit("::").next().unwrap_or(&self.0)
    }

    /// The stable id derived from this path.
    pub fn id(&self) -> TypeId {
        TypeId::from_path(&self.0)
    }
}

impl fmt::Display for TypePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TypePath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<String> for TypePath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

/// A cheap `Copy` key derived from a [`TypePath`].
///
/// Reproducible: the same path yields the same id on every platform, in every
/// build, forever. That is a promise the test at the bottom of this file pins,
/// because changing the hash silently invalidates every archetype signature and
/// every cached lookup in a running editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(u64);

/// FNV-1a's 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a's 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl TypeId {
    /// Derive an id from a path.
    ///
    /// FNV-1a, chosen for being trivially specifiable — thirty characters of
    /// arithmetic that a guest module in any language can reproduce exactly.
    /// That matters more here than hash quality: a WASM module written in Zig
    /// must be able to compute the same id the host does, and "call blake3" is
    /// a heavier ask than "multiply and xor".
    ///
    /// Collision risk is not managed by hash strength but by detection —
    /// [`crate::TypeRegistry`] refuses two different paths mapping to one id, so
    /// the failure is a loud startup error rather than a silent aliasing of two
    /// component types.
    pub const fn from_path(path: &str) -> Self {
        let bytes = path.as_bytes();
        let mut hash = FNV_OFFSET;
        let mut index = 0;

        while index < bytes.len() {
            hash ^= bytes[index] as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
            index += 1;
        }

        Self(hash)
    }

    /// The raw value, for storage and transport across the WASM boundary.
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Rebuild an id from its raw value.
    ///
    /// Deliberately not validated: there is nothing to validate against without
    /// a registry. Treat the result as a claim to be resolved by lookup, not as
    /// proof the type exists — the same posture `slop_core::Handle::from_raw`
    /// takes for the same reason.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_path_always_gives_the_same_id() {
        assert_eq!(
            TypeId::from_path("slop_math::Transform"),
            TypeId::from_path("slop_math::Transform")
        );
    }

    #[test]
    fn different_paths_give_different_ids() {
        assert_ne!(
            TypeId::from_path("slop_math::Transform"),
            TypeId::from_path("slop_math::Velocity")
        );
        // Same short name, different module — the case `short_name` is
        // explicitly not an identity for.
        assert_ne!(
            TypeId::from_path("game::Transform"),
            TypeId::from_path("slop_math::Transform")
        );
    }

    #[test]
    fn the_hash_is_pinned() {
        // The promise: a guest module written in another language computes the
        // same id, and an archetype signature cached in an editor session stays
        // valid. Changing the algorithm breaks both, so it must break this test
        // first.
        //
        // These are FNV-1a/64 of the given strings, verifiable against any
        // other implementation.
        assert_eq!(TypeId::from_path("").to_bits(), 0xcbf2_9ce4_8422_2325);
        assert_eq!(TypeId::from_path("a").to_bits(), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(TypeId::from_path("foobar").to_bits(), 0x85944171f73967e8);
    }

    #[test]
    fn ids_can_round_trip_through_raw_bits() {
        // The WASM boundary hands these across as plain integers.
        let id = TypeId::from_path("game::Inventory");

        assert_eq!(TypeId::from_bits(id.to_bits()), id);
    }

    #[test]
    fn a_path_reports_its_short_name() {
        assert_eq!(
            TypePath::new("slop_math::Transform").short_name(),
            "Transform"
        );
        assert_eq!(TypePath::new("Transform").short_name(), "Transform");
        assert_eq!(TypePath::new("").short_name(), "");
    }

    #[test]
    fn a_path_and_its_id_agree() {
        let path = TypePath::new("game::Inventory");

        assert_eq!(path.id(), TypeId::from_path("game::Inventory"));
    }

    #[test]
    fn the_id_is_usable_in_a_const() {
        // `from_path` is `const` so archetype signatures and static tables can
        // be built at compile time. This fails to compile if that regresses.
        const TRANSFORM: TypeId = TypeId::from_path("slop_math::Transform");

        assert_eq!(TRANSFORM, TypeId::from_path("slop_math::Transform"));
    }
}
