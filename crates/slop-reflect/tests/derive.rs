//! What `#[derive(Reflect)]` produces, and what it refuses to be told.
//!
//! An integration test rather than a unit test because the derive resolves
//! `module_path!()` at its use site, so the paths it produces are only correct
//! when it is used from outside the crate that defines it — which is also how
//! every real consumer will use it.
//!
//! The theme throughout: `docs/DESIGN.md` §2.4 makes `TypeInfo` a value, and the
//! ECS trusts three of its fields in ways that are memory-unsafe or ABI-unsafe
//! to get wrong. The derive takes none of them from the author, and these tests
//! are what hold that line.

use slop_reflect::{
    FieldInfo, Reflect, Transfer, TypeId, TypeInfo, TypeKind, TypeRegistry, register_builtins,
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

/// Nested, to prove blittability composes through a field that is itself a
/// derived struct rather than a primitive.
#[derive(Reflect)]
#[repr(C)]
struct Body {
    position: Position,
    health: Health,
}

/// Owns a heap allocation, so it can never be blittable however it is written.
#[derive(Reflect)]
#[repr(C)]
struct Named {
    name: String,
    health: Health,
}

/// No `#[repr(C)]`. Field order is unspecified, so offsets mean nothing to a
/// separately compiled guest module even though every field is blittable.
#[derive(Reflect)]
struct DefaultRepr {
    x: f32,
    y: f32,
}

#[derive(Reflect)]
#[repr(C)]
#[reflect(path = "game::Pinned")]
struct Renamed {
    value: u32,
}

#[test]
fn a_path_comes_from_the_module_it_is_declared_in() {
    // `module_path!()` resolves at the use site, so this is the test crate's
    // path rather than `slop_reflect`'s. Getting that wrong would give every
    // consumer's types the same prefix.
    assert_eq!(Position::PATH, "derive::Position");
    assert_eq!(Position::type_id(), TypeId::from_path("derive::Position"));
}

#[test]
fn an_explicit_path_overrides_the_module() {
    // The escape hatch that lets a type move between modules without
    // invalidating every save file written against it.
    assert_eq!(Renamed::PATH, "game::Pinned");
    assert_eq!(Renamed::type_id(), TypeId::from_path("game::Pinned"));
}

#[test]
fn the_layout_is_the_types_own() {
    let info = Position::type_info();

    assert_eq!(info.layout(), std::alloc::Layout::new::<Position>());
    assert_eq!(info.layout().size(), 12);
    assert_eq!(info.layout().align(), 4);
}

#[test]
fn field_offsets_are_real_offsets() {
    // Not positions in a list — the actual byte offsets, which is what a
    // property panel, a serializer and a guest module all index by.
    let info = Position::type_info();
    let offset = |name: &str| info.field(name).map(|field| field.offset);

    assert_eq!(offset("x"), Some(0));
    assert_eq!(offset("y"), Some(4));
    assert_eq!(offset("z"), Some(8));
}

#[test]
fn fields_are_in_declaration_order() {
    // The order a property panel and a serializer present them, so it has to
    // follow the source rather than a hash map's whim.
    let info = Body::type_info();
    let names: Vec<&str> = info
        .fields()
        .iter()
        .map(|field| field.name.as_str())
        .collect();

    assert_eq!(names, vec!["position", "health"]);
}

#[test]
fn a_field_names_its_own_type() {
    let info = Body::type_info();

    assert_eq!(
        info.field("position").map(|field| field.type_id),
        Some(Position::type_id())
    );
    assert_eq!(
        info.field("health").map(|field| field.type_id),
        Some(Health::type_id())
    );
}

#[test]
fn a_plain_data_struct_is_blittable() {
    assert_eq!(Position::TRANSFER, Transfer::Blittable);
    assert_eq!(Position::type_info().transfer(), Transfer::Blittable);
}

#[test]
fn blittability_composes_through_nested_structs() {
    // `Body` is blittable only because `Position` and `Health` both are. The
    // whole point of `TRANSFER` being a `const` is that this folds at compile
    // time rather than walking a registry.
    assert_eq!(Body::TRANSFER, Transfer::Blittable);
}

#[test]
fn one_owning_field_makes_the_whole_struct_owning() {
    // The property that matters most here. `Named` is annotated exactly like
    // `Body` — same derive, same `#[repr(C)]` — and cannot claim to be
    // blittable, because `String` is not. A struct holding a pointer into the
    // host heap must never be handed to a guest as raw bytes.
    assert_eq!(Named::TRANSFER, Transfer::Owning);
    assert_eq!(Named::type_info().transfer(), Transfer::Owning);
}

#[test]
fn a_struct_with_interior_padding_is_never_blittable() {
    // The bug this pins. `#[repr(C)]`, no destructor, both fields blittable —
    // every question the check used to ask says yes. But `u8` followed by `u32`
    // puts three bytes of padding between them, and the compiler never writes
    // those bytes: they hold whatever was in that memory before.
    //
    // Two consequences, and the second is the reason this is not a nitpick.
    // Reading them is undefined, which Miri reports the moment
    // `Column::as_bytes` touches the array. And `Blittable` is precisely the
    // flag deciding what may cross into a WASM guest, so the padding would
    // carry three bytes of host memory through the wall §2.3 exists to build.
    #[derive(Reflect)]
    #[repr(C)]
    struct Padded {
        small: u8,
        large: u32,
    }

    assert_eq!(size_of::<Padded>(), 8);
    assert_eq!(
        size_of::<u8>() + size_of::<u32>(),
        5,
        "three bytes unaccounted"
    );
    assert_eq!(Padded::TRANSFER, Transfer::Owning);
}

#[test]
fn a_struct_with_trailing_padding_is_never_blittable() {
    // The other shape: alignment rounds the size up past the last field.
    #[derive(Reflect)]
    #[repr(C)]
    struct Trailing {
        large: u32,
        small: u8,
    }

    assert_eq!(size_of::<Trailing>(), 8);
    assert_eq!(Trailing::TRANSFER, Transfer::Owning);
}

#[test]
fn an_over_aligned_struct_is_never_blittable() {
    // `align(16)` around one byte is fifteen bytes of padding.
    #[derive(Reflect)]
    #[repr(C, align(16))]
    struct OverAligned {
        tag: u8,
    }

    assert_eq!(size_of::<OverAligned>(), 16);
    assert_eq!(OverAligned::TRANSFER, Transfer::Owning);
}

#[test]
fn padding_inside_a_field_makes_the_outer_struct_owning() {
    // Nested padding needs no separate check, and this is why: the inner struct
    // is already `Owning`, and every field must be blittable for the outer one
    // to qualify. Worth pinning because the size arithmetic alone would pass —
    // `Outer` is exactly the sum of its fields.
    #[derive(Reflect)]
    #[repr(C)]
    struct Inner {
        small: u8,
        large: u32,
    }

    #[derive(Reflect)]
    #[repr(C)]
    struct Outer {
        inner: Inner,
        value: u32,
    }

    assert_eq!(
        size_of::<Outer>(),
        size_of::<Inner>() + size_of::<u32>(),
        "the outer struct itself has no padding"
    );
    assert_eq!(Inner::TRANSFER, Transfer::Owning);
    assert_eq!(Outer::TRANSFER, Transfer::Owning);
}

#[test]
fn a_tightly_packed_struct_is_still_blittable() {
    // The check must not be so broad that it rejects what it should allow.
    #[derive(Reflect)]
    #[repr(C)]
    struct Tight {
        x: f32,
        y: f32,
        z: f32,
    }

    assert_eq!(size_of::<Tight>(), 12);
    assert_eq!(Tight::TRANSFER, Transfer::Blittable);
}

#[test]
fn a_default_repr_struct_is_never_blittable() {
    // Every field is blittable and there is no destructor, yet the answer is
    // still `Owning`: `#[repr(Rust)]` leaves field order unspecified, so the
    // offsets are not reproducible across compilations and mean nothing to a
    // separately compiled guest.
    assert_eq!(DefaultRepr::TRANSFER, Transfer::Owning);
}

#[test]
fn a_destructor_is_installed_exactly_when_one_is_needed() {
    // Derived from `needs_drop`, never declared. Adding a `String` field to a
    // component installs a destructor with no edit to the derive.
    assert!(Position::type_info().drop_in_place().is_none());
    assert!(Body::type_info().drop_in_place().is_none());
    assert!(Named::type_info().drop_in_place().is_some());
}

#[test]
fn the_installed_destructor_actually_frees() {
    // The mechanism the ECS will free every component through, exercised end to
    // end. A destructor that runs the wrong type's drop is a leak at best.
    let info = Named::type_info();
    let drop_fn = info
        .drop_in_place()
        .expect("a struct owning a String needs a destructor");

    let mut value = std::mem::ManuallyDrop::new(Named {
        // Long enough to be heap-allocated rather than inline, so a leak is a
        // real one that a sanitizer or allocator counter would see.
        name: "a name long enough to live on the heap rather than the stack".repeat(4),
        health: Health {
            current: 1,
            maximum: 2,
        },
    });

    // SAFETY: `value` is a live, aligned, initialized `Named`, `drop_fn` came
    // from `Named`'s own `TypeInfo`, and `ManuallyDrop` stops the scope end from
    // dropping it a second time.
    unsafe {
        drop_fn(std::ptr::from_mut(&mut *value).cast::<u8>());
    }
}

#[test]
fn a_struct_reports_itself_as_a_struct() {
    assert!(matches!(
        Position::type_info().kind(),
        TypeKind::Struct { .. }
    ));
    assert_eq!(Position::type_info().fields().len(), 3);
}

#[test]
fn derived_types_register_and_resolve_against_the_primitives() {
    // The end-to-end shape: register the primitives, register some components,
    // and confirm every field's type is present. An unresolved field means a
    // property panel with a blank row and a serializer that cannot proceed.
    let mut registry = TypeRegistry::new();
    register_builtins(&mut registry).expect("a fresh registry");

    registry.register_native::<Position>().expect("fresh");
    registry.register_native::<Health>().expect("fresh");
    registry.register_native::<Body>().expect("fresh");

    assert_eq!(registry.unresolved_fields(), Vec::new());
    assert!(registry.get_by_path("derive::Body").is_some());
    assert_eq!(
        registry
            .get(Body::type_id())
            .map(|info| info.fields().len()),
        Some(2)
    );
}

#[test]
fn registering_a_component_before_its_field_types_is_caught_not_crashed() {
    // Registration order is not something a guest module loader controls, so
    // the missing type is reported afterward rather than being an ordering
    // requirement.
    let mut registry = TypeRegistry::new();
    register_builtins(&mut registry).expect("a fresh registry");
    registry.register_native::<Body>().expect("fresh");

    let missing = registry.unresolved_fields();

    assert_eq!(missing.len(), 2, "both of Body's field types are absent");
    // Owners are sorted by path, but a single owner's fields stay in
    // declaration order — the order a property panel would list them, and the
    // order a person reading the report expects.
    assert_eq!(missing[0].1, "position");
    assert_eq!(missing[1].1, "health");

    registry.register_native::<Position>().expect("fresh");
    registry.register_native::<Health>().expect("fresh");

    assert!(
        registry.unresolved_fields().is_empty(),
        "the graph should now be closed"
    );
}

#[test]
fn two_types_with_the_same_short_name_do_not_collide() {
    // `module_path!()` is what keeps `derive::Position` and
    // `nested::Position` apart. Without it every consumer's `Transform` would
    // conflict with the engine's.
    mod nested {
        #[derive(super::Reflect)]
        #[repr(C)]
        pub(super) struct Position {
            pub(super) w: f32,
        }
    }

    assert_ne!(Position::PATH, nested::Position::PATH);
    assert_ne!(Position::type_id(), nested::Position::type_id());

    let mut registry = TypeRegistry::new();
    registry.register_native::<Position>().expect("fresh");
    registry
        .register_native::<nested::Position>()
        .expect("a different module is a different type");

    assert_eq!(registry.len(), 2);
}

#[test]
fn type_info_is_stable_across_calls() {
    // Called once per registration, but a module reloaded twice must produce
    // the same description or `register` reports a spurious conflict.
    assert_eq!(Position::type_info(), Position::type_info());
}

#[test]
fn the_registry_catches_a_hand_written_padded_blittable_claim() {
    // The derive cannot get this wrong, but `TypeInfo::new` is safe and takes
    // the author's word — which is the path a guest module's type table takes.
    // This is the audit a module loader runs, and the input it is meant to
    // distrust.
    let mut registry = TypeRegistry::new();
    slop_reflect::register_builtins(&mut registry).expect("fresh");

    registry
        .register(TypeInfo::new(
            "guest::Padded",
            std::alloc::Layout::from_size_align(8, 4).expect("valid"),
            Transfer::Blittable,
            TypeKind::Struct {
                fields: vec![
                    FieldInfo::new("small", 0, <u8 as Reflect>::type_id()),
                    FieldInfo::new("large", 4, <u32 as Reflect>::type_id()),
                ],
            },
        ))
        .expect("fresh");

    let offenders = registry.padded_blittable();

    assert_eq!(offenders.len(), 1);
    assert_eq!(offenders[0].0.as_str(), "guest::Padded");
    assert_eq!(offenders[0].1, 3, "three bytes nobody writes");
}

#[test]
fn the_registry_accepts_a_tightly_packed_blittable_claim() {
    let mut registry = TypeRegistry::new();
    slop_reflect::register_builtins(&mut registry).expect("fresh");

    registry
        .register(TypeInfo::new(
            "guest::Tight",
            std::alloc::Layout::from_size_align(8, 4).expect("valid"),
            Transfer::Blittable,
            TypeKind::Struct {
                fields: vec![
                    FieldInfo::new("x", 0, <u32 as Reflect>::type_id()),
                    FieldInfo::new("y", 4, <u32 as Reflect>::type_id()),
                ],
            },
        ))
        .expect("fresh");

    assert!(registry.padded_blittable().is_empty());
}

#[test]
fn the_registry_skips_a_type_whose_fields_it_cannot_resolve() {
    // Reported by `unresolved_fields` instead. Guessing at a size we do not
    // have would turn a missing registration into a false padding report.
    let mut registry = TypeRegistry::new();

    registry
        .register(TypeInfo::new(
            "guest::Forward",
            std::alloc::Layout::from_size_align(8, 4).expect("valid"),
            Transfer::Blittable,
            TypeKind::Struct {
                fields: vec![FieldInfo::new(
                    "later",
                    0,
                    TypeId::from_path("guest::NotYet"),
                )],
            },
        ))
        .expect("fresh");

    assert!(registry.padded_blittable().is_empty());
    assert_eq!(registry.unresolved_fields().len(), 1);
}

#[test]
fn every_derived_component_in_the_workspace_style_passes_the_audit() {
    // The derive's guarantee, restated as the property the audit checks — so
    // the two cannot drift apart.
    let mut registry = TypeRegistry::new();
    slop_reflect::register_builtins(&mut registry).expect("fresh");
    registry.register(Position::type_info()).expect("fresh");
    registry.register(Health::type_info()).expect("fresh");
    registry.register(Body::type_info()).expect("fresh");

    assert!(registry.padded_blittable().is_empty());
}

#[test]
fn a_layout_fingerprint_changes_when_a_field_is_added() {
    // The version-skew check. `TypeId` cannot catch this — it hashes the path,
    // which is exactly what stays the same across a rebuild that changed the
    // struct. A guest compiled against the four-field version would read
    // adjacent entities' bytes as its own, silently.
    let three = TypeInfo::new(
        "game::Position",
        std::alloc::Layout::from_size_align(12, 4).expect("valid"),
        Transfer::Blittable,
        TypeKind::Struct {
            fields: vec![
                FieldInfo::new("x", 0, <f32 as Reflect>::type_id()),
                FieldInfo::new("y", 4, <f32 as Reflect>::type_id()),
                FieldInfo::new("z", 8, <f32 as Reflect>::type_id()),
            ],
        },
    );

    let four = TypeInfo::new(
        "game::Position",
        std::alloc::Layout::from_size_align(16, 4).expect("valid"),
        Transfer::Blittable,
        TypeKind::Struct {
            fields: vec![
                FieldInfo::new("x", 0, <f32 as Reflect>::type_id()),
                FieldInfo::new("y", 4, <f32 as Reflect>::type_id()),
                FieldInfo::new("z", 8, <f32 as Reflect>::type_id()),
                FieldInfo::new("w", 12, <f32 as Reflect>::type_id()),
            ],
        },
    );

    assert_eq!(
        three.id(),
        four.id(),
        "the path, and so the id, is unchanged"
    );
    assert_ne!(
        three.fingerprint(),
        four.fingerprint(),
        "but the layout is not"
    );
}

#[test]
fn a_layout_fingerprint_is_stable_for_one_definition() {
    assert_eq!(
        Position::type_info().fingerprint(),
        Position::type_info().fingerprint()
    );
}

#[test]
fn a_layout_fingerprint_notices_a_reordered_field() {
    // Same fields, same size, different offsets. A guest reading `y` would get
    // `x`, which is the quietest possible failure.
    let forward = TypeInfo::new(
        "game::Pair",
        std::alloc::Layout::from_size_align(8, 4).expect("valid"),
        Transfer::Blittable,
        TypeKind::Struct {
            fields: vec![
                FieldInfo::new("x", 0, <f32 as Reflect>::type_id()),
                FieldInfo::new("y", 4, <f32 as Reflect>::type_id()),
            ],
        },
    );

    let swapped = TypeInfo::new(
        "game::Pair",
        std::alloc::Layout::from_size_align(8, 4).expect("valid"),
        Transfer::Blittable,
        TypeKind::Struct {
            fields: vec![
                FieldInfo::new("x", 4, <f32 as Reflect>::type_id()),
                FieldInfo::new("y", 0, <f32 as Reflect>::type_id()),
            ],
        },
    );

    assert_ne!(forward.fingerprint(), swapped.fingerprint());
}

#[test]
fn a_layout_fingerprint_notices_a_renamed_field() {
    // A rename is a layout change from a serializer's point of view, since
    // fields are addressed by name.
    let named = |name: &str| {
        TypeInfo::new(
            "game::One",
            std::alloc::Layout::from_size_align(4, 4).expect("valid"),
            Transfer::Blittable,
            TypeKind::Struct {
                fields: vec![FieldInfo::new(name, 0, <f32 as Reflect>::type_id())],
            },
        )
    };

    assert_ne!(named("x").fingerprint(), named("width").fingerprint());
}

#[test]
fn a_layout_fingerprint_ignores_the_path() {
    // The path is the identity two fingerprints are compared *under*, so folding
    // it in would only ever compare a type to itself.
    let at = |path: &str| {
        TypeInfo::new(
            path,
            std::alloc::Layout::new::<f32>(),
            Transfer::Blittable,
            TypeKind::Primitive(slop_reflect::Primitive::F32),
        )
    };

    assert_eq!(at("game::A").fingerprint(), at("other::B").fingerprint());
}

#[test]
fn a_registry_fingerprint_covers_which_types_exist() {
    // At table scope, unlike at type scope, *which* types are present is part of
    // what must agree — so paths are folded in here.
    let mut small = TypeRegistry::new();
    register_builtins(&mut small).expect("fresh");

    let mut large = TypeRegistry::new();
    register_builtins(&mut large).expect("fresh");
    large.register(Position::type_info()).expect("fresh");

    assert_ne!(small.fingerprint(), large.fingerprint());
}

#[test]
fn a_registry_fingerprint_does_not_depend_on_registration_order() {
    // §2.14: two hosts that registered the same types differently must agree, or
    // the check would reject valid modules at random.
    let one = {
        let mut registry = TypeRegistry::new();
        registry.register(Position::type_info()).expect("fresh");
        registry.register(Health::type_info()).expect("fresh");
        registry
    };

    let other = {
        let mut registry = TypeRegistry::new();
        registry.register(Health::type_info()).expect("fresh");
        registry.register(Position::type_info()).expect("fresh");
        registry
    };

    assert_eq!(one.fingerprint(), other.fingerprint());
}
