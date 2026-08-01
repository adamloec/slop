//! Where types are registered and looked up.
//!
//! One registry per world, held and passed explicitly. There is no global
//! registry, for the reason `docs/CONVENTIONS.md` §5 gives generally and one
//! specific to this crate: `docs/DESIGN.md` §2.12's editor loads several
//! projects at once, each with its own guest modules declaring their own types,
//! and a process-wide registry would make two projects' `Inventory` collide.
//!
//! # Registration is not idempotent by accident
//!
//! Registering the same type twice is fine and does nothing. Registering two
//! *different* types under one path, or two paths that hash to one id, is an
//! error rather than a last-write-wins overwrite. Both are situations where
//! continuing means the ECS silently treats one component as another.

use slop_core::FxHashMap;
use thiserror::Error;

use crate::{TypeId, TypeInfo, TypePath};

/// Why registration failed.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Two different types claim the same path.
    ///
    /// Usually a guest module declaring a type the host already has, or two
    /// modules that both chose `game::Item`. Neither can be resolved
    /// automatically: the engine cannot know which one a save file meant.
    #[error("type '{path}' is already registered with a different definition")]
    Conflict {
        /// The contested path.
        path: TypePath,
    },

    /// Two different paths hash to one [`TypeId`].
    ///
    /// Vanishingly unlikely and deliberately fatal. The alternative is two
    /// component types aliasing each other in every archetype signature, which
    /// would present as data corruption a long way from here. If this ever
    /// fires, rename one of the types — the id follows the path.
    #[error("'{path}' and '{existing}' both hash to {id}; rename one")]
    IdCollision {
        /// The path being registered.
        path: TypePath,
        /// The path already holding that id.
        existing: TypePath,
        /// The id they share.
        id: TypeId,
    },
}

/// The set of types an engine instance knows about.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    // Keyed on the id rather than the path: every hot lookup — archetype
    // signatures, component access — has an id in hand, and the path is only
    // needed when a human or a file is involved.
    types: FxHashMap<TypeId, TypeInfo>,
}

impl TypeRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a type.
    ///
    /// Registering the same definition twice succeeds and changes nothing,
    /// because a module loaded twice is normal and should not be an error.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Conflict`] if the path is taken by a different
    /// definition, [`RegistryError::IdCollision`] if two paths hash alike.
    pub fn register(&mut self, info: TypeInfo) -> Result<(), RegistryError> {
        if let Some(existing) = self.types.get(&info.id()) {
            if existing.path() != info.path() {
                return Err(RegistryError::IdCollision {
                    path: info.path().clone(),
                    existing: existing.path().clone(),
                    id: info.id(),
                });
            }

            if existing != &info {
                return Err(RegistryError::Conflict {
                    path: info.path().clone(),
                });
            }

            return Ok(());
        }

        self.types.insert(info.id(), info);

        Ok(())
    }

    /// Register a host-native type through its [`Reflect`](crate::Reflect) impl.
    ///
    /// # Errors
    ///
    /// As [`register`](Self::register).
    pub fn register_native<T: crate::Reflect>(&mut self) -> Result<(), RegistryError> {
        self.register(T::type_info())
    }

    /// Look up by id. The hot path.
    pub fn get(&self, id: TypeId) -> Option<&TypeInfo> {
        self.types.get(&id)
    }

    /// Look up by path, for deserialization and for anything a human typed.
    pub fn get_by_path(&self, path: &str) -> Option<&TypeInfo> {
        // The id is derived from the path, so this is a hash and one lookup
        // rather than a scan — but the result is confirmed against the path,
        // because an id collision must not resolve to the wrong type here
        // either.
        let info = self.types.get(&TypeId::from_path(path))?;

        (info.path().as_str() == path).then_some(info)
    }

    /// Whether a type is registered.
    pub fn contains(&self, id: TypeId) -> bool {
        self.types.contains_key(&id)
    }

    /// How many types are registered.
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Every registered type, in an order that does not vary between runs.
    ///
    /// Iteration order is reproducible because `FxHashMap` is fixed-seed
    /// (`docs/DESIGN.md` §2.14), but it is *arbitrary* — sorted by nothing in
    /// particular. Anything writing a file should sort by path; see
    /// [`sorted`](Self::sorted).
    pub fn iter(&self) -> impl Iterator<Item = &TypeInfo> {
        self.types.values()
    }

    /// Every registered type, ordered by path.
    ///
    /// For serialization, diagnostics, and anything a human reads. Reproducible
    /// iteration is enough for determinism; a *defined* order is what a file
    /// format needs.
    pub fn sorted(&self) -> Vec<&TypeInfo> {
        let mut all: Vec<&TypeInfo> = self.types.values().collect();
        all.sort_by(|left, right| left.path().cmp(right.path()));

        all
    }

    /// Resolve every field's type, reporting any that are not registered.
    ///
    /// A field names its type by id, and nothing forces that type to have been
    /// registered first — a guest module may declare `Inventory` before
    /// `ItemStack`. Rather than ordering registration, this checks afterward,
    /// which is what a module loader should call once its whole table is in.
    ///
    /// Returns the unresolved `(owner, field, missing type)` triples. Empty
    /// means the registry is closed under field references.
    pub fn unresolved_fields(&self) -> Vec<(&TypePath, &str, TypeId)> {
        let mut missing = Vec::new();

        for info in self.sorted() {
            for field in info.fields() {
                if !self.contains(field.type_id) {
                    missing.push((info.path(), field.name.as_str(), field.type_id));
                }
            }
        }

        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldInfo, Transfer, TypeKind};
    use std::alloc::Layout;

    fn primitive(path: &str) -> TypeInfo {
        TypeInfo::new(
            path,
            Layout::new::<f32>(),
            Transfer::Blittable,
            TypeKind::Primitive,
        )
    }

    #[test]
    fn a_registered_type_is_found_by_id_and_by_path() {
        let mut registry = TypeRegistry::new();
        registry.register(primitive("game::Health")).expect("fresh");

        let id = TypeId::from_path("game::Health");

        assert!(registry.contains(id));
        assert_eq!(
            registry.get(id).map(TypeInfo::path),
            registry.get_by_path("game::Health").map(TypeInfo::path)
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registering_the_same_definition_twice_is_not_an_error() {
        // A module loaded twice, or a type registered by two subsystems that
        // both depend on it. Erroring here would make registration order a
        // caller's problem for no benefit.
        let mut registry = TypeRegistry::new();

        registry.register(primitive("game::Health")).expect("first");
        registry
            .register(primitive("game::Health"))
            .expect("second");

        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn two_different_definitions_of_one_path_conflict() {
        // Last-write-wins here would mean a save file's `game::Health` silently
        // resolving to whichever module loaded second.
        let mut registry = TypeRegistry::new();
        registry.register(primitive("game::Health")).expect("first");

        let different = TypeInfo::new(
            "game::Health",
            Layout::new::<u64>(),
            Transfer::Blittable,
            TypeKind::Primitive,
        );

        assert!(matches!(
            registry.register(different),
            Err(RegistryError::Conflict { .. })
        ));
    }

    #[test]
    fn an_id_collision_is_reported_rather_than_aliased() {
        // Brute-forcing a real FNV-1a/64 collision is not feasible in a test, so
        // the situation is staged: store `game::First`'s definition under
        // `game::Second`'s id, which is exactly the state a collision produces.
        // Reaching into the private map is what makes the branch reachable at
        // all, and leaving it untested would mean the only guard against two
        // component types silently aliasing had never run.
        let mut registry = TypeRegistry::new();
        registry
            .types
            .insert(TypeId::from_path("game::Second"), primitive("game::First"));

        let error = registry
            .register(primitive("game::Second"))
            .expect_err("a colliding id must be rejected");

        assert!(matches!(error, RegistryError::IdCollision { .. }));
        // And the message names both types, because "hash collision" alone
        // leaves a reader with no idea which two to rename.
        let message = error.to_string();
        assert!(message.contains("game::Second"), "{message}");
        assert!(message.contains("game::First"), "{message}");
    }

    #[test]
    fn a_collision_does_not_overwrite_the_type_already_there() {
        // The failure mode being prevented: a save file's `game::First`
        // resolving to `game::Second`'s layout.
        let mut registry = TypeRegistry::new();
        registry
            .types
            .insert(TypeId::from_path("game::Second"), primitive("game::First"));

        let _ = registry.register(primitive("game::Second"));

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get(TypeId::from_path("game::Second"))
                .map(|info| info.path().as_str()),
            Some("game::First"),
            "the incumbent must survive"
        );
    }

    #[test]
    fn a_path_lookup_confirms_the_path_and_not_only_the_hash() {
        // `get_by_path` hashes and does one lookup, which would return the
        // wrong type on a collision if it did not then compare the path.
        let mut registry = TypeRegistry::new();
        registry.register(primitive("game::Health")).expect("fresh");

        assert!(registry.get_by_path("game::Health").is_some());
        assert!(registry.get_by_path("game::Healt").is_none());
        assert!(registry.get_by_path("").is_none());
    }

    #[test]
    fn sorted_iteration_is_by_path() {
        let mut registry = TypeRegistry::new();
        for path in ["z::Last", "a::First", "m::Middle"] {
            registry.register(primitive(path)).expect("fresh");
        }

        let paths: Vec<&str> = registry
            .sorted()
            .iter()
            .map(|info| info.path().as_str())
            .collect();

        assert_eq!(paths, vec!["a::First", "m::Middle", "z::Last"]);
    }

    #[test]
    fn iteration_order_is_reproducible() {
        // `DESIGN.md` §2.14. Two registries built identically must iterate
        // identically, or anything derived from registration order — a
        // generated binding table, a hash of the type set — differs per run.
        let build = || {
            let mut registry = TypeRegistry::new();
            for index in 0..64 {
                registry
                    .register(primitive(&format!("game::Type{index}")))
                    .expect("fresh");
            }
            registry
                .iter()
                .map(|info| info.path().clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(build(), build());
    }

    #[test]
    fn unresolved_field_types_are_reported() {
        // A guest module may declare a struct before the types of its fields,
        // so this is checked after the whole table is in rather than enforced
        // by ordering.
        let mut registry = TypeRegistry::new();
        registry
            .register(TypeInfo::new(
                "game::Inventory",
                Layout::new::<u64>(),
                Transfer::Blittable,
                TypeKind::Struct {
                    fields: vec![FieldInfo::new("stack", 0, TypeId::from_path("game::Stack"))],
                },
            ))
            .expect("fresh");

        let missing = registry.unresolved_fields();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].1, "stack");

        registry.register(primitive("game::Stack")).expect("fresh");

        assert!(registry.unresolved_fields().is_empty());
    }

    #[test]
    fn an_empty_registry_reports_itself_empty() {
        let registry = TypeRegistry::new();

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.unresolved_fields().is_empty());
    }
}
