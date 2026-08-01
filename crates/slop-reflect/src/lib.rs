//! Runtime type information — `docs/DESIGN.md` §2.4.
//!
//! Serialization, the scene format, editor property panels, WASM binding
//! generation, network replication, undo/redo diffs, save games, and debug
//! inspectors are all *derived* from this one system. §2.4 calls skipping it the
//! single most common fatal mistake in from-scratch engines: it produces five
//! incompatible hand-written serializers and a rewrite around month 18.
//!
//! # The one decision everything follows from
//!
//! **Types are registrable at runtime, not only at compile time.**
//!
//! That is forced, not preferred. §2.3 runs gameplay as WASM modules, and a
//! guest declares `struct Inventory { … }` the host was never compiled against.
//! §2.12's editor is then expected to render a property panel for it. So
//! [`TypeInfo`] is a **value** — size, alignment, field offsets and drop
//! behaviour arrive as data — and the derive macro is a convenient front end for
//! host-native types rather than the mechanism.
//!
//! ```text
//! #[derive(Reflect)] struct Transform  ─┐
//!                                       ├─→ TypeInfo ─→ registry, ECS, editor, serializer
//! a guest module's exported type table ─┘
//! ```
//!
//! One data model, two front ends. Nothing downstream can tell which a type came
//! from.
//!
//! # How this differs from the Rust-conventional design
//!
//! `bevy_reflect` and most Rust reflection crates key a registry on
//! [`std::any::TypeId`], which requires a Rust type to exist. Engines that had
//! to support user-defined types at runtime all landed where this does — Godot's
//! `ClassDB` registers at runtime, Unity gets it from the CLR, and Unreal's
//! static `UCLASS` reflection needed `UBlueprintGeneratedClass` grafted alongside
//! it for exactly the case §2.4 describes. So this is the *engine*-conventional
//! route, taken because the Rust-conventional one cannot express the
//! requirement.
//!
//! Note also that `std::any::TypeId` is documented as unstable across
//! compilations, so anything written to a file needs a stable key regardless.
//! The runtime requirement promotes that key from secondary to primary rather
//! than inventing it — see [`TypePath`].
//!
//! # What this crate deliberately does not do
//!
//! No serializers. Reflection describes types; turning a described value into
//! JSON, a binary scene chunk, or a network packet is a separate concern that
//! consumes this one. Building them together is how a reflection system ends up
//! shaped by whichever format was written first.

mod info;
mod path;
mod registry;

pub use info::{FieldInfo, Transfer, TypeInfo, TypeKind};
pub use path::{TypeId, TypePath};
pub use registry::{RegistryError, TypeRegistry};

/// A host-native type that can describe itself.
///
/// The compile-time front end. Implemented by the derive macro for ordinary
/// Rust types; a type declared by a guest module has no Rust type and therefore
/// no impl, and supplies its [`TypeInfo`] as data instead.
///
/// # Safety
///
/// Implementors must return a [`TypeInfo`] whose layout is `Layout::new::<Self>()`
/// and whose drop function, if any, drops `Self` in place. The ECS allocates
/// columns by that layout and frees elements through that function, so a wrong
/// answer here is memory-unsafe. This is why hand-written impls should be rare —
/// the derive macro cannot get it wrong.
pub unsafe trait Reflect: 'static {
    /// Describe this type.
    fn type_info() -> TypeInfo
    where
        Self: Sized;
}

/// Implement [`Reflect`] for a primitive.
///
/// A macro rather than a blanket impl because each needs its own canonical path,
/// and those paths are an ABI: a guest module naming `f32` must resolve to the
/// same type the host means.
macro_rules! primitive {
    ($($type:ty),* $(,)?) => {
        $(
            // SAFETY: the layout is taken directly from the type, and no drop
            // function is installed — every type here is `Copy`.
            unsafe impl Reflect for $type {
                fn type_info() -> TypeInfo {
                    TypeInfo::new(
                        stringify!($type),
                        std::alloc::Layout::new::<$type>(),
                        Transfer::Blittable,
                        TypeKind::Primitive,
                    )
                }
            }
        )*
    };
}

primitive!(bool, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

/// Register every primitive.
///
/// Field types resolve through the registry, so a struct with an `f32` field is
/// unresolvable until `f32` is present. Every registry wants these, which is why
/// it is one call rather than eleven.
///
/// # Errors
///
/// Only if a primitive's path is already taken by a different definition, which
/// would mean something registered a type called `f32`.
pub fn register_primitives(registry: &mut TypeRegistry) -> Result<(), RegistryError> {
    registry.register_native::<bool>()?;
    registry.register_native::<u8>()?;
    registry.register_native::<u16>()?;
    registry.register_native::<u32>()?;
    registry.register_native::<u64>()?;
    registry.register_native::<i8>()?;
    registry.register_native::<i16>()?;
    registry.register_native::<i32>()?;
    registry.register_native::<i64>()?;
    registry.register_native::<f32>()?;
    registry.register_native::<f64>()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_describe_themselves_correctly() {
        let info = f32::type_info();

        assert_eq!(info.path().as_str(), "f32");
        assert_eq!(info.layout(), std::alloc::Layout::new::<f32>());
        assert_eq!(info.transfer(), Transfer::Blittable);
        assert!(info.drop_in_place().is_none());
        assert!(matches!(info.kind(), TypeKind::Primitive));
    }

    #[test]
    fn every_primitive_registers_and_resolves() {
        let mut registry = TypeRegistry::new();
        register_primitives(&mut registry).expect("a fresh registry");

        assert_eq!(registry.len(), 11);
        assert!(registry.get_by_path("f32").is_some());
        assert!(registry.get_by_path("bool").is_some());
        assert!(registry.unresolved_fields().is_empty());
    }

    #[test]
    fn primitive_layouts_are_what_rust_says_they_are() {
        // Guards the macro against being handed a type whose path and layout
        // disagree — `u64` registered with `u32`'s layout would make every
        // column holding one half the size it should be.
        let mut registry = TypeRegistry::new();
        register_primitives(&mut registry).expect("fresh");

        let size = |path: &str| registry.get_by_path(path).map(|info| info.layout().size());

        assert_eq!(size("bool"), Some(1));
        assert_eq!(size("u8"), Some(1));
        assert_eq!(size("u32"), Some(4));
        assert_eq!(size("f64"), Some(8));
        assert_eq!(size("i64"), Some(8));
    }

    #[test]
    fn registering_primitives_twice_is_harmless() {
        let mut registry = TypeRegistry::new();

        register_primitives(&mut registry).expect("first");
        register_primitives(&mut registry).expect("second");

        assert_eq!(registry.len(), 11);
    }
}
