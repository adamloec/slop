//! The text format: [`Value`] to a string and back.
//!
//! `docs/DESIGN.md` §8 item 1 asked for a text format for diffability during
//! development, with binary for shipping, both derived from reflection — and
//! left the choice between RON, TOML and custom open. This is that choice.
//!
//! ```text
//! game::Transform {
//!     position: game::Vec3 {
//!         x: 1.0,
//!         y: 0.0,
//!         z: -2.5,
//!     },
//!     name: "player spawn",
//!     visible: true,
//! }
//! ```
//!
//! # Why a hand-written format
//!
//! **The value model is dynamic and `serde` is static.** `serde` is built around
//! `T: Deserialize` known at compile time; §2.4 gives us a [`TypeInfo`] known at
//! runtime. Bridging that means `DeserializeSeed` and a good deal of machinery to
//! make a static framework accept a dynamic schema — the same impedance mismatch
//! that made `Column<T>` impossible and produced a type-erased `Column` instead.
//!
//! The grammar we actually need is closed and small: scalars, text, and nested
//! named structs. A recursive-descent parser over that is a few hundred lines of
//! safe code whose failure modes are exactly what a round-trip property test
//! catches. That is a different trade from the one in `slop-core`'s job pool,
//! where the third-party crate was taken — there the alternative was unsafe
//! concurrent code whose failures tests do *not* catch.
//!
//! # Reading is schema-driven
//!
//! [`from_text`] takes the [`TypeInfo`] it expects, so the file needs no type
//! suffixes: a field declared `f32` reads `1.0`, not `1.0f32`. That is what keeps
//! the format diffable, which was the stated reason for having one.
//!
//! It also means an unknown type is an error rather than something preserved.
//! That matches the registry refusing a conflicting registration rather than
//! taking the last write: a scene naming a component the engine does not have is
//! a wiring problem, and silently dropping it would corrupt the scene on the next
//! save.
//!
//! # What round-trips exactly
//!
//! Every integer, `bool`, `char`, and string. Floats round-trip **bit for bit**,
//! including `-0.0` and both infinities, because Rust's shortest-round-trip
//! formatting is exact by construction.
//!
//! The one exception is a NaN's payload: every NaN is written `NaN` and reads
//! back as the canonical one. [`Value`]'s equality accounts for that by treating
//! any NaN as equal to any other.

use std::fmt::Write as _;

use crate::{Primitive, Struct, TypeInfo, TypeKind, TypeRegistry, Value};

/// Spaces per nesting level.
const INDENT: usize = 4;

/// Why text could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}, column {column}: {message}")]
pub struct TextError {
    /// One-based line number.
    pub line: usize,
    /// One-based column number.
    pub column: usize,
    /// What went wrong.
    pub message: String,
}

/// Write a value as text.
///
/// The result parses back to an equal value through [`from_text`], given the same
/// [`TypeInfo`].
///
/// Infallible, and that is a property of [`Value`] rather than an oversight: it
/// has a variant for every describable kind and none for
/// [`TypeKind::Opaque`](crate::TypeKind::Opaque), so a value that could not be
/// written cannot be constructed. Refusing an opaque type happens where one is
/// first encountered — reading it out of memory, or [`from_text`].
pub fn to_text(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);

    out
}

/// Write `value` at `depth`, without a trailing newline.
fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Bool(inner) => write!(out, "{inner}").expect("writing to a String cannot fail"),
        Value::Char(inner) => write_char(out, *inner),
        Value::U8(inner) => write!(out, "{inner}").expect("infallible"),
        Value::U16(inner) => write!(out, "{inner}").expect("infallible"),
        Value::U32(inner) => write!(out, "{inner}").expect("infallible"),
        Value::U64(inner) => write!(out, "{inner}").expect("infallible"),
        Value::Usize(inner) => write!(out, "{inner}").expect("infallible"),
        Value::I8(inner) => write!(out, "{inner}").expect("infallible"),
        Value::I16(inner) => write!(out, "{inner}").expect("infallible"),
        Value::I32(inner) => write!(out, "{inner}").expect("infallible"),
        Value::I64(inner) => write!(out, "{inner}").expect("infallible"),
        Value::Isize(inner) => write!(out, "{inner}").expect("infallible"),
        // `{:?}` rather than `{}`: it is the shortest representation that parses
        // back to the same bits, and it keeps the `.0` on a whole number so a
        // reader can see the field is a float.
        Value::F32(inner) => write!(out, "{inner:?}").expect("infallible"),
        Value::F64(inner) => write!(out, "{inner:?}").expect("infallible"),
        Value::String(inner) => write_string(out, inner),
        Value::Struct(inner) => write_struct(out, inner, depth),
    }
}

/// Write a struct across several lines.
fn write_struct(out: &mut String, value: &Struct, depth: usize) {
    write!(out, "{} {{", value.path()).expect("infallible");

    if value.is_empty() {
        out.push(char::from(b'}'));
        return;
    }

    let inner = depth + 1;
    for (name, field) in value.fields() {
        out.push('\n');
        indent(out, inner);
        write!(out, "{name}: ").expect("infallible");
        write_value(out, field, inner);
        // A trailing comma on every field, so adding one at the end is a
        // one-line diff rather than two.
        out.push(',');
    }

    out.push('\n');
    indent(out, depth);
    out.push(char::from(b'}'));
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth * INDENT {
        out.push(' ');
    }
}

fn write_char(out: &mut String, value: char) {
    out.push('\'');
    match value {
        '\'' => out.push_str("\\'"),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        other => escape_or_push(out, other),
    }
    out.push('\'');
}

fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => escape_or_push(out, other),
        }
    }
    out.push('"');
}

/// Push a character, escaping it if it would be invisible or ambiguous in a file.
fn escape_or_push(out: &mut String, value: char) {
    if value.is_control() {
        write!(out, "\\u{{{:x}}}", value as u32).expect("infallible");
    } else {
        out.push(value);
    }
}

/// Read text as a value of the type `info` describes.
///
/// Schema-driven: the expected type decides how each literal is interpreted, so
/// the text carries no type suffixes.
///
/// # Errors
///
/// [`TextError`] with a line and column, for anything the text does not say or
/// says wrongly — a missing field, an unknown field, a number that does not fit,
/// a struct whose written path disagrees with the expected one.
pub fn from_text(text: &str, info: &TypeInfo, registry: &TypeRegistry) -> Result<Value, TextError> {
    let mut parser = Parser::new(text);
    let value = parser.value(info, registry)?;

    parser.skip_trivia();
    if !parser.at_end() {
        return Err(parser.error("expected end of input"));
    }

    Ok(value)
}

/// A cursor over the text, tracking position for error messages.
struct Parser<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, offset: 0 }
    }

    /// Build an error at the current position.
    fn error(&self, message: impl Into<String>) -> TextError {
        let consumed = &self.text[..self.offset];
        let line = consumed.matches('\n').count() + 1;
        let column = consumed
            .rsplit_once('\n')
            .map_or(consumed.chars().count(), |(_, tail)| tail.chars().count())
            + 1;

        TextError {
            line,
            column,
            message: message.into(),
        }
    }

    fn at_end(&self) -> bool {
        self.offset >= self.text.len()
    }

    fn rest(&self) -> &'a str {
        &self.text[self.offset..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.offset += next.len_utf8();

        Some(next)
    }

    /// Skip whitespace and `//` comments.
    fn skip_trivia(&mut self) {
        loop {
            let before = self.offset;

            while let Some(next) = self.peek() {
                if next.is_whitespace() {
                    self.offset += next.len_utf8();
                } else {
                    break;
                }
            }

            if self.rest().starts_with("//") {
                while let Some(next) = self.peek() {
                    self.offset += next.len_utf8();
                    if next == '\n' {
                        break;
                    }
                }
            }

            if self.offset == before {
                return;
            }
        }
    }

    /// Consume `expected`, or report what was found instead.
    fn expect(&mut self, expected: &str) -> Result<(), TextError> {
        self.skip_trivia();

        if self.rest().starts_with(expected) {
            self.offset += expected.len();
            return Ok(());
        }

        Err(self.error(format!("expected `{expected}`")))
    }

    /// Read a value of the type `info` describes.
    fn value(&mut self, info: &TypeInfo, registry: &TypeRegistry) -> Result<Value, TextError> {
        self.skip_trivia();

        match info.kind() {
            TypeKind::Primitive(primitive) => self.primitive(*primitive),
            TypeKind::String => Ok(Value::String(self.string()?)),
            TypeKind::Struct { fields } => self.structure(info, fields, registry),
            TypeKind::Opaque => Err(self.error(format!(
                "`{}` is opaque, so its value cannot be read",
                info.path()
            ))),
        }
    }

    fn primitive(&mut self, primitive: Primitive) -> Result<Value, TextError> {
        match primitive {
            Primitive::Bool => self.boolean(),
            Primitive::Char => Ok(Value::Char(self.character()?)),
            Primitive::F32 => self.float(Primitive::F32),
            Primitive::F64 => self.float(Primitive::F64),
            _ => self.integer(primitive),
        }
    }

    fn boolean(&mut self) -> Result<Value, TextError> {
        if self.rest().starts_with("true") {
            self.offset += 4;
            return Ok(Value::Bool(true));
        }
        if self.rest().starts_with("false") {
            self.offset += 5;
            return Ok(Value::Bool(false));
        }

        Err(self.error("expected `true` or `false`"))
    }

    /// Read the run of characters a number occupies.
    fn number_token(&mut self) -> &'a str {
        let start = self.offset;

        // Named floats first — they are words rather than digits.
        for name in ["NaN", "nan", "inf", "-inf", "infinity", "-infinity"] {
            if self.rest().starts_with(name) {
                self.offset += name.len();
                return &self.text[start..self.offset];
            }
        }

        while let Some(next) = self.peek() {
            let part = next.is_ascii_digit()
                || matches!(next, '-' | '+' | '.' | 'e' | 'E')
                // A `+`/`-` is only part of the number at the start or right
                // after an exponent marker, but accepting it here and letting
                // `parse` reject the result keeps this a scanner rather than a
                // second parser.
                ;

            if part {
                self.offset += next.len_utf8();
            } else {
                break;
            }
        }

        &self.text[start..self.offset]
    }

    /// Read a float.
    ///
    /// Parsed at its own width rather than as `f64` and narrowed: `f32::from_str`
    /// is exact for the shortest representation the writer emits, and narrowing
    /// would round twice.
    fn float(&mut self, primitive: Primitive) -> Result<Value, TextError> {
        let start = self.offset;
        let token = self.number_token();

        if token.is_empty() {
            return Err(self.error("expected a number"));
        }

        let parsed = match primitive {
            Primitive::F32 => token.parse::<f32>().map(Value::F32).ok(),
            Primitive::F64 => token.parse::<f64>().map(Value::F64).ok(),
            other => unreachable!("{other:?} is not a float"),
        };

        parsed.ok_or_else(|| {
            self.offset = start;
            self.error(format!("`{token}` is not a valid `{}`", primitive.name()))
        })
    }

    fn integer(&mut self, primitive: Primitive) -> Result<Value, TextError> {
        let start = self.offset;
        let token = self.number_token();

        if token.is_empty() {
            return Err(self.error("expected a number"));
        }

        // Each width parses through its own type, so a value that does not fit
        // is an error here rather than a silent truncation.
        macro_rules! parse {
            ($variant:ident, $type:ty) => {
                token.parse::<$type>().map(Value::$variant).map_err(|_| {
                    self.offset = start;
                    self.error(format!("`{token}` is not a valid `{}`", primitive.name()))
                })
            };
        }

        match primitive {
            Primitive::U8 => parse!(U8, u8),
            Primitive::U16 => parse!(U16, u16),
            Primitive::U32 => parse!(U32, u32),
            Primitive::U64 => parse!(U64, u64),
            Primitive::Usize => parse!(Usize, usize),
            Primitive::I8 => parse!(I8, i8),
            Primitive::I16 => parse!(I16, i16),
            Primitive::I32 => parse!(I32, i32),
            Primitive::I64 => parse!(I64, i64),
            Primitive::Isize => parse!(Isize, isize),
            other => unreachable!("{other:?} is not an integer"),
        }
    }

    fn character(&mut self) -> Result<char, TextError> {
        self.expect("'")?;

        let value = match self.bump() {
            Some('\\') => self.escape('\'')?,
            Some('\'') => return Err(self.error("a character literal cannot be empty")),
            Some(other) => other,
            None => return Err(self.error("unterminated character literal")),
        };

        self.expect("'")?;

        Ok(value)
    }

    fn string(&mut self) -> Result<String, TextError> {
        self.expect("\"")?;

        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') => return Ok(out),
                Some('\\') => out.push(self.escape('"')?),
                Some(other) => out.push(other),
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    /// Read the character after a backslash. `quote` is the delimiter that the
    /// enclosing literal also allows escaping.
    fn escape(&mut self, quote: char) -> Result<char, TextError> {
        match self.bump() {
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('\\') => Ok('\\'),
            Some('0') => Ok('\0'),
            Some(found) if found == quote => Ok(quote),
            Some('u') => self.unicode_escape(),
            Some(found) => Err(self.error(format!("unknown escape `\\{found}`"))),
            None => Err(self.error("unterminated escape")),
        }
    }

    fn unicode_escape(&mut self) -> Result<char, TextError> {
        self.expect("{")?;

        let start = self.offset;
        while self.peek().is_some_and(|next| next.is_ascii_hexdigit()) {
            self.offset += 1;
        }
        let digits = &self.text[start..self.offset];

        self.expect("}")?;

        let code = u32::from_str_radix(digits, 16)
            .map_err(|_| self.error(format!("`{digits}` is not a hexadecimal number")))?;

        char::from_u32(code).ok_or_else(|| self.error(format!("`{code:x}` is not a character")))
    }

    /// Read an identifier or a `::`-separated path.
    fn path(&mut self) -> &'a str {
        let start = self.offset;

        while let Some(next) = self.peek() {
            if next.is_alphanumeric() || next == '_' {
                self.offset += next.len_utf8();
            } else if next == ':' && self.rest().starts_with("::") {
                self.offset += 2;
            } else {
                break;
            }
        }

        &self.text[start..self.offset]
    }

    fn structure(
        &mut self,
        info: &TypeInfo,
        fields: &[crate::FieldInfo],
        registry: &TypeRegistry,
    ) -> Result<Value, TextError> {
        self.skip_trivia();

        // The path is written but optional on read: omitting it is convenient
        // when hand-editing, and stating it is checked, which catches a file
        // edited against a different schema.
        if self.peek().is_some_and(|next| next != '{') {
            let start = self.offset;
            let written = self.path();

            if written != info.path().as_str() {
                self.offset = start;
                return Err(self.error(format!("expected `{}`, found `{written}`", info.path())));
            }
        }

        self.expect("{")?;

        let mut read: Vec<(String, Value)> = Vec::with_capacity(fields.len());
        loop {
            self.skip_trivia();

            if self.rest().starts_with('}') {
                self.offset += 1;
                break;
            }

            if self.at_end() {
                return Err(self.error("unterminated struct"));
            }

            let start = self.offset;
            let name = self.path().to_owned();
            if name.is_empty() {
                return Err(self.error("expected a field name"));
            }

            let Some(field) = fields.iter().find(|field| field.name == name) else {
                self.offset = start;
                return Err(self.error(format!("`{}` has no field `{name}`", info.path())));
            };

            if read.iter().any(|(seen, _)| *seen == name) {
                self.offset = start;
                return Err(self.error(format!("field `{name}` is given twice")));
            }

            self.expect(":")?;

            let Some(field_info) = registry.get(field.type_id) else {
                self.offset = start;
                return Err(self.error(format!("the type of field `{name}` is not registered")));
            };

            let value = self.value(field_info, registry)?;
            read.push((name, value));

            self.skip_trivia();
            if self.rest().starts_with(',') {
                self.offset += 1;
            } else if !self.rest().starts_with('}') {
                return Err(self.error("expected `,` or `}`"));
            }
        }

        // Reordered into declaration order rather than the order the file gave,
        // so a value read from a hand-edited file compares equal to one built in
        // code and writes back out canonically.
        let mut ordered = Vec::with_capacity(fields.len());
        for field in fields {
            let Some(position) = read.iter().position(|(name, _)| *name == field.name) else {
                return Err(self.error(format!(
                    "`{}` is missing field `{}`",
                    info.path(),
                    field.name
                )));
            };

            ordered.push(read.swap_remove(position));
        }

        Ok(Value::Struct(Struct::new(info.path().clone(), ordered)))
    }
}
