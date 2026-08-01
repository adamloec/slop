//! Querying, end to end.

use slop_ecs::{Entity, World};
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

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Frozen {}

#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Name {
    text: String,
}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Velocity>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Frozen>().expect("fresh");
    world.register::<Name>().expect("fresh");

    world
}

/// Spawn an entity with a position and velocity derived from `seed`.
fn moving(world: &mut World, seed: u32) -> Entity {
    let entity = world.spawn();
    world
        .insert(
            entity,
            Position {
                x: seed as f32,
                y: 0.0,
            },
        )
        .expect("ok");
    world
        .insert(
            entity,
            Velocity {
                dx: 1.0,
                dy: seed as f32,
            },
        )
        .expect("ok");

    entity
}

#[test]
fn a_query_over_an_empty_world_yields_nothing() {
    let world = world();

    assert_eq!(world.query::<&Position>().count(), 0);
}

#[test]
fn a_query_visits_every_matching_entity() {
    let mut world = world();
    for seed in 0..10 {
        moving(&mut world, seed);
    }

    assert_eq!(world.query::<&Position>().count(), 10);
}

#[test]
fn a_query_skips_entities_missing_a_component() {
    // The filter that makes archetype storage worth having: non-matching tables
    // are skipped whole, not row by row.
    let mut world = world();

    for seed in 0..5 {
        moving(&mut world, seed);
    }
    for _ in 0..7 {
        let entity = world.spawn();
        world.insert(entity, Health { value: 1 }).expect("ok");
    }

    assert_eq!(world.query::<&Position>().count(), 5);
    assert_eq!(world.query::<&Health>().count(), 7);
    assert_eq!(world.query::<(&Position, &Velocity)>().count(), 5);
    assert_eq!(world.query::<(&Position, &Health)>().count(), 0);
}

#[test]
fn a_query_matches_archetypes_holding_more_than_it_asks_for() {
    // `contains_all`, not equality. An entity with extra components still
    // matches — otherwise adding a marker to an entity would drop it out of
    // every existing query.
    let mut world = world();

    let plain = moving(&mut world, 1);
    let decorated = moving(&mut world, 2);
    world.insert(decorated, Health { value: 9 }).expect("ok");
    world.insert(decorated, Frozen {}).expect("ok");

    assert_eq!(world.query::<&Position>().count(), 2);
    assert!(world.contains(plain));
}

#[test]
fn values_read_through_a_query_are_the_ones_stored() {
    let mut world = world();
    for seed in 0..20 {
        moving(&mut world, seed);
    }

    let mut seen: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .collect();
    seen.sort_unstable();

    assert_eq!(seen, (0..20).collect::<Vec<_>>());
}

#[test]
fn a_mutable_query_writes_through_to_storage() {
    // The thing a system does every frame.
    let mut world = world();
    for seed in 0..10 {
        moving(&mut world, seed);
    }

    for (mut position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
        position.x += velocity.dx;
        position.y += velocity.dy;
    }

    let mut seen: Vec<(u32, u32)> = world
        .query::<&Position>()
        .map(|position| (position.x as u32, position.y as u32))
        .collect();
    seen.sort_unstable();

    // Each entity started at x = seed with dx = 1 and dy = seed, so after one
    // step it is at (seed + 1, seed).
    let expected: Vec<(u32, u32)> = (0..10_u32).map(|seed| (seed + 1, seed)).collect();

    assert_eq!(seen, expected);
}

#[test]
fn a_query_can_yield_the_entity_alongside_its_components() {
    let mut world = world();
    let mut spawned = Vec::new();

    for seed in 0..8 {
        spawned.push(moving(&mut world, seed));
    }

    let mut seen: Vec<Entity> = world
        .query::<(Entity, &Position)>()
        .map(|(entity, _)| entity)
        .collect();

    seen.sort_unstable();
    spawned.sort_unstable();

    assert_eq!(seen, spawned);
}

#[test]
fn an_entity_only_query_visits_everything_including_bare_entities() {
    // `Entity` names no component, so it constrains nothing — including the
    // empty archetype, where entities with no components live.
    let mut world = world();

    world.spawn();
    world.spawn();
    moving(&mut world, 1);

    assert_eq!(world.query::<Entity>().count(), 3);
}

#[test]
fn a_zero_sized_marker_can_be_queried() {
    // Its column never allocates, so the base pointer is dangling and every row
    // resolves to the same address. Correct for a type with no bytes, and easy
    // to get wrong.
    let mut world = world();

    for seed in 0..6 {
        let entity = moving(&mut world, seed);
        if seed % 2 == 0 {
            world.insert(entity, Frozen {}).expect("ok");
        }
    }

    assert_eq!(world.query::<&Frozen>().count(), 3);
    assert_eq!(world.query::<(&Frozen, &Position)>().count(), 3);
}

#[test]
fn a_query_reads_across_several_archetypes() {
    // Three distinct component sets all holding a Position. The iterator has to
    // finish one table and resolve the next, which is where an off-by-one in
    // archetype advancement would show up.
    let mut world = world();

    for seed in 0..4 {
        moving(&mut world, seed);
    }
    for seed in 4..9 {
        let entity = world.spawn();
        world
            .insert(
                entity,
                Position {
                    x: seed as f32,
                    y: 0.0,
                },
            )
            .expect("ok");
    }
    for seed in 9..15 {
        let entity = moving(&mut world, seed);
        world.insert(entity, Health { value: seed }).expect("ok");
    }

    let mut seen: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .collect();
    seen.sort_unstable();

    assert_eq!(seen, (0..15).collect::<Vec<_>>());
    assert_eq!(world.query::<&Position>().count(), 15);
}

#[test]
fn an_archetype_emptied_by_despawns_is_skipped() {
    // Empty archetypes are kept rather than reclaimed, so the iterator must
    // step over them rather than yielding a row that is not there.
    let mut world = world();

    let entities: Vec<Entity> = (0..5).map(|seed| moving(&mut world, seed)).collect();
    for entity in &entities {
        world.despawn(*entity);
    }

    let entity = world.spawn();
    world.insert(entity, Health { value: 1 }).expect("ok");

    assert_eq!(world.query::<&Position>().count(), 0);
    assert_eq!(world.query::<&Health>().count(), 1);
}

#[test]
fn a_query_of_an_owning_component_reads_the_real_value() {
    let mut world = world();

    for index in 0..5 {
        let entity = world.spawn();
        world
            .insert(
                entity,
                Name {
                    text: format!("entity number {index} with a long enough name to allocate"),
                },
            )
            .expect("ok");
    }

    let mut seen: Vec<String> = world
        .query::<&Name>()
        .map(|name| name.text.clone())
        .collect();
    seen.sort();

    assert_eq!(seen.len(), 5);
    assert!(seen[0].starts_with("entity number 0"));
}

#[test]
fn a_mutable_query_can_replace_an_owning_component() {
    // Assigning through `&mut String` drops the old allocation and takes the
    // new one. A leak or a double free here is what Miri reports.
    let mut world = world();

    for index in 0..4 {
        let entity = world.spawn();
        world
            .insert(
                entity,
                Name {
                    text: format!("original name {index} long enough to be on the heap"),
                },
            )
            .expect("ok");
    }

    for mut name in world.query_mut::<&mut Name>() {
        name.text = format!("replaced: {}", name.text);
    }

    assert!(
        world
            .query::<&Name>()
            .all(|name| name.text.starts_with("replaced: "))
    );
}

#[test]
fn several_read_only_queries_can_be_live_at_once() {
    // `query` takes `&self`, so this compiles. It would not if read-only
    // queries needed `&mut World`, which is the reason `ReadOnlyQueryData`
    // exists at all.
    let mut world = world();
    for seed in 0..3 {
        moving(&mut world, seed);
    }

    let positions = world.query::<&Position>();
    let velocities = world.query::<&Velocity>();

    assert_eq!(positions.count(), velocities.count());
}

#[test]
#[should_panic(expected = "twice with mutable access")]
fn a_query_aliasing_one_component_panics() {
    // `(&mut Position, &Position)` would hand out an exclusive and a shared
    // reference to the same element. Rejected when the query is built.
    let mut world = world();
    moving(&mut world, 1);

    let _ = world.query_mut::<(&mut Position, &Position)>().count();
}

#[test]
fn a_wide_tuple_query_works() {
    // Four components at once, which exercises the tuple macro past the arity
    // where a copy-paste mistake would go unnoticed.
    let mut world = world();

    let entity = moving(&mut world, 3);
    world.insert(entity, Health { value: 42 }).expect("ok");
    world.insert(entity, Frozen {}).expect("ok");

    let found: Vec<(Entity, u32, f32)> = world
        .query::<(Entity, &Health, &Position, &Frozen)>()
        .map(|(entity, health, position, _)| (entity, health.value, position.x))
        .collect();

    assert_eq!(found, vec![(entity, 42, 3.0)]);
}

#[test]
fn queries_still_work_after_structural_churn() {
    // The interaction most likely to break: queries resolve base pointers per
    // archetype, and structural change reallocates columns. Since a query
    // borrows the world, the two cannot overlap — but the query built *after*
    // the churn must see the new pointers.
    let mut world = world();

    let entities: Vec<Entity> = (0..30).map(|seed| moving(&mut world, seed)).collect();

    for (index, entity) in entities.iter().enumerate() {
        if index % 2 == 0 {
            world
                .insert(
                    *entity,
                    Health {
                        value: index as u32,
                    },
                )
                .expect("ok");
        }
        if index % 3 == 0 {
            world.remove::<Velocity>(*entity);
        }
    }
    world.assert_consistent();

    assert_eq!(world.query::<&Position>().count(), 30);
    assert_eq!(world.query::<&Health>().count(), 15);

    let mut seen: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .collect();
    seen.sort_unstable();

    assert_eq!(seen, (0..30).collect::<Vec<_>>());
}
