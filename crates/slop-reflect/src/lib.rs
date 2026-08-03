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
//! # Serialization stops at the value
//!
//! `docs/DESIGN.md` §4 gives this crate "serialization primitives", and the line
//! it draws is between a *value* and the *memory* holding one:
//!
//! ```text
//! raw component bytes  ←──→  Value  ←──→  text
//!        (slop-ecs)                    (this crate)
//! ```
//!
//! [`Value`] and the [text format](to_text) are here. Reading a struct out of
//! raw memory by field offset is not — that is pointer arithmetic, and
//! `docs/CONVENTIONS.md` §7 sanctions it in `slop-ecs`, so `slop-ecs` owns the
//! left arrow and this crate stays entirely safe.
//!
//! What the split buys is that a second format — binary for shipping, per §8
//! item 1 — writes another right arrow and touches neither the reflection walk
//! nor any `unsafe`. Building format and reflection together is how a reflection
//! system ends up shaped by whichever format was written first.

mod info;
mod path;
mod registry;
mod text;
mod value;

pub use info::{FieldInfo, Primitive, Transfer, TypeInfo, TypeKind};
pub use path::{TypeId, TypePath};
pub use registry::{RegistryError, TypeRegistry};
pub use text::{Reader, TextError, from_text, to_text, to_text_body};
pub use value::{Struct, Value};

/// Derive [`Reflect`] for a struct with named fields.
///
/// The compile-time front end. Layout, destructor and blittability are all
/// computed from the type rather than taken from the author — see the macro's
/// own documentation for why each one is.
#[cfg(feature = "derive")]
pub use slop_reflect_derive::Reflect;

/// A host-native type that can describe itself.
///
/// The compile-time front end. Implemented by the derive macro for ordinary
/// Rust types; a type declared by a guest module has no Rust type and therefore
/// no impl, and supplies its [`TypeInfo`] as data instead.
///
/// # Thread safety
///
/// `Send + Sync` is a supertrait rather than a bound applied where components
/// are stored, because the obligation belongs to the type and not to any one
/// container. `slop_ecs::Column` asserts both unconditionally — it holds
/// type-erased bytes and has no Rust type to ask — and defers the claim to
/// "the level above". This is that level.
///
/// Without the bound the deferral pointed nowhere: a hand-written impl over a
/// type holding an `Rc`, satisfying the layout contract below exactly, could be
/// inserted into a `World` and read by two systems in one batch, racing the
/// non-atomic refcount across `docs/DESIGN.md` §2.5's worker threads from
/// entirely safe caller code. An implementor who meets the stated contract must
/// not be able to produce UB, which is what made that an unsound contract rather
/// than a missing check.
///
/// Nothing in-tree ever tripped it, because every type reachable through
/// `#[derive(Reflect)]` happens to be `Send + Sync` already — which is exactly
/// why it was worth closing before §2.3's guest path makes runtime registration
/// the common case.
///
/// # Safety
///
/// Implementors must return a [`TypeInfo`] whose layout is `Layout::new::<Self>()`
/// and whose drop function, if any, drops `Self` in place. The ECS allocates
/// columns by that layout and frees elements through that function, so a wrong
/// answer here is memory-unsafe. This is why hand-written impls should be rare —
/// the derive macro cannot get it wrong.
pub unsafe trait Reflect: Send + Sync + 'static {
    /// This type's canonical path — see [`TypePath`].
    ///
    /// An associated constant rather than something read out of
    /// [`type_info`](Self::type_info), so that a field's id can be resolved
    /// without building a whole `TypeInfo` for it, and so that archetype
    /// signatures can be assembled in a `const` context.
    const PATH: &'static str;

    /// Whether this type's bytes can cross to a guest module.
    ///
    /// A constant, and **derived rather than declared**: the macro computes it
    /// from `#[repr(C)]`, [`std::mem::needs_drop`], and every field's own
    /// `TRANSFER`. All three are available in a `const` context, so the answer
    /// folds at compile time and a struct containing a `String` cannot claim to
    /// be blittable however it is annotated.
    const TRANSFER: Transfer;

    /// Describe this type.
    fn type_info() -> TypeInfo
    where
        Self: Sized;

    /// This type's id, without constructing a [`TypeInfo`].
    fn type_id() -> TypeId
    where
        Self: Sized,
    {
        TypeId::from_path(Self::PATH)
    }
}

/// Implement [`Reflect`] for a primitive.
///
/// A macro rather than a blanket impl because each needs its own canonical path,
/// and those paths are an ABI: a guest module naming `f32` must resolve to the
/// same type the host means.
macro_rules! primitive {
    ($($type:ty => $kind:ident),* $(,)?) => {
        $(
            // SAFETY: the layout is taken directly from the type, and no drop
            // function is installed — every type here is `Copy`.
            unsafe impl Reflect for $type {
                const PATH: &'static str = stringify!($type);
                const TRANSFER: Transfer = Transfer::Blittable;

                fn type_info() -> TypeInfo {
                    TypeInfo::new(
                        Self::PATH,
                        std::alloc::Layout::new::<$type>(),
                        Self::TRANSFER,
                        TypeKind::Primitive(crate::Primitive::$kind),
                    )
                }
            }
        )*
    };
}

primitive!(
    bool => Bool,
    char => Char,
    u8 => U8,
    u16 => U16,
    u32 => U32,
    u64 => U64,
    usize => Usize,
    i8 => I8,
    i16 => I16,
    i32 => I32,
    i64 => I64,
    isize => Isize,
    f32 => F32,
    f64 => F64,
);

// SAFETY: the layout is `String`'s own, and the drop function does nothing but
// drop a `String` in place through a correctly cast pointer.
//
// `Owning`, not `Blittable`: a `String` is a pointer into the host heap, so its
// bytes mean nothing inside a guest's linear memory. `Opaque` rather than a
// struct, because its three fields are private implementation detail — an editor
// shows a text box, not a capacity field.
unsafe impl Reflect for String {
    const PATH: &'static str = "std::string::String";
    const TRANSFER: Transfer = Transfer::Owning;

    fn type_info() -> TypeInfo {
        // SAFETY: as above.
        unsafe {
            TypeInfo::with_drop(
                Self::PATH,
                std::alloc::Layout::new::<Self>(),
                Self::TRANSFER,
                TypeKind::String,
                |pointer| std::ptr::drop_in_place(pointer.cast::<Self>()),
            )
        }
    }
}

/// Register every type this crate ships an implementation for.
///
/// Field types resolve through the registry, so a struct with an `f32` field is
/// unresolvable until `f32` is present. Every registry wants all of these, which
/// is why it is one call rather than fifteen.
///
/// Deliberately not automatic: a registry with nothing in it is a valid starting
/// point for a tool that only inspects a guest module's own types, and a
/// `Default` that silently populated itself would make that impossible to
/// express.
///
/// # Errors
///
/// Only if one of these paths is already taken by a different definition, which
/// would mean something registered its own type called `f32`.
pub fn register_builtins(registry: &mut TypeRegistry) -> Result<(), RegistryError> {
    registry.register_native::<bool>()?;
    registry.register_native::<char>()?;
    registry.register_native::<u8>()?;
    registry.register_native::<u16>()?;
    registry.register_native::<u32>()?;
    registry.register_native::<u64>()?;
    registry.register_native::<usize>()?;
    registry.register_native::<i8>()?;
    registry.register_native::<i16>()?;
    registry.register_native::<i32>()?;
    registry.register_native::<i64>()?;
    registry.register_native::<isize>()?;
    registry.register_native::<f32>()?;
    registry.register_native::<f64>()?;
    registry.register_native::<String>()?;

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
        assert!(matches!(info.kind(), TypeKind::Primitive(_)));
    }

    #[test]
    fn every_builtin_registers_and_resolves() {
        let mut registry = TypeRegistry::new();
        register_builtins(&mut registry).expect("a fresh registry");

        assert_eq!(registry.len(), 15);
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
        register_builtins(&mut registry).expect("fresh");

        let size = |path: &str| registry.get_by_path(path).map(|info| info.layout().size());

        assert_eq!(size("bool"), Some(1));
        assert_eq!(size("u8"), Some(1));
        assert_eq!(size("u32"), Some(4));
        assert_eq!(size("f64"), Some(8));
        assert_eq!(size("i64"), Some(8));
    }

    #[test]
    fn registering_builtins_twice_is_harmless() {
        let mut registry = TypeRegistry::new();

        register_builtins(&mut registry).expect("first");
        register_builtins(&mut registry).expect("second");

        assert_eq!(registry.len(), 15);
    }
}
