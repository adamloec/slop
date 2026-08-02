//! A whole world to text and back.
//!
//! This is M1's exit condition from `docs/DESIGN.md` §6 — *a scene round-trips*
//! — with the caveat that the crate calls it a world, because §4 reserves
//! "scene" for the runtime spatial structure.
//!
//! The property is that a world written and read back holds the same entities
//! with the same components. Entity *ids* deliberately do not survive: a runtime
//! id carries a generation and a slot that mean nothing in the next process, so
//! the file numbers entities itself and the load reports the mapping.

use slop_ecs::document::{self, LoadError};
use slop_ecs::{EcsError, ValueError, World};
use slop_reflect::{Reflect, TypeInfo};

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

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Gravity {
    value: f32,
}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Label {
    text: String,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Player {}

/// A component nothing can look inside, so it cannot be written.
#[derive(Debug)]
struct Handle {
    _raw: u64,
}

// SAFETY: the path is unique to this test, `Owning` is conservative and correct,
// and no destructor is installed because `Handle` needs none.
unsafe impl Reflect for Handle {
    const PATH: &'static str = "slop_ecs::tests::document::Handle";
    const TRANSFER: slop_reflect::Transfer = slop_reflect::Transfer::Owning;

    fn type_info() -> TypeInfo {
        TypeInfo::new(
            Self::PATH,
            std::alloc::Layout::new::<Self>(),
            Self::TRANSFER,
            slop_reflect::TypeKind::Opaque,
        )
    }
}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Gravity>().expect("fresh");
    world.register::<Label>().expect("fresh");
    world.register::<Player>().expect("fresh");
    world.register::<Handle>().expect("fresh");

    world
}

/// A world with a bit of everything: several archetypes, an owning component, a
/// marker, and a resource.
fn populated() -> World {
    let mut world = world();

    world
        .insert_resource(Gravity { value: -9.81 })
        .expect("registered");

    let player = world.spawn();
    world
        .insert(
            player,
            Position {
                x: 1.0,
                y: 2.0,
                z: -0.5,
            },
        )
        .expect("registered");
    world
        .insert(
            player,
            Health {
                current: 80,
                maximum: 100,
            },
        )
        .expect("registered");
    world
        .insert(
            player,
            Label {
                text: "the \"player\"\nwith a newline".to_owned(),
            },
        )
        .expect("registered");
    world.insert(player, Player {}).expect("registered");

    for index in 0..3 {
        let rock = world.spawn();
        world
            .insert(
                rock,
                Position {
                    x: index as f32,
                    y: 0.0,
                    z: 0.0,
                },
            )
            .expect("registered");
    }

    let bare = world.spawn();
    world
        .insert(
            bare,
            Health {
                current: 1,
                maximum: 1,
            },
        )
        .expect("registered");

    world
}

/// Every entity's components, as sorted text, so two worlds can be compared
/// without depending on entity order or ids.
fn contents(world: &World) -> Vec<String> {
    let mut all: Vec<String> = world
        .archetypes()
        .iter()
        .flat_map(|archetype| {
            archetype.entities().iter().map(|entity| {
                let mut parts: Vec<String> = archetype
                    .signature()
                    .types()
                    .iter()
                    .filter_map(|type_id| {
                        let value = world.component_value(*entity, *type_id).ok()?;
                        Some(slop_reflect::to_text(&value))
                    })
                    .collect();
                parts.sort();

                parts.join(" | ")
            })
        })
        .collect();
    all.sort();

    all
}

#[test]
fn a_world_round_trips() {
    // M1's exit condition.
    let original = populated();
    let saved = document::save(&original);

    let mut loaded = world();
    document::load(&saved.text, &mut loaded).unwrap_or_else(|error| {
        panic!("could not read back:\n{}\n{error}", saved.text);
    });

    assert_eq!(
        contents(&loaded),
        contents(&original),
        "the world changed:\n{}",
        saved.text
    );
    assert_eq!(loaded.len(), original.len());
    assert_eq!(
        loaded.resource::<Gravity>(),
        Some(&Gravity { value: -9.81 })
    );
    loaded.assert_consistent();
}

#[test]
fn saving_twice_produces_the_same_text() {
    // §2.14: the same world produces the same file, or a save is a spurious diff
    // every time.
    let world = populated();

    assert_eq!(document::save(&world).text, document::save(&world).text);
}

#[test]
fn a_round_tripped_world_saves_identically() {
    // The stronger property: the file is a fixed point. If loading reordered
    // fields or lost precision, this is where it would show.
    let original = populated();
    let first = document::save(&original);

    let mut loaded = world();
    document::load(&first.text, &mut loaded).expect("valid");

    let second = document::save(&loaded);

    assert_eq!(second.text, first.text);
}

#[test]
fn an_empty_world_round_trips() {
    let original = world();
    let saved = document::save(&original);

    let mut loaded = world();
    let report = document::load(&saved.text, &mut loaded).expect("valid");

    assert!(loaded.is_empty());
    assert!(report.entities.is_empty());
    assert_eq!(saved.text.trim(), "slop world 1");
}

#[test]
fn the_load_reports_which_entity_each_file_index_became() {
    // The remapping table an entity-valued component will resolve through once
    // hierarchy lands.
    let original = populated();
    let saved = document::save(&original);

    let mut loaded = world();
    let report = document::load(&saved.text, &mut loaded).expect("valid");

    assert_eq!(report.entities.len(), original.len());
    for entity in report.entities.values() {
        assert!(loaded.contains(*entity), "a reported entity is not alive");
    }
}

#[test]
fn loading_is_additive_rather_than_replacing() {
    // There is no clear-and-load, because a half-cleared world after a failed
    // load would be worse than either outcome. Replacing means loading into a
    // fresh world.
    let saved = document::save(&populated());

    let mut world = populated();
    let before = world.len();

    document::load(&saved.text, &mut world).expect("valid");

    assert_eq!(world.len(), before * 2);
    world.assert_consistent();
}

#[test]
fn an_opaque_component_is_reported_rather_than_dropped_silently() {
    // A runtime-only component must not make the world unsaveable, and must not
    // vanish without anyone being told — which is the failure §2.4 exists for.
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(
            entity,
            Health {
                current: 1,
                maximum: 2,
            },
        )
        .expect("registered");
    world
        .insert(entity, Handle { _raw: 7 })
        .expect("registered");

    let saved = document::save(&world);

    assert_eq!(saved.skipped.len(), 1);
    assert_eq!(saved.skipped[0].as_str(), Handle::PATH);
    assert!(
        saved.text.contains("document::Health"),
        "the rest of the entity still saved:\n{}",
        saved.text
    );

    // And it reads back, minus the part that could not be written.
    let mut loaded = world;
    document::load(&saved.text, &mut loaded).expect("valid");
    loaded.assert_consistent();
}

#[test]
fn resources_are_written_and_restored() {
    let mut original = world();
    original
        .insert_resource(Gravity { value: 1.5 })
        .expect("registered");
    original
        .insert_resource(Health {
            current: 3,
            maximum: 4,
        })
        .expect("registered");

    let saved = document::save(&original);

    let mut loaded = world();
    document::load(&saved.text, &mut loaded).expect("valid");

    assert_eq!(loaded.resource::<Gravity>(), Some(&Gravity { value: 1.5 }));
    assert_eq!(
        loaded.resource::<Health>(),
        Some(&Health {
            current: 3,
            maximum: 4
        })
    );
}

#[test]
fn a_zero_sized_component_survives() {
    let mut original = world();
    let entity = original.spawn();
    original.insert(entity, Player {}).expect("registered");

    let saved = document::save(&original);

    let mut loaded = world();
    document::load(&saved.text, &mut loaded).expect("valid");

    assert_eq!(loaded.query::<&Player>().count(), 1, "{}", saved.text);
}

#[test]
fn an_owning_component_survives_with_its_escapes() {
    let mut original = world();
    let entity = original.spawn();
    original
        .insert(
            entity,
            Label {
                text: "quotes \" backslash \\ tab \t newline \n done".to_owned(),
            },
        )
        .expect("registered");

    let saved = document::save(&original);

    let mut loaded = world();
    document::load(&saved.text, &mut loaded).expect("valid");

    let text: Vec<&str> = loaded
        .query::<&Label>()
        .map(|label| label.text.as_str())
        .collect();

    assert_eq!(text, vec!["quotes \" backslash \\ tab \t newline \n done"]);
}

#[test]
fn the_file_is_readable() {
    // Text was chosen for diffability, so the shape is worth pinning.
    let mut world = world();
    world
        .insert_resource(Gravity { value: -9.81 })
        .expect("registered");
    let entity = world.spawn();
    world
        .insert(
            entity,
            Health {
                current: 1,
                maximum: 2,
            },
        )
        .expect("registered");

    assert_eq!(
        document::save(&world).text,
        "slop world 1\n\
         \n\
         resource document::Gravity {\n\
         \x20   value: -9.81,\n\
         }\n\
         \n\
         entity 0 {\n\
         \x20   document::Health {\n\
         \x20       current: 1,\n\
         \x20       maximum: 2,\n\
         \x20   }\n\
         }\n"
    );
}

#[test]
fn a_hand_written_file_loads() {
    // The point of text: someone can type one.
    let text = "
        // a world someone wrote by hand
        slop world 1

        resource document::Gravity { value: -1.0 }

        entity 7 {
            document::Health { maximum: 50, current: 25 }
            document::Player {}
        }
    ";

    let mut world = world();
    let report = document::load(text, &mut world).expect("valid");

    let entity = report.entities[&7];

    assert_eq!(
        world.get::<Health>(entity),
        Some(&Health {
            current: 25,
            maximum: 50
        })
    );
    assert!(world.has::<Player>(entity));
    assert_eq!(world.resource::<Gravity>(), Some(&Gravity { value: -1.0 }));
    world.assert_consistent();
}

#[test]
fn a_missing_header_is_rejected() {
    let mut world = world();

    let error = document::load("entity 0 {}", &mut world).expect_err("no header");

    assert!(
        matches!(error, LoadError::Malformed { line: 1, .. }),
        "{error}"
    );
    assert!(world.is_empty());
}

#[test]
fn a_future_version_is_rejected_by_number() {
    // What lets an old build say "this file is newer than me" rather than
    // "this file is broken".
    let mut world = world();

    let error = document::load("slop world 2", &mut world).expect_err("version 2");

    assert!(error.to_string().contains('2'), "{error}");
}

#[test]
fn an_unknown_type_is_reported_rather_than_skipped() {
    // A component silently dropped on load is silently lost on the next save.
    let mut world = world();

    let error = document::load(
        "slop world 1\nentity 0 { game::NotRegistered { x: 1 } }",
        &mut world,
    )
    .expect_err("unknown type");

    assert!(
        matches!(error, LoadError::UnknownType { ref path, .. } if path == "game::NotRegistered"),
        "{error}"
    );
    assert!(world.is_empty());
}

#[test]
fn nothing_is_spawned_when_a_load_fails_part_way() {
    // The check-then-commit property, at file scale: the whole file is parsed
    // and every value checked before the world is touched, so a bad field near
    // the end leaves no half-loaded remains.
    let mut world = world();

    let error = document::load(
        "slop world 1\n\
         entity 0 { document::Health { current: 1, maximum: 2 } }\n\
         entity 1 { document::Health { current: 1, maximum: 99999999999999 } }\n",
        &mut world,
    )
    .expect_err("the second entity does not fit in a u32");

    assert!(matches!(error, LoadError::Text(_)), "{error}");
    assert!(
        world.is_empty(),
        "the first entity was spawned before the second failed"
    );
    world.assert_consistent();
}

#[test]
fn a_duplicate_entity_index_is_rejected() {
    // Two entities cannot share an index, or the remapping table would have to
    // pick one and a reference to the other would resolve wrongly.
    let mut world = world();

    let error = document::load("slop world 1\nentity 0 {}\nentity 0 {}", &mut world)
        .expect_err("index 0 twice");

    assert!(error.to_string().contains("twice"), "{error}");
    assert!(world.is_empty());
}

#[test]
fn an_unclosed_entity_is_rejected() {
    let mut world = world();

    let error = document::load("slop world 1\nentity 0 { document::Player {}", &mut world)
        .expect_err("no closing brace");

    assert!(error.to_string().contains("not closed"), "{error}");
}

#[test]
fn a_value_that_does_not_match_its_type_is_rejected() {
    let mut world = world();

    let error = document::load(
        "slop world 1\nentity 0 { document::Health { current: 1 } }",
        &mut world,
    )
    .expect_err("maximum is missing");

    assert!(error.to_string().contains("maximum"), "{error}");
    assert!(world.is_empty());
}

#[test]
fn a_resource_the_world_refuses_surfaces_as_an_ecs_error() {
    // `Handle` is opaque, so it can be named in a file but not built.
    let mut world = world();

    let error = document::load(
        "slop world 1\nresource slop_ecs::tests::document::Handle 0",
        &mut world,
    )
    .expect_err("opaque");

    assert!(
        matches!(
            error,
            LoadError::Text(_) | LoadError::World(EcsError::Value(ValueError::Opaque { .. }))
        ),
        "{error}"
    );
}

#[test]
fn a_large_world_round_trips_intact() {
    let mut original = world();

    for index in 0..200_u32 {
        let entity = original.spawn();
        original
            .insert(
                entity,
                Position {
                    x: index as f32,
                    y: -(index as f32),
                    z: 0.5,
                },
            )
            .expect("registered");

        if index % 2 == 0 {
            original
                .insert(
                    entity,
                    Health {
                        current: index,
                        maximum: 500,
                    },
                )
                .expect("registered");
        }
        if index % 3 == 0 {
            original
                .insert(
                    entity,
                    Label {
                        text: format!("entity {index}"),
                    },
                )
                .expect("registered");
        }
        if index % 5 == 0 {
            original.insert(entity, Player {}).expect("registered");
        }
    }

    let saved = document::save(&original);
    assert!(saved.skipped.is_empty());

    let mut loaded = world();
    document::load(&saved.text, &mut loaded).expect("valid");

    assert_eq!(loaded.len(), original.len());
    assert_eq!(contents(&loaded), contents(&original));
    loaded.assert_consistent();
}
