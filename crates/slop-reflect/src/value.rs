//! A reflected value, decoupled from the memory holding it.
//!
//! # Why an intermediate representation at all
//!
//! Serialization has to go from *component memory* to *text* and back. Doing
//! that in one step means every format re-implements the reflection walk, and
//! `docs/DESIGN.md` §2.4 names five incompatible hand-written serializers as the
//! exact failure it wants avoided.
//!
//! So there are two steps and a [`Value`] between them:
//!
//! ```text
//! raw component bytes  ←──→  Value  ←──→  text
//!        (slop-ecs)                    (this crate)
//! ```
//!
//! The split is also where the `unsafe` lands. Reading a struct out of raw memory
//! by field offset is pointer arithmetic, and `docs/CONVENTIONS.md` §7 sanctions
//! that in `slop-ecs` and not here — so `slop-ecs` owns the left arrow and this
//! crate stays entirely safe. A second format later writes another right arrow
//! and touches neither the walk nor the `unsafe`.
//!
//! It buys two more things worth having. A property panel and an undo/redo diff
//! both want a value they can hold and compare without a live entity behind it.
//! And a value parsed from a file can be validated before anything is written
//! into the world.
//!
//! # Exactness
//!
//! One variant per primitive rather than a widened `Int(i128)` and
//! `Float(f64)`. A round-trip test that cannot tell `1.0f32` from `1.0f64` is not
//! testing round-tripping, and `u64::MAX` does not fit in an `i64`.

use crate::{Primitive, TypePath};

/// A value of a reflected type.
///
/// Structurally what the type's [`TypeKind`](crate::TypeKind) describes: one
/// variant per [`Primitive`], one for text, one for a struct.
#[derive(Debug, Clone)]
pub enum Value {
    /// `bool`.
    Bool(bool),
    /// `char`.
    Char(char),
    /// `u8`.
    U8(u8),
    /// `u16`.
    U16(u16),
    /// `u32`.
    U32(u32),
    /// `u64`.
    U64(u64),
    /// `usize`.
    Usize(usize),
    /// `i8`.
    I8(i8),
    /// `i16`.
    I16(i16),
    /// `i32`.
    I32(i32),
    /// `i64`.
    I64(i64),
    /// `isize`.
    Isize(isize),
    /// `f32`.
    F32(f32),
    /// `f64`.
    F64(f64),
    /// Owned text.
    String(String),
    /// A struct, with its fields in declaration order.
    Struct(Struct),
}

impl Value {
    /// Which primitive this is, if it is one.
    pub fn primitive(&self) -> Option<Primitive> {
        Some(match self {
            Self::Bool(_) => Primitive::Bool,
            Self::Char(_) => Primitive::Char,
            Self::U8(_) => Primitive::U8,
            Self::U16(_) => Primitive::U16,
            Self::U32(_) => Primitive::U32,
            Self::U64(_) => Primitive::U64,
            Self::Usize(_) => Primitive::Usize,
            Self::I8(_) => Primitive::I8,
            Self::I16(_) => Primitive::I16,
            Self::I32(_) => Primitive::I32,
            Self::I64(_) => Primitive::I64,
            Self::Isize(_) => Primitive::Isize,
            Self::F32(_) => Primitive::F32,
            Self::F64(_) => Primitive::F64,
            Self::String(_) | Self::Struct(_) => return None,
        })
    }

    /// The struct this holds, if it is one.
    pub fn as_struct(&self) -> Option<&Struct> {
        match self {
            Self::Struct(value) => Some(value),
            _ => None,
        }
    }

    /// A short description of what this is, for error messages.
    pub fn describe(&self) -> String {
        match self {
            Self::String(_) => "a string".to_owned(),
            Self::Struct(value) => format!("a `{}`", value.path()),
            _ => format!(
                "`{}`",
                self.primitive().expect("every other variant is one").name()
            ),
        }
    }
}

/// Equality that a round-trip test can rely on.
///
/// Floats compare **by bits**, with one exception: any NaN equals any other NaN.
///
/// Both halves are deliberate. Comparing by bits keeps `-0.0` distinct from
/// `0.0`, which `==` would not and which text does round-trip. Treating NaNs as
/// equal is the concession text forces — a NaN's payload bits do not survive
/// being written as `NaN` and read back, so a bitwise comparison would fail a
/// round-trip that is otherwise exact.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Char(left), Self::Char(right)) => left == right,
            (Self::U8(left), Self::U8(right)) => left == right,
            (Self::U16(left), Self::U16(right)) => left == right,
            (Self::U32(left), Self::U32(right)) => left == right,
            (Self::U64(left), Self::U64(right)) => left == right,
            (Self::Usize(left), Self::Usize(right)) => left == right,
            (Self::I8(left), Self::I8(right)) => left == right,
            (Self::I16(left), Self::I16(right)) => left == right,
            (Self::I32(left), Self::I32(right)) => left == right,
            (Self::I64(left), Self::I64(right)) => left == right,
            (Self::Isize(left), Self::Isize(right)) => left == right,
            (Self::F32(left), Self::F32(right)) => {
                (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
            }
            (Self::F64(left), Self::F64(right)) => {
                (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
            }
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Struct(left), Self::Struct(right)) => left == right,
            _ => false,
        }
    }
}

/// A struct value: which type, and what its fields hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Struct {
    path: TypePath,
    /// In the declaring type's field order, which is what a text file and a
    /// property panel both present.
    fields: Vec<(String, Value)>,
}

impl Struct {
    /// Build a struct value.
    pub fn new(path: impl Into<TypePath>, fields: Vec<(String, Value)>) -> Self {
        Self {
            path: path.into(),
            fields,
        }
    }

    /// Which type this is.
    pub fn path(&self) -> &TypePath {
        &self.path
    }

    /// The fields, in declaration order.
    pub fn fields(&self) -> &[(String, Value)] {
        &self.fields
    }

    /// One field by name.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    /// How many fields there are.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the struct has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_reports_which_primitive_it_is() {
        assert_eq!(Value::U32(1).primitive(), Some(Primitive::U32));
        assert_eq!(Value::F64(1.0).primitive(), Some(Primitive::F64));
        assert_eq!(Value::String(String::new()).primitive(), None);
    }

    #[test]
    fn one_bit_pattern_per_width_stays_distinct() {
        // The reason there is a variant per primitive rather than one widened
        // integer: these are different values and a round-trip test must be able
        // to tell them apart.
        assert_ne!(Value::U32(1), Value::U64(1));
        assert_ne!(Value::F32(1.0), Value::F64(1.0));
        assert_ne!(Value::I8(1), Value::U8(1));
    }

    #[test]
    fn negative_zero_is_not_zero() {
        // `==` on floats would say these are equal. Text round-trips the sign,
        // so the comparison has to notice it.
        assert_ne!(Value::F32(-0.0), Value::F32(0.0));
        assert_ne!(Value::F64(-0.0), Value::F64(0.0));
    }

    #[test]
    fn any_nan_equals_any_nan() {
        // The concession text forces: a NaN's payload does not survive being
        // written as `NaN`, so a bitwise comparison would fail an otherwise
        // exact round-trip.
        let quiet = f32::NAN;
        let negative = -f32::NAN;

        assert_ne!(quiet.to_bits(), negative.to_bits());
        assert_eq!(Value::F32(quiet), Value::F32(negative));
    }

    #[test]
    fn infinities_are_distinguished_by_sign() {
        assert_ne!(Value::F32(f32::INFINITY), Value::F32(f32::NEG_INFINITY));
        assert_eq!(Value::F32(f32::INFINITY), Value::F32(f32::INFINITY));
    }

    #[test]
    fn a_struct_finds_its_fields_by_name() {
        let value = Struct::new(
            "game::Position",
            vec![
                ("x".to_owned(), Value::F32(1.0)),
                ("y".to_owned(), Value::F32(2.0)),
            ],
        );

        assert_eq!(value.field("x"), Some(&Value::F32(1.0)));
        assert_eq!(value.field("z"), None);
        assert_eq!(value.len(), 2);
        assert_eq!(value.path().as_str(), "game::Position");
    }

    #[test]
    fn struct_fields_keep_declaration_order() {
        // A text file and a property panel both present them in this order, so
        // it must not be a set.
        let value = Struct::new(
            "game::Ordered",
            vec![
                ("z".to_owned(), Value::U8(1)),
                ("a".to_owned(), Value::U8(2)),
            ],
        );

        let names: Vec<&str> = value
            .fields()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();

        assert_eq!(names, vec!["z", "a"]);
    }

    #[test]
    fn two_structs_of_different_types_are_not_equal() {
        let left = Struct::new("game::A", vec![("x".to_owned(), Value::U8(1))]);
        let right = Struct::new("game::B", vec![("x".to_owned(), Value::U8(1))]);

        assert_ne!(Value::Struct(left), Value::Struct(right));
    }

    #[test]
    fn a_value_describes_itself_for_an_error_message() {
        assert_eq!(Value::U32(1).describe(), "`u32`");
        assert_eq!(Value::String(String::new()).describe(), "a string");
        assert_eq!(
            Value::Struct(Struct::new("game::Position", Vec::new())).describe(),
            "a `game::Position`"
        );
    }
}
