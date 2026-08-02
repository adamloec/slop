//! The text format, and the round-trip property `docs/DESIGN.md` §5 asks for.
//!
//! > Serialization round-trip tests. Every reflected type: serialize →
//! > deserialize → compare.
//!
//! The property under test is one line: **`from_text(to_text(v)) == v`, exactly.**
//! Most of this file is that assertion aimed at the values most likely to break
//! it — float edge cases, escapes, nesting — plus the parser's error paths, since
//! a scene file is something a human edits and the errors are the interface.

use slop_reflect::{
    FieldInfo, Primitive, Reflect, Struct, TypeInfo, TypeKind, TypeRegistry, Value, from_text,
    register_builtins, to_text,
};

#[derive(Reflect)]
#[repr(C)]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Reflect)]
#[repr(C)]
struct Health {
    current: u32,
    maximum: u32,
}

#[derive(Reflect)]
#[repr(C)]
struct Body {
    position: Position,
    health: Health,
}

#[derive(Reflect)]
#[repr(C)]
struct Labelled {
    name: String,
    visible: bool,
    initial: char,
}

/// Every width, so no primitive goes untested.
#[derive(Reflect)]
#[repr(C)]
struct Widths {
    a: u8,
    b: u16,
    c: u32,
    d: u64,
    e: i8,
    f: i16,
    g: i32,
    h: i64,
    i: f32,
    j: f64,
    k: bool,
}

#[derive(Reflect)]
#[repr(C)]
struct Empty {}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::new();
    register_builtins(&mut registry).expect("fresh");
    registry.register(Position::type_info()).expect("fresh");
    registry.register(Health::type_info()).expect("fresh");
    registry.register(Body::type_info()).expect("fresh");
    registry.register(Labelled::type_info()).expect("fresh");
    registry.register(Widths::type_info()).expect("fresh");
    registry.register(Empty::type_info()).expect("fresh");

    registry
}

/// The property, applied to one value.
#[track_caller]
fn round_trips(value: &Value, info: &TypeInfo) {
    let text = to_text(value);
    let back = from_text(&text, info, &registry())
        .unwrap_or_else(|error| panic!("failed to read back:\n{text}\n{error}"));

    assert_eq!(
        &back, value,
        "round trip changed the value; text was:\n{text}"
    );
}

/// The property, applied to a bare primitive.
#[track_caller]
fn primitive_round_trips(value: Value) {
    let primitive = value.primitive().expect("a primitive");
    let info = TypeInfo::new(
        primitive.name(),
        std::alloc::Layout::new::<u64>(),
        slop_reflect::Transfer::Blittable,
        TypeKind::Primitive(primitive),
    );

    round_trips(&value, &info);
}

#[test]
fn every_integer_width_round_trips_at_its_extremes() {
    for value in [
        Value::U8(0),
        Value::U8(u8::MAX),
        Value::U16(u16::MAX),
        Value::U32(u32::MAX),
        // The value that a widened `Int(i64)` representation would lose.
        Value::U64(u64::MAX),
        Value::Usize(usize::MAX),
        Value::I8(i8::MIN),
        Value::I8(i8::MAX),
        Value::I16(i16::MIN),
        Value::I32(i32::MIN),
        Value::I64(i64::MIN),
        Value::Isize(isize::MIN),
    ] {
        primitive_round_trips(value);
    }
}

#[test]
fn floats_round_trip_bit_for_bit() {
    for value in [
        0.0_f32,
        -0.0,
        1.0,
        -1.5,
        f32::MIN,
        f32::MAX,
        f32::MIN_POSITIVE,
        f32::EPSILON,
        // A value whose shortest representation needs every digit.
        1.234_567_8_f32,
        std::f32::consts::PI,
    ] {
        let text = to_text(&Value::F32(value));
        let parsed: f32 = text.parse().expect("the writer emits a parsable float");

        assert_eq!(
            parsed.to_bits(),
            value.to_bits(),
            "{value:?} wrote as `{text}` and came back different"
        );
        primitive_round_trips(Value::F32(value));
    }
}

#[test]
fn double_precision_round_trips_bit_for_bit() {
    for value in [
        0.0_f64,
        -0.0,
        f64::MIN,
        f64::MAX,
        f64::EPSILON,
        std::f64::consts::PI,
        1.0 / 3.0,
    ] {
        primitive_round_trips(Value::F64(value));
    }
}

#[test]
fn infinities_keep_their_sign() {
    primitive_round_trips(Value::F32(f32::INFINITY));
    primitive_round_trips(Value::F32(f32::NEG_INFINITY));
    primitive_round_trips(Value::F64(f64::INFINITY));
    primitive_round_trips(Value::F64(f64::NEG_INFINITY));

    assert_eq!(to_text(&Value::F32(f32::INFINITY)), "inf");
    assert_eq!(to_text(&Value::F32(f32::NEG_INFINITY)), "-inf");
}

#[test]
fn a_nan_survives_as_a_nan() {
    // The one documented inexactness: the payload does not survive, so `Value`
    // treats any NaN as equal to any other and this passes on that basis.
    primitive_round_trips(Value::F32(f32::NAN));
    primitive_round_trips(Value::F64(f64::NAN));

    assert_eq!(to_text(&Value::F32(f32::NAN)), "NaN");
}

#[test]
fn a_whole_float_keeps_its_point() {
    // So a reader can tell the field is a float without consulting the type.
    assert_eq!(to_text(&Value::F32(1.0)), "1.0");
    assert_eq!(to_text(&Value::F64(-2.0)), "-2.0");
}

#[test]
fn booleans_and_characters_round_trip() {
    primitive_round_trips(Value::Bool(true));
    primitive_round_trips(Value::Bool(false));

    for value in ['a', 'Z', '0', 'é', '日', '\n', '\t', '\\', '\'', '"', '\0'] {
        primitive_round_trips(Value::Char(value));
    }
}

#[test]
fn a_struct_round_trips() {
    let value = Value::Struct(Struct::new(
        "text::Position",
        vec![
            ("x".to_owned(), Value::F32(1.0)),
            ("y".to_owned(), Value::F32(-2.5)),
            ("z".to_owned(), Value::F32(0.0)),
        ],
    ));

    round_trips(&value, &Position::type_info());
}

#[test]
fn a_nested_struct_round_trips() {
    let value = Value::Struct(Struct::new(
        "text::Body",
        vec![
            (
                "position".to_owned(),
                Value::Struct(Struct::new(
                    "text::Position",
                    vec![
                        ("x".to_owned(), Value::F32(1.0)),
                        ("y".to_owned(), Value::F32(2.0)),
                        ("z".to_owned(), Value::F32(3.0)),
                    ],
                )),
            ),
            (
                "health".to_owned(),
                Value::Struct(Struct::new(
                    "text::Health",
                    vec![
                        ("current".to_owned(), Value::U32(50)),
                        ("maximum".to_owned(), Value::U32(100)),
                    ],
                )),
            ),
        ],
    ));

    round_trips(&value, &Body::type_info());
}

#[test]
fn an_empty_struct_round_trips() {
    let value = Value::Struct(Struct::new("text::Empty", Vec::new()));

    round_trips(&value, &Empty::type_info());
    assert_eq!(to_text(&value), "text::Empty {}");
}

#[test]
fn strings_round_trip_including_the_awkward_ones() {
    for text in [
        "",
        "plain",
        "with \"quotes\"",
        "with \\ backslash",
        "line\nbreak",
        "tab\there",
        "unicode: é 日 🎮",
        "control: \u{7}",
        "// not a comment",
        "{ not a struct }",
    ] {
        let value = Value::Struct(Struct::new(
            "text::Labelled",
            vec![
                ("name".to_owned(), Value::String(text.to_owned())),
                ("visible".to_owned(), Value::Bool(true)),
                ("initial".to_owned(), Value::Char('x')),
            ],
        ));

        round_trips(&value, &Labelled::type_info());
    }
}

#[test]
fn every_primitive_width_round_trips_inside_one_struct() {
    let value = Value::Struct(Struct::new(
        "text::Widths",
        vec![
            ("a".to_owned(), Value::U8(u8::MAX)),
            ("b".to_owned(), Value::U16(u16::MAX)),
            ("c".to_owned(), Value::U32(u32::MAX)),
            ("d".to_owned(), Value::U64(u64::MAX)),
            ("e".to_owned(), Value::I8(i8::MIN)),
            ("f".to_owned(), Value::I16(i16::MIN)),
            ("g".to_owned(), Value::I32(i32::MIN)),
            ("h".to_owned(), Value::I64(i64::MIN)),
            ("i".to_owned(), Value::F32(f32::MIN)),
            ("j".to_owned(), Value::F64(f64::MAX)),
            ("k".to_owned(), Value::Bool(false)),
        ],
    ));

    round_trips(&value, &Widths::type_info());
}

#[test]
fn the_written_form_is_what_a_human_would_want_to_diff() {
    let value = Value::Struct(Struct::new(
        "text::Health",
        vec![
            ("current".to_owned(), Value::U32(50)),
            ("maximum".to_owned(), Value::U32(100)),
        ],
    ));

    assert_eq!(
        to_text(&value),
        "text::Health {\n    current: 50,\n    maximum: 100,\n}"
    );
}

#[test]
fn a_trailing_comma_on_every_field_keeps_diffs_to_one_line() {
    let text = to_text(&Value::Struct(Struct::new(
        "text::Health",
        vec![
            ("current".to_owned(), Value::U32(1)),
            ("maximum".to_owned(), Value::U32(2)),
        ],
    )));

    assert!(
        text.contains("maximum: 2,\n}"),
        "no trailing comma:\n{text}"
    );
}

#[test]
fn the_type_path_may_be_omitted_when_reading() {
    // Convenient when hand-editing. Stating it is checked; omitting it is
    // inferred from the schema.
    let value = from_text(
        "{ current: 10, maximum: 20 }",
        &Health::type_info(),
        &registry(),
    )
    .expect("valid");

    assert_eq!(
        value.as_struct().and_then(|value| value.field("current")),
        Some(&Value::U32(10))
    );
}

#[test]
fn a_wrong_type_path_is_rejected() {
    // What catches a file edited against a different schema.
    let error = from_text(
        "text::Position { current: 10, maximum: 20 }",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("the path disagrees");

    assert!(error.message.contains("text::Health"), "{error}");
}

#[test]
fn fields_may_be_written_in_any_order_and_come_back_canonical() {
    // So a value read from a hand-edited file compares equal to one built in
    // code, and writes back out in declaration order.
    let value = from_text(
        "{ maximum: 20, current: 10 }",
        &Health::type_info(),
        &registry(),
    )
    .expect("valid");

    let names: Vec<&str> = value
        .as_struct()
        .expect("a struct")
        .fields()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();

    assert_eq!(names, vec!["current", "maximum"]);
}

#[test]
fn comments_and_whitespace_are_ignored() {
    let text = "
        // the player's health
        text::Health {
            current: 10,   // hurt
            maximum: 20,
        }
    ";

    let value = from_text(text, &Health::type_info(), &registry()).expect("valid");

    assert_eq!(
        value.as_struct().and_then(|value| value.field("current")),
        Some(&Value::U32(10))
    );
}

#[test]
fn a_missing_trailing_comma_is_accepted() {
    let value = from_text(
        "{ current: 1, maximum: 2 }",
        &Health::type_info(),
        &registry(),
    )
    .expect("valid");

    assert_eq!(value.as_struct().expect("a struct").len(), 2);
}

#[test]
fn a_missing_field_is_an_error_naming_it() {
    let error = from_text("{ current: 1 }", &Health::type_info(), &registry())
        .expect_err("maximum is absent");

    assert!(error.message.contains("maximum"), "{error}");
}

#[test]
fn an_unknown_field_is_an_error_naming_it() {
    let error = from_text(
        "{ current: 1, maximum: 2, armour: 3 }",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("armour is not a field");

    assert!(error.message.contains("armour"), "{error}");
}

#[test]
fn a_repeated_field_is_an_error() {
    let error = from_text(
        "{ current: 1, current: 2, maximum: 3 }",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("current is given twice");

    assert!(error.message.contains("twice"), "{error}");
}

#[test]
fn a_number_that_does_not_fit_is_an_error_rather_than_a_truncation() {
    // Silently wrapping to 44 would be a save file that loads wrong.
    let error = from_text(
        "{ current: 4294967296, maximum: 1 }",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("one past u32::MAX");

    assert!(error.message.contains("u32"), "{error}");
}

#[test]
fn a_negative_number_in_an_unsigned_field_is_an_error() {
    let error = from_text(
        "{ current: -1, maximum: 1 }",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("u32 cannot be negative");

    assert!(error.message.contains("u32"), "{error}");
}

#[test]
fn an_error_reports_where_it_happened() {
    let error = from_text(
        "text::Health {\n    current: 1,\n    bogus: 2,\n}",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("bogus is not a field");

    assert_eq!(error.line, 3, "{error}");
    assert_eq!(error.column, 5, "{error}");
}

#[test]
fn trailing_content_after_a_value_is_an_error() {
    let error = from_text(
        "{ current: 1, maximum: 2 } and then some",
        &Health::type_info(),
        &registry(),
    )
    .expect_err("there is more text");

    assert!(error.message.contains("end of input"), "{error}");
}

#[test]
fn an_unterminated_struct_is_an_error() {
    let error = from_text("{ current: 1,", &Health::type_info(), &registry())
        .expect_err("no closing brace");

    assert!(error.message.contains("unterminated"), "{error}");
}

#[test]
fn an_unterminated_string_is_an_error() {
    let error = from_text(
        "{ name: \"unclosed, visible: true, initial: 'x' }",
        &Labelled::type_info(),
        &registry(),
    )
    .expect_err("no closing quote");

    assert!(error.message.contains("unterminated"), "{error}");
}

#[test]
fn an_opaque_type_cannot_be_written_or_read() {
    // `Opaque` means "its internals are the owning crate's business", so there
    // is nothing to serialize. A component that must survive a save needs a
    // describable kind — which is exactly why `String` stopped being opaque.
    let opaque = TypeInfo::new(
        "game::Handle",
        std::alloc::Layout::new::<u64>(),
        slop_reflect::Transfer::Owning,
        TypeKind::Opaque,
    );

    let error = from_text("0", &opaque, &registry()).expect_err("nothing to read");

    assert!(error.message.contains("opaque"), "{error}");
}

#[test]
fn a_field_whose_type_is_unregistered_is_an_error() {
    // The registry is what resolves a field's type, and a scene naming a
    // component the engine does not have is a wiring problem rather than
    // something to guess at.
    let dangling = TypeInfo::new(
        "game::Dangling",
        std::alloc::Layout::new::<u32>(),
        slop_reflect::Transfer::Blittable,
        TypeKind::Struct {
            fields: vec![FieldInfo::new(
                "value",
                0,
                slop_reflect::TypeId::from_path("game::NeverRegistered"),
            )],
        },
    );

    let error =
        from_text("{ value: 1 }", &dangling, &registry()).expect_err("the field type is unknown");

    assert!(error.message.contains("not registered"), "{error}");
}

#[test]
fn every_registered_primitive_can_describe_a_value() {
    // §5 asks for the round trip to run "automatically for all registered
    // types". This is the primitive half: the registry's own built-ins, each
    // exercised through the kind it reports rather than a hand-written list.
    let registry = registry();

    for info in registry.sorted() {
        let TypeKind::Primitive(primitive) = info.kind() else {
            continue;
        };

        let value = match primitive {
            Primitive::Bool => Value::Bool(true),
            Primitive::Char => Value::Char('t'),
            Primitive::U8 => Value::U8(7),
            Primitive::U16 => Value::U16(7),
            Primitive::U32 => Value::U32(7),
            Primitive::U64 => Value::U64(7),
            Primitive::Usize => Value::Usize(7),
            Primitive::I8 => Value::I8(-7),
            Primitive::I16 => Value::I16(-7),
            Primitive::I32 => Value::I32(-7),
            Primitive::I64 => Value::I64(-7),
            Primitive::Isize => Value::Isize(-7),
            Primitive::F32 => Value::F32(-7.5),
            Primitive::F64 => Value::F64(-7.5),
        };

        let text = to_text(&value);
        let back = from_text(&text, info, &registry)
            .unwrap_or_else(|error| panic!("{} failed: {error}", info.path()));

        assert_eq!(back, value, "{} did not round trip", info.path());
    }
}
