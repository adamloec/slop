//! Component memory to a value and back, and the whole loop through text.
//!
//! `docs/DESIGN.md` §5 asks for *serialize → deserialize → compare*. `slop-reflect`
//! tests the text half; this tests the memory half and then the two joined:
//!
//! ```text
//! component  →  Value  →  text  →  Value  →  component
//! ```
//!
//! Two failure modes carry most of the weight:
//!
//! - **An owning component is neither leaked nor double-freed.** Reading a
//!   `String` component clones it; writing one installs a new allocation. A
//!   count that comes out wrong is a leak or a use-after-free.
//! - **A rejected value leaves nothing behind.** Writing is validated in full
//!   first, so a value that fails half way through a struct must not have
//!   written the fields before it — those may own allocations nobody can now
//!   reach.

use std::cell::RefCell;
use std::rc::Rc;

use slop_ecs::{EcsError, Entity, ValueError, World};
use slop_reflect::{Reflect, Struct, TypeInfo, TypeRegistry, Value, from_text, to_text};

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Health {
    current: u32,
    maximum: u32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Label {
    text: String,
}

/// Nested, so the walk recurses rather than only touching primitives.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Body {
    position: Position,
    health: Health,
}

/// Mixes an owning field with plain data, which is where a half-written struct
/// would leak.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Named {
    name: String,
    health: Health,
}

/// Zero-sized, which allocates no scratch space at all.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Marker {}

/// Every width, so no primitive goes unread.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
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
    l: char,
}

/// Counts its own destructor, so a leak or a double free is a failed assertion
/// rather than something only a sanitizer sees.
#[derive(Debug, Clone)]
struct Tracked {
    drops: Rc<RefCell<u32>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        *self.drops.borrow_mut() += 1;
    }
}

// SAFETY: the path is unique to this test, `Owning` is correct because an `Rc`
// means nothing outside this address space, and the destructor is `Tracked`'s.
unsafe impl Reflect for Tracked {
    const PATH: &'static str = "slop_ecs::tests::serialize::Tracked";
    const TRANSFER: slop_reflect::Transfer = slop_reflect::Transfer::Owning;

    fn type_info() -> TypeInfo {
        unsafe fn drop_tracked(pointer: *mut u8) {
            // SAFETY: only ever called on an initialized, aligned `Tracked`.
            unsafe { std::ptr::drop_in_place(pointer.cast::<Tracked>()) };
        }

        // SAFETY: the layout and destructor are both `Tracked`'s own.
        unsafe {
            TypeInfo::with_drop(
                Self::PATH,
                std::alloc::Layout::new::<Self>(),
                Self::TRANSFER,
                slop_reflect::TypeKind::Opaque,
                drop_tracked,
            )
        }
    }
}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Label>().expect("fresh");
    world.register::<Body>().expect("fresh");
    world.register::<Named>().expect("fresh");
    world.register::<Marker>().expect("fresh");
    world.register::<Widths>().expect("fresh");
    world.register::<Tracked>().expect("fresh");

    world
}

/// Insert a component, read it back as a value, and check it survived.
#[track_caller]
fn value_round_trips<T: Reflect + std::fmt::Debug + PartialEq + Clone>(
    world: &mut World,
    component: T,
) -> Value {
    let entity = world.spawn();
    world.insert(entity, component.clone()).expect("registered");

    let value = world
        .component_value(entity, T::type_id())
        .expect("readable");

    // Back into a second entity through the value, and compare the components.
    let other = world.spawn();
    world
        .insert_value(other, T::type_id(), &value)
        .expect("the value came from this very type");

    assert_eq!(
        world.get::<T>(other),
        Some(&component),
        "the value did not survive the trip through memory"
    );
    world.assert_consistent();

    value
}

#[test]
fn a_plain_component_round_trips_through_a_value() {
    let mut world = world();

    value_round_trips(
        &mut world,
        Position {
            x: 1.0,
            y: -2.5,
            z: 0.0,
        },
    );
}

#[test]
fn every_primitive_width_round_trips_through_memory() {
    let mut world = world();

    value_round_trips(
        &mut world,
        Widths {
            a: u8::MAX,
            b: u16::MAX,
            c: u32::MAX,
            d: u64::MAX,
            e: i8::MIN,
            f: i16::MIN,
            g: i32::MIN,
            h: i64::MIN,
            i: f32::MIN,
            j: f64::MAX,
            k: true,
            l: '日',
        },
    );
}

#[test]
fn a_nested_component_round_trips() {
    let mut world = world();

    let value = value_round_trips(
        &mut world,
        Body {
            position: Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            health: Health {
                current: 50,
                maximum: 100,
            },
        },
    );

    // The nesting really is nested rather than flattened.
    let nested = value
        .as_struct()
        .and_then(|body| body.field("position"))
        .and_then(Value::as_struct)
        .expect("a struct inside a struct");

    assert_eq!(nested.field("x"), Some(&Value::F32(1.0)));
}

#[test]
fn a_zero_sized_component_round_trips_without_scratch_space() {
    let mut world = world();

    value_round_trips(&mut world, Marker {});
}

#[test]
fn an_owning_component_round_trips_and_is_dropped_exactly_once() {
    let drops = Rc::new(RefCell::new(0));

    {
        let mut world = world();

        let entity = world.spawn();
        world
            .insert(
                entity,
                Label {
                    text: "a heap allocation".to_owned(),
                },
            )
            .expect("registered");

        // Reading clones, so the original is untouched.
        let value = world
            .component_value(entity, Label::type_id())
            .expect("readable");

        assert_eq!(
            world.get::<Label>(entity).map(|label| label.text.as_str()),
            Some("a heap allocation"),
            "reading must not move the value out"
        );

        let other = world.spawn();
        world
            .insert_value(other, Label::type_id(), &value)
            .expect("valid");

        assert_eq!(
            world.get::<Label>(other).map(|label| label.text.as_str()),
            Some("a heap allocation")
        );
        world.assert_consistent();
    }

    // A counted type through the same path, so the destructor arithmetic is
    // checked rather than assumed.
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(
            entity,
            Tracked {
                drops: Rc::clone(&drops),
            },
        )
        .expect("registered");

    assert_eq!(*drops.borrow(), 0);
    drop(world);
    assert_eq!(*drops.borrow(), 1, "one component, one destructor");
}

#[test]
fn the_whole_loop_through_text_preserves_the_component() {
    // component → Value → text → Value → component, which is §5's round trip
    // end to end rather than either half of it.
    let mut world = world();

    let original = Named {
        name: "player \"one\"\nsecond line".to_owned(),
        health: Health {
            current: 42,
            maximum: 99,
        },
    };

    let entity = world.spawn();
    world.insert(entity, original.clone()).expect("registered");

    let value = world
        .component_value(entity, Named::type_id())
        .expect("readable");
    let text = to_text(&value);
    let parsed =
        from_text(&text, &Named::type_info(), world.registry()).expect("what we just wrote");

    assert_eq!(parsed, value, "the text half changed the value:\n{text}");

    let other = world.spawn();
    world
        .insert_value(other, Named::type_id(), &parsed)
        .expect("valid");

    assert_eq!(world.get::<Named>(other), Some(&original));
    world.assert_consistent();
}

#[test]
fn every_component_in_a_world_round_trips_through_text() {
    // §5 asks for this to run "automatically for all registered types". This is
    // that, driven from the world's own contents rather than a hand-written
    // list — so a component added later is covered without editing the test.
    let mut world = world();

    let entity = world.spawn();
    world
        .insert(
            entity,
            Position {
                x: 1.5,
                y: -0.0,
                z: f32::MAX,
            },
        )
        .expect("registered");
    world
        .insert(
            entity,
            Health {
                current: 1,
                maximum: u32::MAX,
            },
        )
        .expect("registered");
    world
        .insert(
            entity,
            Label {
                text: "unicode 日 and a tab\t".to_owned(),
            },
        )
        .expect("registered");
    world.insert(entity, Marker {}).expect("registered");

    let registry: &TypeRegistry = world.registry();
    let signature: Vec<_> = world
        .archetypes()
        .iter()
        .find(|archetype| archetype.len() == 1)
        .expect("one populated archetype")
        .signature()
        .types()
        .to_vec();

    assert!(!signature.is_empty(), "the test would prove nothing empty");

    for type_id in signature {
        let info = registry.get(type_id).expect("registered");
        let value = world
            .component_value(entity, type_id)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", info.path()));

        let text = to_text(&value);
        let back = from_text(&text, info, registry)
            .unwrap_or_else(|error| panic!("{} could not be read back: {error}", info.path()));

        assert_eq!(back, value, "{} did not round trip:\n{text}", info.path());
    }
}

#[test]
fn a_resource_round_trips_through_a_value() {
    let mut world = world();
    world
        .insert_resource(Health {
            current: 7,
            maximum: 9,
        })
        .expect("registered");

    let value = world
        .resource_value(Health::type_id())
        .expect("readable")
        .expect("present");

    let text = to_text(&value);
    let parsed =
        from_text(&text, &Health::type_info(), world.registry()).expect("what we just wrote");

    world
        .insert_resource_value(Health::type_id(), &parsed)
        .expect("valid");

    assert_eq!(
        world.resource::<Health>(),
        Some(&Health {
            current: 7,
            maximum: 9
        })
    );
    world.assert_consistent();
}

#[test]
fn an_absent_resource_reads_as_none_rather_than_an_error() {
    let world = world();

    assert_eq!(world.resource_value(Health::type_id()), Ok(None));
}

#[test]
fn an_opaque_component_cannot_be_read() {
    // `Opaque` means "its internals are the owning crate's business", so there
    // is nothing to serialize — reported rather than silently skipped, because a
    // component that vanishes from a save file is the failure §2.4 exists to
    // prevent.
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(
            entity,
            Tracked {
                drops: Rc::new(RefCell::new(0)),
            },
        )
        .expect("registered");

    let error = world
        .component_value(entity, Tracked::type_id())
        .expect_err("opaque");

    assert!(matches!(error, EcsError::Value(ValueError::Opaque { .. })));
}

#[test]
fn a_value_of_the_wrong_shape_is_rejected() {
    let mut world = world();
    let entity = world.spawn();

    // A `Health` value offered as a `Position`.
    let wrong = Value::Struct(Struct::new(
        "serialize::Health",
        vec![
            ("current".to_owned(), Value::U32(1)),
            ("maximum".to_owned(), Value::U32(2)),
        ],
    ));

    let error = world
        .insert_value(entity, Position::type_id(), &wrong)
        .expect_err("the paths disagree");

    assert!(matches!(
        error,
        EcsError::Value(ValueError::Mismatch { .. })
    ));
    assert!(!world.has::<Position>(entity), "nothing was inserted");
    world.assert_consistent();
}

#[test]
fn a_value_with_a_wrong_field_type_is_rejected() {
    let mut world = world();
    let entity = world.spawn();

    let wrong = Value::Struct(Struct::new(
        "serialize::Health",
        vec![
            // `Health::current` is a `u32`.
            ("current".to_owned(), Value::F32(1.0)),
            ("maximum".to_owned(), Value::U32(2)),
        ],
    ));

    let error = world
        .insert_value(entity, Health::type_id(), &wrong)
        .expect_err("current is not an f32");

    assert!(matches!(
        error,
        EcsError::Value(ValueError::Mismatch { .. })
    ));
    assert!(!world.has::<Health>(entity));
}

#[test]
fn a_value_missing_a_field_is_rejected_before_anything_is_written() {
    // The reason writing is validated in full first. `Named`'s first field owns
    // a heap allocation; if the write started and then failed on the second
    // field, that allocation would be stranded with nothing able to reach it.
    let drops = Rc::new(RefCell::new(0));
    let mut world = world();
    let entity = world.spawn();

    let incomplete = Value::Struct(Struct::new(
        "serialize::Named",
        vec![("name".to_owned(), Value::String("stranded".to_owned()))],
    ));

    let error = world
        .insert_value(entity, Named::type_id(), &incomplete)
        .expect_err("health is absent");

    assert!(matches!(
        error,
        EcsError::Value(ValueError::MissingField { .. })
    ));
    assert!(!world.has::<Named>(entity), "nothing was inserted");
    assert_eq!(*drops.borrow(), 0);
    world.assert_consistent();
}

#[test]
fn inserting_a_value_on_a_dead_entity_is_an_error() {
    let mut world = world();
    let entity = world.spawn();
    world.despawn(entity);

    let value = Value::Struct(Struct::new(
        "serialize::Health",
        vec![
            ("current".to_owned(), Value::U32(1)),
            ("maximum".to_owned(), Value::U32(2)),
        ],
    ));

    assert_eq!(
        world.insert_value(entity, Health::type_id(), &value),
        Err(EcsError::NoSuchEntity { entity })
    );
}

#[test]
fn reading_a_component_the_entity_does_not_have_is_an_error() {
    let mut world = world();
    let entity = world.spawn();

    assert_eq!(
        world.component_value(entity, Health::type_id()),
        Err(EcsError::MissingComponent {
            entity,
            type_id: Health::type_id()
        })
    );
}

#[test]
fn an_unregistered_type_is_an_error_rather_than_a_guess() {
    #[derive(Reflect, Debug)]
    #[repr(C)]
    struct Unknown {
        value: u32,
    }

    let mut world = world();
    let entity = world.spawn();

    assert_eq!(
        world.component_value(entity, Unknown::type_id()),
        Err(EcsError::UnregisteredComponent {
            type_id: Unknown::type_id()
        })
    );
}

#[test]
fn inserting_a_value_over_an_existing_component_replaces_it() {
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(
            entity,
            Label {
                text: "first".to_owned(),
            },
        )
        .expect("registered");

    let replacement = Value::Struct(Struct::new(
        "serialize::Label",
        vec![("text".to_owned(), Value::String("second".to_owned()))],
    ));

    world
        .insert_value(entity, Label::type_id(), &replacement)
        .expect("valid");

    assert_eq!(
        world.get::<Label>(entity).map(|label| label.text.as_str()),
        Some("second")
    );
    world.assert_consistent();
}

#[test]
fn many_components_survive_being_rebuilt_from_values() {
    // Drives the whole path over enough entities that a stranded allocation or a
    // double free shows up under Miri.
    let mut world = world();

    let originals: Vec<Named> = (0..16)
        .map(|index| Named {
            name: format!("entity {index}"),
            health: Health {
                current: index,
                maximum: 100,
            },
        })
        .collect();

    let sources: Vec<Entity> = originals
        .iter()
        .map(|named| {
            let entity = world.spawn();
            world.insert(entity, named.clone()).expect("registered");
            entity
        })
        .collect();

    let values: Vec<Value> = sources
        .iter()
        .map(|entity| {
            world
                .component_value(*entity, Named::type_id())
                .expect("readable")
        })
        .collect();

    for entity in sources {
        world.despawn(entity);
    }

    for (original, value) in originals.iter().zip(&values) {
        let entity = world.spawn();
        world
            .insert_value(entity, Named::type_id(), value)
            .expect("valid");

        assert_eq!(world.get::<Named>(entity), Some(original));
    }

    world.assert_consistent();
}
