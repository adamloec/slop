//! Between a [`Value`] and the memory a component lives in.
//!
//! `slop-reflect` owns the right-hand arrow; this owns the left:
//!
//! ```text
//! raw component bytes  ←──→  Value  ←──→  text
//!        (here)                       (slop-reflect)
//! ```
//!
//! Reading a struct out of raw memory by field offset is pointer arithmetic, so
//! it lives in the crate that already does that rather than in the one that
//! describes types.
//!
//! # Check, then commit
//!
//! Writing is split into [`validate`] and [`write_value`], and the split is what makes
//! the write **infallible**.
//!
//! A struct written field by field can fail half way — and the fields already
//! written may own heap allocations, so unwinding means dropping exactly those
//! and no others, from a partially initialized value nothing else can describe.
//! Checking the whole value against the whole type first removes the case
//! entirely: by the time a byte is written, nothing left can go wrong.
//!
//! [`from_value`] does both and is the one to reach for. The halves are public
//! because a scene loader validates every component before touching the world,
//! so that a file with one bad field fails cleanly instead of leaving half a
//! scene loaded.

use std::alloc::Layout;

use slop_reflect::{Primitive, Struct, TypeInfo, TypeKind, TypeRegistry, Value};
use thiserror::Error;

/// Why a value and a type could not be reconciled.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueError {
    /// A type nothing can look inside.
    ///
    /// [`TypeKind::Opaque`] is the statement "its internals are the owning
    /// crate's business", so there is nothing to read out or write in. A
    /// component that must survive a save needs a describable kind.
    #[error("`{path}` is opaque, so its value cannot be read or written")]
    Opaque {
        /// The type that could not be described.
        path: String,
    },

    /// A field's type is not registered, so its layout is unknown.
    #[error("the type of field `{field}` on `{path}` is not registered")]
    UnregisteredField {
        /// The struct being walked.
        path: String,
        /// The field whose type could not be resolved.
        field: String,
    },

    /// The value is not of the shape the type describes.
    #[error("`{path}` expected {expected}, found {found}")]
    Mismatch {
        /// The type being written into.
        path: String,
        /// What the type wanted.
        expected: String,
        /// What the value held.
        found: String,
    },

    /// The value does not carry a field the type declares.
    #[error("`{path}` is missing field `{field}`")]
    MissingField {
        /// The struct being written.
        path: String,
        /// The field the value did not supply.
        field: String,
    },
}

/// Read a value out of component memory.
///
/// # Safety
///
/// `pointer` must point at an initialized, properly aligned value of exactly the
/// type `info` describes. It is only read — ownership stays with the caller, and
/// anything owning a heap allocation is cloned.
///
/// # Errors
///
/// [`ValueError::Opaque`] if the type or any field of it cannot be looked
/// inside, or [`ValueError::UnregisteredField`] if a field's type is absent from
/// the registry.
pub unsafe fn to_value(
    info: &TypeInfo,
    pointer: *const u8,
    registry: &TypeRegistry,
) -> Result<Value, ValueError> {
    match info.kind() {
        // SAFETY: the caller guarantees an initialized, aligned value of this
        // exact type, and the kind says which primitive it is.
        TypeKind::Primitive(primitive) => Ok(unsafe { read_primitive(*primitive, pointer) }),

        // SAFETY: as above. Cloned rather than moved, so the caller keeps
        // ownership of the original allocation.
        TypeKind::String => Ok(Value::String(unsafe {
            (*pointer.cast::<String>()).clone()
        })),

        TypeKind::Struct { fields } => {
            let mut read = Vec::with_capacity(fields.len());

            for field in fields {
                let field_info =
                    registry
                        .get(field.type_id)
                        .ok_or_else(|| ValueError::UnregisteredField {
                            path: info.path().to_string(),
                            field: field.name.clone(),
                        })?;

                // SAFETY: `field.offset` came from `offset_of!` on this exact
                // type, so the field is within the value and correctly aligned
                // for its own type.
                let value = unsafe { to_value(field_info, pointer.add(field.offset), registry) }?;

                read.push((field.name.clone(), value));
            }

            Ok(Value::Struct(Struct::new(info.path().clone(), read)))
        }

        TypeKind::Opaque => Err(ValueError::Opaque {
            path: info.path().to_string(),
        }),
    }
}

/// Read one scalar.
///
/// # Safety
///
/// As [`to_value`], with `primitive` naming the type at `pointer`.
unsafe fn read_primitive(primitive: Primitive, pointer: *const u8) -> Value {
    // SAFETY: the caller guarantees the pointer is an initialized, aligned value
    // of the named type, so each read is of exactly what is there.
    unsafe {
        match primitive {
            Primitive::Bool => Value::Bool(pointer.cast::<bool>().read()),
            Primitive::Char => Value::Char(pointer.cast::<char>().read()),
            Primitive::U8 => Value::U8(pointer.read()),
            Primitive::U16 => Value::U16(pointer.cast::<u16>().read()),
            Primitive::U32 => Value::U32(pointer.cast::<u32>().read()),
            Primitive::U64 => Value::U64(pointer.cast::<u64>().read()),
            Primitive::Usize => Value::Usize(pointer.cast::<usize>().read()),
            Primitive::I8 => Value::I8(pointer.cast::<i8>().read()),
            Primitive::I16 => Value::I16(pointer.cast::<i16>().read()),
            Primitive::I32 => Value::I32(pointer.cast::<i32>().read()),
            Primitive::I64 => Value::I64(pointer.cast::<i64>().read()),
            Primitive::Isize => Value::Isize(pointer.cast::<isize>().read()),
            Primitive::F32 => Value::F32(pointer.cast::<f32>().read()),
            Primitive::F64 => Value::F64(pointer.cast::<f64>().read()),
        }
    }
}

/// Check that `value` can be written as the type `info` describes.
///
/// Safe, and touches no memory. What [`write_value`] relies on to be infallible.
///
/// # Errors
///
/// [`ValueError::Mismatch`] if the shapes disagree, [`ValueError::MissingField`]
/// if the value omits a declared field, and as [`to_value`] otherwise.
pub fn validate(value: &Value, info: &TypeInfo, registry: &TypeRegistry) -> Result<(), ValueError> {
    let mismatch = |expected: String| ValueError::Mismatch {
        path: info.path().to_string(),
        expected,
        found: value.describe(),
    };

    match info.kind() {
        TypeKind::Primitive(primitive) => {
            if value.primitive() != Some(*primitive) {
                return Err(mismatch(format!("`{}`", primitive.name())));
            }
        }

        TypeKind::String => {
            if !matches!(value, Value::String(_)) {
                return Err(mismatch("a string".to_owned()));
            }
        }

        TypeKind::Struct { fields } => {
            let Some(structure) = value.as_struct() else {
                return Err(mismatch(format!("a `{}`", info.path())));
            };

            if structure.path() != info.path() {
                return Err(mismatch(format!("a `{}`", info.path())));
            }

            for field in fields {
                let Some(field_value) = structure.field(&field.name) else {
                    return Err(ValueError::MissingField {
                        path: info.path().to_string(),
                        field: field.name.clone(),
                    });
                };

                let field_info =
                    registry
                        .get(field.type_id)
                        .ok_or_else(|| ValueError::UnregisteredField {
                            path: info.path().to_string(),
                            field: field.name.clone(),
                        })?;

                validate(field_value, field_info, registry)?;
            }
        }

        TypeKind::Opaque => {
            return Err(ValueError::Opaque {
                path: info.path().to_string(),
            });
        }
    }

    Ok(())
}

/// Write `value` into uninitialized memory.
///
/// # Safety
///
/// Two obligations, and the second is the unusual one:
///
/// 1. `pointer` must be writable, correctly aligned, and **uninitialized** space
///    for the type `info` describes. Anything already there is overwritten
///    without being dropped.
/// 2. [`validate`] must have returned `Ok` for this exact `value`, `info` and
///    `registry`. It is what makes this infallible, and without it a struct can
///    stop half way with some fields initialized and no way to say which.
///
/// On return the memory holds a complete value, and the caller owns it.
pub unsafe fn write_value(
    value: &Value,
    info: &TypeInfo,
    pointer: *mut u8,
    registry: &TypeRegistry,
) {
    match info.kind() {
        // SAFETY: `validate` established the value is this primitive, and the
        // caller that the space is writable and aligned.
        TypeKind::Primitive(_) => unsafe { write_primitive(value, pointer) },

        TypeKind::String => {
            let Value::String(text) = value else {
                unreachable!("`validate` established this is a string")
            };

            // SAFETY: as above. The clone is what the destination owns; the
            // caller's `value` keeps its own.
            unsafe { pointer.cast::<String>().write(text.clone()) };
        }

        TypeKind::Struct { fields } => {
            let structure = value.as_struct().expect("`validate` established this");

            for field in fields {
                let field_value = structure
                    .field(&field.name)
                    .expect("`validate` established every field is present");
                let field_info = registry
                    .get(field.type_id)
                    .expect("`validate` established every field type resolves");

                // SAFETY: `field.offset` is within this type and aligned for the
                // field's own type, and every field is written exactly once —
                // which is what leaves the whole value initialized.
                unsafe {
                    write_value(field_value, field_info, pointer.add(field.offset), registry)
                };
            }
        }

        TypeKind::Opaque => unreachable!("`validate` rejects an opaque type"),
    }
}

/// Write one scalar.
///
/// # Safety
///
/// As [`write_value`].
unsafe fn write_primitive(value: &Value, pointer: *mut u8) {
    // SAFETY: `validate` established the value matches the destination type, and
    // the caller that the space is writable and aligned for it.
    unsafe {
        match value {
            Value::Bool(inner) => pointer.cast::<bool>().write(*inner),
            Value::Char(inner) => pointer.cast::<char>().write(*inner),
            Value::U8(inner) => pointer.write(*inner),
            Value::U16(inner) => pointer.cast::<u16>().write(*inner),
            Value::U32(inner) => pointer.cast::<u32>().write(*inner),
            Value::U64(inner) => pointer.cast::<u64>().write(*inner),
            Value::Usize(inner) => pointer.cast::<usize>().write(*inner),
            Value::I8(inner) => pointer.cast::<i8>().write(*inner),
            Value::I16(inner) => pointer.cast::<i16>().write(*inner),
            Value::I32(inner) => pointer.cast::<i32>().write(*inner),
            Value::I64(inner) => pointer.cast::<i64>().write(*inner),
            Value::Isize(inner) => pointer.cast::<isize>().write(*inner),
            Value::F32(inner) => pointer.cast::<f32>().write(*inner),
            Value::F64(inner) => pointer.cast::<f64>().write(*inner),
            other => unreachable!("`validate` rejects {other:?} for a primitive"),
        }
    }
}

/// Check and write in one step.
///
/// # Safety
///
/// As [`write_value`]'s first obligation: `pointer` must be writable, aligned, and
/// uninitialized space for `info`'s type. The second is discharged here.
///
/// **Nothing is written when this returns an error**, so the memory is still
/// uninitialized and the caller still owns nothing.
///
/// # Errors
///
/// As [`validate`].
pub unsafe fn from_value(
    value: &Value,
    info: &TypeInfo,
    pointer: *mut u8,
    registry: &TypeRegistry,
) -> Result<(), ValueError> {
    validate(value, info, registry)?;

    // SAFETY: the caller's obligation, plus the `validate` above.
    unsafe { write_value(value, info, pointer, registry) };

    Ok(())
}

/// Run `act` with uninitialized space for `layout`, then free it.
///
/// A zero-sized type gets a dangling-but-aligned pointer and no allocation,
/// which is what Rust expects of a pointer to a zero-sized value.
///
/// # Safety
///
/// `act` must leave the space in whatever state it found it, or take ownership
/// of what it wrote — nothing here drops anything.
pub(crate) unsafe fn with_scratch<R>(layout: Layout, act: impl FnOnce(*mut u8) -> R) -> R {
    if layout.size() == 0 {
        return act(std::ptr::without_provenance_mut(layout.align()));
    }

    // SAFETY: the size is non-zero, checked above.
    let buffer = unsafe { std::alloc::alloc(layout) };
    if buffer.is_null() {
        std::alloc::handle_alloc_error(layout);
    }

    let result = act(buffer);

    // SAFETY: `buffer` came from `alloc` with exactly this layout, and `act` has
    // either left it uninitialized or moved out whatever it wrote.
    unsafe { std::alloc::dealloc(buffer, layout) };

    result
}
