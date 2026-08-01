//! The world, end to end.
//!
//! An integration test rather than a unit test because these are the operations
//! a game performs, and the failure mode being guarded against is not a crash —
//! it is one entity reading another's components after a structural change,
//! which presents as a gameplay bug a long way from its cause.
//!
//! `assert_consistent` runs after nearly every mutation. It checks the three
//! structures agree: every live entity occupies exactly one row, every location
//! points at a row that actually holds that entity, and every column is the same
//! length as its archetype's entity list.

use slop_ecs::{EcsError, Entity, World};
use slop_reflect::Reflect;

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Velocity {
    dx: f32,
    dy: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Health {
    value: u32,
}

/// Owns a heap allocation, so a leak or a double free is observable.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Name {
    text: String,
}

/// Zero-sized, which exercises the column path that never allocates.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Player {}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Velocity>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Name>().expect("fresh");
    world.register::<Player>().expect("fresh");

    world
}

#[test]
fn a_spawned_entity_is_alive_and_holds_nothing() {
    let mut world = world();
    let entity = world.spawn();

    assert!(world.contains(entity));
    assert_eq!(world.len(), 1);
    assert!(!world.has::<Position>(entity));
    assert_eq!(world.get::<Position>(entity), None);
    world.assert_consistent();
}

#[test]
fn a_component_can_be_inserted_and_read_back() {
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(entity, Position { x: 1.0, y: 2.0 })
        .expect("Position is registered");

    assert!(world.has::<Position>(entity));
    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 1.0, y: 2.0 })
    );
    world.assert_consistent();
}

#[test]
fn inserting_twice_replaces_rather_than_moving() {
    // No table change: the entity already lives in the archetype that holds a
    // Position, so this is a write in place and none of the migration
    // machinery runs.
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(entity, Position { x: 1.0, y: 1.0 })
        .expect("ok");
    world
        .insert(entity, Position { x: 9.0, y: 9.0 })
        .expect("ok");

    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 9.0, y: 9.0 })
    );
    assert_eq!(world.len(), 1);
    world.assert_consistent();
}

#[test]
fn a_component_can_be_mutated_in_place() {
    let mut world = world();
    let entity = world.spawn();
    world.insert(entity, Health { value: 100 }).expect("ok");

    world.get_mut::<Health>(entity).expect("present").value = 50;

    assert_eq!(world.get::<Health>(entity), Some(&Health { value: 50 }));
    world.assert_consistent();
}

#[test]
fn components_survive_migration_between_archetypes() {
    // The core of the whole design. Adding a component physically relocates
    // every existing component to a different table, and they must all arrive
    // intact.
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(entity, Position { x: 1.0, y: 2.0 })
        .expect("ok");
    world
        .insert(entity, Velocity { dx: 3.0, dy: 4.0 })
        .expect("ok");
    world.insert(entity, Health { value: 7 }).expect("ok");

    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 1.0, y: 2.0 })
    );
    assert_eq!(
        world.get::<Velocity>(entity),
        Some(&Velocity { dx: 3.0, dy: 4.0 })
    );
    assert_eq!(world.get::<Health>(entity), Some(&Health { value: 7 }));
    world.assert_consistent();
}

#[test]
fn removing_a_component_keeps_the_others() {
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(entity, Position { x: 1.0, y: 2.0 })
        .expect("ok");
    world
        .insert(entity, Velocity { dx: 3.0, dy: 4.0 })
        .expect("ok");

    assert!(world.remove::<Velocity>(entity));

    assert!(!world.has::<Velocity>(entity));
    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 1.0, y: 2.0 }),
        "the surviving component must have moved intact"
    );
    world.assert_consistent();
}

#[test]
fn removing_something_absent_reports_false() {
    let mut world = world();
    let entity = world.spawn();

    assert!(!world.remove::<Position>(entity));
    world.assert_consistent();
}

#[test]
fn migration_patches_the_entity_that_was_swapped_into_the_hole() {
    // The step that looks optional and is not. Three entities share a table;
    // when the first migrates away, the last is swapped into its row, and its
    // location must follow. Without the patch it reads the wrong row — which is
    // one entity appearing to acquire another's components.
    let mut world = world();

    let first = world.spawn();
    let second = world.spawn();
    let third = world.spawn();

    for (entity, value) in [(first, 1_u32), (second, 2), (third, 3)] {
        world.insert(entity, Health { value }).expect("ok");
    }

    // `first` leaves the {Health} table for {Health, Position}.
    world
        .insert(first, Position { x: 0.0, y: 0.0 })
        .expect("ok");

    assert_eq!(world.get::<Health>(first), Some(&Health { value: 1 }));
    assert_eq!(world.get::<Health>(second), Some(&Health { value: 2 }));
    assert_eq!(
        world.get::<Health>(third),
        Some(&Health { value: 3 }),
        "the swapped entity must still read its own component"
    );
    world.assert_consistent();
}

#[test]
fn despawning_patches_the_swapped_entity_too() {
    let mut world = world();

    let first = world.spawn();
    let second = world.spawn();
    let third = world.spawn();

    for (entity, value) in [(first, 1_u32), (second, 2), (third, 3)] {
        world.insert(entity, Health { value }).expect("ok");
    }

    assert!(world.despawn(first));

    assert!(!world.contains(first));
    assert_eq!(world.get::<Health>(second), Some(&Health { value: 2 }));
    assert_eq!(world.get::<Health>(third), Some(&Health { value: 3 }));
    assert_eq!(world.len(), 2);
    world.assert_consistent();
}

#[test]
fn a_stale_handle_stops_resolving() {
    // Generational handles, and the reason `PLAN.md` §4.1-C chose to bump on
    // free. A despawned entity's handle must not resolve even after its slot is
    // handed out again.
    let mut world = world();

    let first = world.spawn();
    world.insert(first, Health { value: 1 }).expect("ok");
    world.despawn(first);

    let reused = world.spawn();
    world.insert(reused, Health { value: 2 }).expect("ok");

    assert!(!world.contains(first), "the old handle is stale");
    assert!(world.contains(reused));
    assert_eq!(
        world.get::<Health>(first),
        None,
        "a stale handle must not read the entity that took its slot"
    );
    assert_eq!(world.get::<Health>(reused), Some(&Health { value: 2 }));
    world.assert_consistent();
}

#[test]
fn despawning_twice_is_false_rather_than_a_panic() {
    let mut world = world();
    let entity = world.spawn();

    assert!(world.despawn(entity));
    assert!(!world.despawn(entity));
    world.assert_consistent();
}

#[test]
fn an_unregistered_component_is_refused() {
    #[derive(Reflect, Debug, Clone, Copy)]
    #[repr(C)]
    struct NeverRegistered {
        value: u32,
    }

    let mut world = world();
    let entity = world.spawn();

    assert!(matches!(
        world.insert(entity, NeverRegistered { value: 1 }),
        Err(EcsError::UnregisteredComponent { .. })
    ));
    world.assert_consistent();
}

#[test]
fn inserting_on_a_dead_entity_is_an_error() {
    let mut world = world();
    let entity = world.spawn();
    world.despawn(entity);

    assert!(matches!(
        world.insert(entity, Health { value: 1 }),
        Err(EcsError::NoSuchEntity { .. })
    ));
    world.assert_consistent();
}

#[test]
fn a_component_owning_a_heap_allocation_survives_migration() {
    // Migration relocates bytes without running destructors. If it dropped
    // instead, this string would be freed and the destination would hold a
    // dangling pointer — which Miri catches and a plain test would not.
    let mut world = world();
    let entity = world.spawn();

    let text = "a string long enough to be heap allocated rather than inline".repeat(3);
    world
        .insert(entity, Name { text: text.clone() })
        .expect("ok");

    // Three migrations, each relocating the string.
    world
        .insert(entity, Position { x: 1.0, y: 1.0 })
        .expect("ok");
    world
        .insert(entity, Velocity { dx: 2.0, dy: 2.0 })
        .expect("ok");
    world.insert(entity, Health { value: 3 }).expect("ok");

    assert_eq!(
        world.get::<Name>(entity).map(|name| name.text.as_str()),
        Some(text.as_str()),
        "the string must survive being relocated three times"
    );
    world.assert_consistent();
}

#[test]
fn removing_an_owning_component_drops_it_exactly_once() {
    // The other half: a component the destination archetype does not want is
    // destroyed rather than relocated. Dropping it twice, or not at all, is
    // what Miri reports here.
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(
            entity,
            Name {
                text: String::from("dropped exactly once, please"),
            },
        )
        .expect("ok");
    world.insert(entity, Health { value: 1 }).expect("ok");

    assert!(world.remove::<Name>(entity));

    assert!(!world.has::<Name>(entity));
    assert_eq!(world.get::<Health>(entity), Some(&Health { value: 1 }));
    world.assert_consistent();
}

#[test]
fn despawning_drops_owning_components() {
    let mut world = world();
    let entity = world.spawn();

    world
        .insert(
            entity,
            Name {
                text: String::from("freed on despawn"),
            },
        )
        .expect("ok");

    assert!(world.despawn(entity));
    world.assert_consistent();
}

#[test]
fn a_zero_sized_marker_behaves_like_any_other_component() {
    // Marker components are the common case — `Player`, `Static`, `Hidden` —
    // and their column never allocates, so the pointer arithmetic that would be
    // undefined for a zero stride has to be skipped rather than performed.
    let mut world = world();
    let entity = world.spawn();

    world.insert(entity, Player {}).expect("ok");
    world.insert(entity, Health { value: 5 }).expect("ok");

    assert!(world.has::<Player>(entity));
    assert_eq!(world.get::<Player>(entity), Some(&Player {}));
    assert_eq!(world.get::<Health>(entity), Some(&Health { value: 5 }));

    assert!(world.remove::<Player>(entity));
    assert!(!world.has::<Player>(entity));
    world.assert_consistent();
}

#[test]
fn archetypes_are_shared_by_component_set_not_by_entity() {
    // The empty archetype plus one per distinct set. If insertion order
    // produced different signatures, this would grow without bound.
    let mut world = world();

    for _ in 0..100 {
        let entity = world.spawn();
        world
            .insert(entity, Position { x: 0.0, y: 0.0 })
            .expect("ok");
        world
            .insert(entity, Velocity { dx: 0.0, dy: 0.0 })
            .expect("ok");
    }

    // {}, {Position}, {Position, Velocity} — three, regardless of entity count.
    assert_eq!(world.archetypes().len(), 3);
    assert_eq!(world.len(), 100);
    world.assert_consistent();
}

#[test]
fn insertion_order_does_not_create_extra_archetypes() {
    // Signatures are sorted, so {Position, Velocity} is one table however the
    // components arrived.
    let mut world = world();

    let forwards = world.spawn();
    world
        .insert(forwards, Position { x: 0.0, y: 0.0 })
        .expect("ok");
    world
        .insert(forwards, Velocity { dx: 0.0, dy: 0.0 })
        .expect("ok");

    let backwards = world.spawn();
    world
        .insert(backwards, Velocity { dx: 0.0, dy: 0.0 })
        .expect("ok");
    world
        .insert(backwards, Position { x: 0.0, y: 0.0 })
        .expect("ok");

    // {}, {Position}, {Velocity}, {Position, Velocity} — the two singletons are
    // real intermediate steps; the pair is shared.
    assert_eq!(world.archetypes().len(), 4);
    world.assert_consistent();
}

#[test]
fn many_entities_churn_without_losing_track_of_any() {
    // The stress case, and the one most likely to expose a missing index patch:
    // repeated structural changes across a population, with every entity's
    // identity checked at the end.
    let mut world = world();
    let mut entities: Vec<(Entity, u32)> = Vec::new();

    for value in 0..200_u32 {
        let entity = world.spawn();
        world.insert(entity, Health { value }).expect("ok");
        entities.push((entity, value));
    }

    // Give every third entity a Position, moving it to another table.
    for (entity, value) in &entities {
        if value % 3 == 0 {
            world
                .insert(
                    *entity,
                    Position {
                        x: *value as f32,
                        y: 0.0,
                    },
                )
                .expect("ok");
        }
    }
    world.assert_consistent();

    // Despawn every fifth, which swap-removes across both tables.
    for (entity, value) in &entities {
        if value % 5 == 0 {
            assert!(world.despawn(*entity));
        }
    }
    world.assert_consistent();

    // Take Position away from every ninth survivor, moving it back.
    for (entity, value) in &entities {
        if value % 9 == 0 && value % 5 != 0 {
            world.remove::<Position>(*entity);
        }
    }
    world.assert_consistent();

    // Every survivor still reads its own Health, and every casualty is gone.
    for (entity, value) in &entities {
        if value % 5 == 0 {
            assert!(!world.contains(*entity), "entity {value} should be gone");
            assert_eq!(world.get::<Health>(*entity), None);
        } else {
            assert_eq!(
                world.get::<Health>(*entity),
                Some(&Health { value: *value }),
                "entity {value} lost track of its own component"
            );
        }
    }

    assert_eq!(world.len(), 200 - 40);
}

#[test]
fn a_runtime_registered_type_is_a_component_like_any_other() {
    // `DESIGN.md` §2.4's whole point: a type the host was never compiled
    // against, described entirely as data, is a first-class component. This is
    // the shape a WASM guest module's exported type table takes.
    use slop_reflect::{Transfer, TypeInfo, TypeKind};
    use std::alloc::Layout;

    let mut world = World::with_builtins();

    // No Rust type exists for this. Only a description.
    let info = TypeInfo::new(
        "guest::Inventory",
        Layout::from_size_align(8, 4).expect("valid"),
        Transfer::Blittable,
        TypeKind::Struct { fields: Vec::new() },
    );
    let type_id = info.id();
    world.register_info(info).expect("fresh");

    let entity = world.spawn();

    // Insertion goes through the typed path today, so this asserts what the
    // storage layer can already do: an archetype for a runtime type builds,
    // allocates, and holds rows.
    let signature = slop_ecs::Signature::new([type_id]);
    let archetype =
        slop_ecs::Archetype::new(signature, world.registry()).expect("the type is registered");

    assert_eq!(archetype.columns().len(), 1);
    assert_eq!(archetype.columns()[0].element_layout().size(), 8);
    assert!(
        archetype.columns()[0].is_blittable(),
        "a blittable guest type must be able to cross back over the boundary"
    );

    world.assert_consistent();
    assert!(world.contains(entity));
}
