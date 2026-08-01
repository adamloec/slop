//! What the engine knows about a type, as data.
//!
//! Everything here is a value rather than a generic parameter, and that is the
//! whole design (`docs/DESIGN.md` §2.4). A [`TypeInfo`] can be built by a derive
//! macro from a Rust type, or decoded from a table a WASM guest module exported,
//! and nothing downstream can tell which — the ECS allocates a column, the
//! editor draws a property panel, and the serializer walks fields, all from the
//! same struct.

use std::alloc::Layout;

use crate::{TypeId, TypePath};

/// How a type's bytes may be moved.
///
/// The distinction the columnar WASM boundary in `docs/DESIGN.md` §2.3 turns
/// on: a column may only be handed to a guest module if its contents mean the
/// same thing inside the guest's linear memory as they do in the host's heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// Plain bytes with no interior pointers, no padding that carries meaning,
    /// and no `Drop`. A `Vec3` or a `u32`.
    ///
    /// Only these can cross to a guest as raw memory, and only these can be
    /// bulk-copied between archetypes with `memcpy`.
    Blittable,
    /// Owns something a raw copy would alias — a heap allocation, a file
    /// handle, an `Arc`. A `String` or a `Vec<T>`.
    ///
    /// Movable within the host, since a Rust move is a bitwise copy that
    /// transfers ownership, but meaningless in a guest's address space.
    Owning,
}

impl Transfer {
    /// Whether these bytes mean the same thing outside this address space.
    ///
    /// `const` so that [`Reflect::TRANSFER`](crate::Reflect::TRANSFER) can be
    /// folded through nested structs at compile time — a struct is blittable
    /// only if every field is, and that has to be computable in a `const`
    /// context to cost nothing.
    pub const fn is_blittable(self) -> bool {
        matches!(self, Self::Blittable)
    }
}

/// A field within a struct.
///
/// The offset is the load-bearing part: it is what a property panel, a
/// serializer, and a guest module all need, and it is the only form a type
/// declared at runtime can supply. Accessor closures would be more ergonomic and
/// cannot be produced for a type nobody compiled against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    /// Field name, as declared.
    pub name: String,
    /// Byte offset from the start of the containing value.
    pub offset: usize,
    /// The field's own type, resolved through the registry.
    pub type_id: TypeId,
}

impl FieldInfo {
    /// Describe a field.
    pub fn new(name: impl Into<String>, offset: usize, type_id: TypeId) -> Self {
        Self {
            name: name.into(),
            offset,
            type_id,
        }
    }
}

/// The shape of a type.
///
/// Only what M1 needs. Enums, tuples, lists and maps each add a variant here
/// rather than a parallel type, so that a consumer's `match` fails to compile
/// when a new shape lands instead of silently ignoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    /// A scalar with no interior structure: `u32`, `f32`, `bool`.
    Primitive,
    /// A struct with named fields.
    Struct {
        /// In declaration order, which is also the order a property panel and a
        /// serializer present them.
        fields: Vec<FieldInfo>,
    },
    /// A type the engine can store and move but cannot look inside.
    ///
    /// The escape hatch for a component whose internals are the owning crate's
    /// business — the ECS can hold it, the editor shows only its name.
    Opaque,
}

/// Everything the engine knows about one type.
///
/// # Safety-relevant fields
///
/// [`layout`](Self::layout) and [`drop_in_place`](Self::drop_in_place) are
/// trusted by the ECS: a column allocates by the layout, and frees each element
/// through the drop function. A `TypeInfo` describing a type incorrectly is
/// memory-unsafe, which is why the constructor that accepts a drop function is
/// `unsafe` and the safe one produces no drop at all.
#[derive(Debug, Clone)]
pub struct TypeInfo {
    path: TypePath,
    id: TypeId,
    layout: Layout,
    // `None` means nothing to run — true of every primitive, every `Copy`
    // struct, and every type a guest module declares. The ECS treats `None` as
    // permission to forget an element rather than drop it, which is what makes
    // bulk archetype moves a `memcpy`.
    drop_in_place: Option<unsafe fn(*mut u8)>,
    transfer: Transfer,
    kind: TypeKind,
}

impl TypeInfo {
    /// Describe a type with no destructor.
    ///
    /// Safe, and the path a WASM guest module's type table takes: a guest
    /// declares plain data, so there is never a destructor to run and never a
    /// function pointer to trust.
    ///
    /// `layout` still has to be right — a wrong one produces wrong values and
    /// wasted memory — but it cannot produce a call through a bad function
    /// pointer, which is the difference between this and
    /// [`with_drop`](Self::with_drop).
    pub fn new(
        path: impl Into<TypePath>,
        layout: Layout,
        transfer: Transfer,
        kind: TypeKind,
    ) -> Self {
        let path = path.into();

        Self {
            id: path.id(),
            path,
            layout,
            drop_in_place: None,
            transfer,
            kind,
        }
    }

    /// Describe a type that owns resources and must be dropped.
    ///
    /// # Safety
    ///
    /// `drop_in_place` must be sound to call on any pointer to an initialized,
    /// properly aligned value of the type `layout` describes, and must be called
    /// at most once per value. In practice the only correct implementation is
    /// `|pointer| std::ptr::drop_in_place(pointer.cast::<T>())`, which is what
    /// the derive macro emits.
    ///
    /// `layout` must be `Layout::new::<T>()` for that same `T`. The ECS
    /// allocates and strides by it, so a mismatch is out-of-bounds access.
    pub unsafe fn with_drop(
        path: impl Into<TypePath>,
        layout: Layout,
        transfer: Transfer,
        kind: TypeKind,
        drop_in_place: unsafe fn(*mut u8),
    ) -> Self {
        let path = path.into();

        Self {
            id: path.id(),
            path,
            layout,
            drop_in_place: Some(drop_in_place),
            transfer,
            kind,
        }
    }

    /// The canonical path. This is what serialization writes.
    pub fn path(&self) -> &TypePath {
        &self.path
    }

    /// The cheap key derived from the path.
    pub fn id(&self) -> TypeId {
        self.id
    }

    /// Size and alignment.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// The destructor, if there is one.
    ///
    /// # Safety
    ///
    /// Calling the returned function requires everything
    /// [`with_drop`](Self::with_drop) requires. It is exposed because the ECS's
    /// storage layer is the one place that must call it.
    pub fn drop_in_place(&self) -> Option<unsafe fn(*mut u8)> {
        self.drop_in_place
    }

    /// Whether this type's bytes can cross to a guest module.
    pub fn transfer(&self) -> Transfer {
        self.transfer
    }

    /// Whether a value can be relocated with a plain byte copy and the source
    /// forgotten.
    ///
    /// True for both [`Transfer`] kinds, and stated as its own method because
    /// it is a different question from [`Transfer::Blittable`] and the two are
    /// easy to conflate. A Rust move *is* a bitwise copy — that is guaranteed,
    /// there are no move constructors — so an archetype can always relocate a
    /// component with `memcpy` regardless of whether it owns a heap allocation.
    /// What `Blittable` additionally promises is that the bytes still *mean*
    /// something outside this address space.
    pub fn is_relocatable(&self) -> bool {
        true
    }

    /// The type's shape.
    pub fn kind(&self) -> &TypeKind {
        &self.kind
    }

    /// The fields, or an empty slice for anything that is not a struct.
    pub fn fields(&self) -> &[FieldInfo] {
        match &self.kind {
            TypeKind::Struct { fields } => fields,
            TypeKind::Primitive | TypeKind::Opaque => &[],
        }
    }

    /// Find a field by name.
    pub fn field(&self, name: &str) -> Option<&FieldInfo> {
        self.fields().iter().find(|field| field.name == name)
    }
}

impl PartialEq for TypeInfo {
    /// Compares identity and layout, not function pointers.
    ///
    /// Two `TypeInfo` values for the same type must compare equal even when one
    /// came from a derive and the other from a module table, and function
    /// pointer equality is not something Rust guarantees is meaningful.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.path == other.path
            && self.layout == other.layout
            && self.transfer == other.transfer
            && self.drop_in_place.is_some() == other.drop_in_place.is_some()
            && self.kind == other.kind
    }
}

impl Eq for TypeInfo {}

#[cfg(test)]
mod tests {
    use super::*;

    fn struct_info() -> TypeInfo {
        TypeInfo::new(
            "game::Position",
            Layout::new::<[f32; 3]>(),
            Transfer::Blittable,
            TypeKind::Struct {
                fields: vec![
                    FieldInfo::new("x", 0, TypeId::from_path("f32")),
                    FieldInfo::new("y", 4, TypeId::from_path("f32")),
                    FieldInfo::new("z", 8, TypeId::from_path("f32")),
                ],
            },
        )
    }

    #[test]
    fn an_info_derives_its_id_from_its_path() {
        let info = struct_info();

        assert_eq!(info.id(), TypeId::from_path("game::Position"));
        assert_eq!(info.path().as_str(), "game::Position");
    }

    #[test]
    fn the_safe_constructor_never_produces_a_destructor() {
        // The property that makes the guest-module path safe: data arriving
        // from an untrusted source can describe a layout, but can never install
        // a function pointer the host will call.
        assert!(struct_info().drop_in_place().is_none());
    }

    #[test]
    fn fields_are_found_by_name_and_carry_offsets() {
        let info = struct_info();

        assert_eq!(info.fields().len(), 3);
        assert_eq!(info.field("y").map(|field| field.offset), Some(4));
        assert_eq!(info.field("w"), None);
    }

    #[test]
    fn non_structs_report_no_fields_rather_than_panicking() {
        let primitive = TypeInfo::new(
            "f32",
            Layout::new::<f32>(),
            Transfer::Blittable,
            TypeKind::Primitive,
        );

        assert!(primitive.fields().is_empty());
        assert_eq!(primitive.field("anything"), None);
    }

    #[test]
    fn a_type_with_a_destructor_reports_one() {
        // SAFETY: the drop function casts to `String`, which is the type
        // `layout` describes.
        let info = unsafe {
            TypeInfo::with_drop(
                "std::string::String",
                Layout::new::<String>(),
                Transfer::Owning,
                TypeKind::Opaque,
                |pointer| std::ptr::drop_in_place(pointer.cast::<String>()),
            )
        };

        assert!(info.drop_in_place().is_some());
        assert_eq!(info.transfer(), Transfer::Owning);
    }

    #[test]
    fn the_destructor_actually_runs_the_right_one() {
        // Not a formality: this is the mechanism the ECS frees every component
        // through, and a `TypeInfo` whose drop function does not match its
        // layout is a leak at best.
        use std::rc::Rc;

        let witness = Rc::new(());
        let mut value = Some(Rc::clone(&witness));

        assert_eq!(Rc::strong_count(&witness), 2);

        // SAFETY: the drop function casts to `Option<Rc<()>>`, matching the
        // layout, and `value` is a live, aligned, initialized value of it.
        unsafe {
            let info = TypeInfo::with_drop(
                "test::OptionRc",
                Layout::new::<Option<Rc<()>>>(),
                Transfer::Owning,
                TypeKind::Opaque,
                |pointer| std::ptr::drop_in_place(pointer.cast::<Option<Rc<()>>>()),
            );

            let drop_fn = info.drop_in_place().expect("a destructor was installed");
            drop_fn(std::ptr::from_mut(&mut value).cast::<u8>());
            // `value` is now logically uninitialized; forget it so the scope
            // end does not drop it a second time.
            std::mem::forget(value);
        }

        assert_eq!(
            Rc::strong_count(&witness),
            1,
            "the destructor should have released the clone"
        );
    }

    #[test]
    fn equality_ignores_the_function_pointer_itself() {
        // Two descriptions of one type, from a derive and from a module table,
        // must agree. Rust does not guarantee function pointer equality is
        // meaningful, so only its presence is compared.
        assert_eq!(struct_info(), struct_info());
    }

    #[test]
    fn types_differing_in_layout_are_not_equal() {
        let wide = TypeInfo::new(
            "game::Position",
            Layout::new::<[f64; 3]>(),
            Transfer::Blittable,
            TypeKind::Struct { fields: Vec::new() },
        );

        assert_ne!(struct_info(), wide);
    }
}
