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
    /// Plain bytes with **no padding**, no interior pointers, and no `Drop`.
    /// A `Vec3` or a `u32`.
    ///
    /// Only these can cross to a guest as raw memory, and only these can be
    /// bulk-copied between archetypes with `memcpy`.
    ///
    /// # Padding is part of the claim, not a detail
    ///
    /// A `#[repr(C)] struct { a: u8, b: u32 }` is eight bytes holding five bytes
    /// of fields. Nobody ever writes the other three, so they hold whatever was
    /// in that memory before. Reading them is undefined, and `Column::as_bytes`
    /// reads the whole array — so a padded type declared blittable both is
    /// undefined behaviour and leaks host memory into a guest.
    ///
    /// Two paths reach this enum, and they are trusted differently:
    ///
    /// | Path | How the claim is checked |
    /// |---|---|
    /// | `#[derive(Reflect)]` | The derive refuses `Blittable` unless the struct is exactly as large as its fields. A Rust component cannot get this wrong. |
    /// | [`TypeInfo::new`] by hand, or from a guest's type table | **Not checked here.** `TypeInfo::new` is safe and takes the author's word. |
    ///
    /// The second is audited by
    /// [`TypeRegistry::padded_blittable`](crate::TypeRegistry::padded_blittable),
    /// which a module loader should call once a guest's whole table is in. That
    /// is the right place for it: a guest's declaration is the input that should
    /// not be trusted, and it is not fully checkable until every field type it
    /// references has been registered.
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

/// Which scalar a [`TypeKind::Primitive`] is.
///
/// A closed set. Unlike a struct, a primitive cannot be declared by a guest
/// module — a guest's `i32` is *this* `i32`, because agreeing on what an `i32`
/// is is the precondition for agreeing on anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    /// `bool`, one byte holding `0` or `1`.
    Bool,
    /// `char`, a Unicode scalar value.
    Char,
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `usize`. **Platform-dependent width** — eight bytes on the x86-64 that
    /// `docs/DESIGN.md` §2.14 scopes to, and a portability hazard in anything
    /// written to a file. Prefer a fixed width in a component that will be
    /// serialized.
    Usize,
    /// `i8`.
    I8,
    /// `i16`.
    I16,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `isize`. Platform-dependent, as [`Usize`](Self::Usize).
    Isize,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
}

impl Primitive {
    /// Whether this is a floating-point type.
    ///
    /// Text output distinguishes them: an `f32` holding a whole number is
    /// written `1.0` rather than `1`, so a human reading the file can tell the
    /// field is a float without consulting the type.
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    /// The Rust name, which is also the registered [`TypePath`].
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Char => "char",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Usize => "usize",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::Isize => "isize",
            Self::F32 => "f32",
            Self::F64 => "f64",
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
    /// A scalar with no interior structure, and **which** scalar it is.
    ///
    /// The payload is not decoration. A serializer handed a four-byte primitive
    /// cannot tell `u32` from `f32`, and a property panel cannot choose a widget
    /// — the layout is identical and only the interpretation differs. Anything
    /// reading these bytes needs to be told.
    Primitive(Primitive),
    /// Owned UTF-8 text.
    ///
    /// Its own variant rather than a [`Primitive`]: it owns a heap allocation,
    /// its length is not in its layout, and it is never
    /// [`Blittable`](Transfer::Blittable). What makes it a *kind* rather than an
    /// [`Opaque`](Self::Opaque) is that its contents are fully describable — a
    /// serializer can write it and read it back, which is exactly what `Opaque`
    /// means it cannot do.
    String,
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
            TypeKind::Primitive(_) | TypeKind::String | TypeKind::Opaque => &[],
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

impl TypeInfo {
    /// A hash of this type's **memory layout**, for checking that a separately
    /// compiled module agrees about it.
    ///
    /// # The problem this exists for
    ///
    /// `docs/DESIGN.md` §2.3 hands a guest module a column of raw bytes and lets
    /// it iterate them as its own struct. Nothing in that exchange checks the two
    /// sides agree about what the struct *is*. If the host has
    /// `Position { x, y, z }` and the guest was compiled against a version with a
    /// fourth field, the guest reads adjacent entities' data as its own — no
    /// crash, no error, just wrong numbers a long way from the cause.
    ///
    /// The [`TypeId`](crate::TypeId) cannot catch it: it hashes the *path*, which
    /// is exactly what stays the same across a version skew. Identity and layout
    /// are different questions, and this answers the second.
    ///
    /// A loader compares the guest's declared fingerprint against the host's and
    /// refuses the module on a mismatch — a startup error naming the type,
    /// instead of corruption at runtime.
    ///
    /// # What it covers
    ///
    /// Size, alignment, transfer, kind, and every field's name, offset and type
    /// id — everything a reader of these bytes depends on. Deliberately **not**
    /// the path: the path is the identity under which two fingerprints are
    /// compared, so folding it in would only ever compare a type to itself.
    ///
    /// Field types are covered by id rather than by their own fingerprint, so a
    /// full check fingerprints every type in the table. That is what
    /// [`TypeRegistry::fingerprint`](crate::TypeRegistry::fingerprint) is for,
    /// and it is why the two exist as a pair.
    ///
    /// FNV-1a, for the same reason [`TypeId`](crate::TypeId) uses it: a guest in
    /// any language must be able to reproduce it from a written specification.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = crate::path::FNV_OFFSET;

        let mut eat = |value: u64| {
            for byte in value.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(crate::path::FNV_PRIME);
            }
        };

        eat(self.layout.size() as u64);
        eat(self.layout.align() as u64);
        eat(match self.transfer {
            Transfer::Blittable => 1,
            Transfer::Owning => 2,
        });

        match &self.kind {
            TypeKind::Primitive(primitive) => {
                eat(1);
                eat(*primitive as u64);
            }
            TypeKind::String => eat(4),
            TypeKind::Opaque => eat(2),
            TypeKind::Struct { fields } => {
                eat(3);
                eat(fields.len() as u64);

                for field in fields {
                    for byte in field.name.as_bytes() {
                        eat(u64::from(*byte));
                    }
                    eat(field.offset as u64);
                    eat(field.type_id.to_bits());
                }
            }
        }

        hash
    }
}

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
            TypeKind::Primitive(Primitive::F32),
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
