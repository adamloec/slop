//! Narrowing a query, end to end.
//!
//! The property under test throughout is that a filter changes *which entities*
//! are visited and nothing else — not what is yielded, not what is readable, and
//! not the order. A world with several overlapping archetypes is built once so
//! every test asks its question against the same population.

use slop_ecs::{Entity, Or, With, Without, World};
use slop_reflect::Reflect;

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Velocity {
    dx: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Health {
    value: u32,
}

/// Markers, which is what filters mostly exist for.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Player {}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Enemy {}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Frozen {}

/// Owns a heap allocation, so a filtered query is exercised over a non-blittable
/// column too.
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
    world.register::<Player>().expect("fresh");
    world.register::<Enemy>().expect("fresh");
    world.register::<Frozen>().expect("fresh");
    world.register::<Name>().expect("fresh");

    world
}

/// A world of six entities across five archetypes.
///
/// | `x` | Position | Player | Enemy | Frozen | Velocity |
/// |---|---|---|---|---|---|
/// | 1 | ✓ | ✓ | | | |
/// | 2 | ✓ | ✓ | | ✓ | |
/// | 3 | ✓ | | ✓ | | |
/// | 4 | ✓ | | ✓ | ✓ | |
/// | 5 | ✓ | | | | ✓ |
/// | 6 | ✓ | | | | |
fn populated() -> World {
    let mut world = world();

    let mut place = |x: f32| {
        let entity = world.spawn();
        world.insert(entity, Position { x }).expect("ok");
        entity
    };

    let one = place(1.0);
    let two = place(2.0);
    let three = place(3.0);
    let four = place(4.0);
    let five = place(5.0);
    place(6.0);

    world.insert(one, Player {}).expect("ok");
    world.insert(two, Player {}).expect("ok");
    world.insert(two, Frozen {}).expect("ok");
    world.insert(three, Enemy {}).expect("ok");
    world.insert(four, Enemy {}).expect("ok");
    world.insert(four, Frozen {}).expect("ok");
    world.insert(five, Velocity { dx: 1.0 }).expect("ok");

    world
}

/// The `x` values a query visited, sorted so archetype order is not asserted.
fn sorted(values: impl Iterator<Item = f32>) -> Vec<u32> {
    let mut values: Vec<u32> = values.map(|x| x as u32).collect();
    values.sort_unstable();

    values
}

#[test]
fn an_unfiltered_query_visits_everything() {
    let world = populated();

    let visited = sorted(world.query::<&Position>().map(|position| position.x));

    assert_eq!(visited, vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn with_narrows_to_archetypes_holding_the_marker() {
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .with::<Player>()
            .map(|position| position.x),
    );

    assert_eq!(visited, vec![1, 2]);
}

#[test]
fn without_narrows_to_archetypes_lacking_it() {
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .without::<Frozen>()
            .map(|position| position.x),
    );

    assert_eq!(visited, vec![1, 3, 5, 6]);
}

#[test]
fn with_and_without_compose() {
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .with::<Player>()
            .without::<Frozen>()
            .map(|position| position.x),
    );

    assert_eq!(visited, vec![1]);
}

#[test]
fn order_of_narrowing_does_not_matter() {
    let world = populated();

    let one = sorted(
        world
            .query::<&Position>()
            .with::<Player>()
            .without::<Frozen>()
            .map(|position| position.x),
    );
    let other = sorted(
        world
            .query::<&Position>()
            .without::<Frozen>()
            .with::<Player>()
            .map(|position| position.x),
    );

    assert_eq!(one, other);
}

#[test]
fn or_is_a_disjunction_where_tuples_conjoin() {
    let world = populated();

    let either = sorted(
        world
            .query::<&Position>()
            .filtered::<Or<(With<Player>, With<Enemy>)>>()
            .map(|position| position.x),
    );
    let both = sorted(
        world
            .query::<&Position>()
            .filtered::<(With<Player>, With<Enemy>)>()
            .map(|position| position.x),
    );

    assert_eq!(either, vec![1, 2, 3, 4]);
    assert_eq!(both, Vec::<u32>::new(), "nothing is both");
}

#[test]
fn or_mixes_with_and_without() {
    // Everything that is not frozen, plus the frozen player — so only the frozen
    // enemy is excluded. Written by naming the filter types, which is the only
    // place `Without` is spelled out rather than reached through the builder.
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .filtered::<Or<(Without<Frozen>, With<Player>)>>()
            .map(|position| position.x),
    );

    assert_eq!(visited, vec![1, 2, 3, 5, 6]);
}

#[test]
fn a_filter_composes_with_or() {
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .filtered::<Or<(With<Player>, With<Enemy>)>>()
            .without::<Frozen>()
            .map(|position| position.x),
    );

    assert_eq!(visited, vec![1, 3]);
}

#[test]
fn contradictory_filters_yield_nothing_rather_than_failing() {
    let world = populated();

    let visited = sorted(
        world
            .query::<&Position>()
            .with::<Player>()
            .without::<Player>()
            .map(|position| position.x),
    );

    assert_eq!(visited, Vec::<u32>::new());
}

#[test]
fn a_filter_narrows_without_changing_what_is_yielded() {
    let world = populated();

    // The same tuple shape as the unfiltered query — a filter contributes
    // nothing to unpack, which is why it is not `QueryData`.
    let visited: Vec<(u32, f32)> = world
        .query::<(Entity, &Position)>()
        .with::<Player>()
        .map(|(entity, position)| (entity.index(), position.x))
        .collect();

    assert_eq!(visited.len(), 2);
    for (_, x) in &visited {
        assert!(*x == 1.0 || *x == 2.0);
    }
}

#[test]
fn a_mutable_query_can_be_narrowed_and_writes_through() {
    let mut world = populated();

    for mut position in world.query_mut::<&mut Position>().with::<Player>() {
        position.x += 100.0;
    }

    let visited = sorted(world.query::<&Position>().map(|position| position.x));

    assert_eq!(visited, vec![3, 4, 5, 6, 101, 102]);
    world.assert_consistent();
}

#[test]
fn filtering_on_a_component_the_query_also_reads_is_allowed() {
    // `With<Health>` reads no `Health`, so this is not an aliasing conflict —
    // it is merely redundant, and forbidding it would be surprising.
    let mut world = world();
    let entity = world.spawn();
    world.insert(entity, Health { value: 7 }).expect("ok");

    let values: Vec<u32> = world
        .query::<&Health>()
        .with::<Health>()
        .map(|health| health.value)
        .collect();

    assert_eq!(values, vec![7]);
}

#[test]
fn a_filter_on_an_unregistered_component_matches_nothing() {
    // No archetype can hold a type nothing ever inserted, so `With` finds none
    // and `Without` finds all. The registry is not consulted, because a filter
    // asks about signatures rather than about layouts.
    #[derive(Reflect, Debug, Clone, Copy)]
    #[repr(C)]
    struct NeverUsed {}

    let world = populated();

    assert_eq!(world.query::<&Position>().with::<NeverUsed>().count(), 0);
    assert_eq!(world.query::<&Position>().without::<NeverUsed>().count(), 6);
}

#[test]
#[should_panic(expected = "must be narrowed before it is iterated")]
fn narrowing_a_query_that_has_started_is_rejected() {
    let world = populated();

    let mut query = world.query::<&Position>();
    query.next();

    let _ = query.with::<Player>();
}

#[test]
fn an_optional_component_does_not_narrow_the_query() {
    let world = populated();

    let visited: Vec<(u32, bool)> = world
        .query::<(&Position, Option<&Velocity>)>()
        .map(|(position, velocity)| (position.x as u32, velocity.is_some()))
        .collect();

    let mut visited = visited;
    visited.sort_unstable();

    assert_eq!(
        visited,
        vec![
            (1, false),
            (2, false),
            (3, false),
            (4, false),
            (5, true),
            (6, false)
        ]
    );
}

#[test]
fn an_optional_component_reads_the_real_value_where_it_exists() {
    let world = populated();

    let speeds: Vec<f32> = world
        .query::<Option<&Velocity>>()
        .filter_map(|velocity| velocity.map(|velocity| velocity.dx))
        .collect();

    assert_eq!(speeds, vec![1.0]);
}

#[test]
fn an_optional_component_can_be_mutated_where_present() {
    let mut world = populated();

    for mut velocity in world.query_mut::<Option<&mut Velocity>>().flatten() {
        velocity.dx = 9.0;
    }

    let speeds: Vec<f32> = world
        .query::<&Velocity>()
        .map(|velocity| velocity.dx)
        .collect();

    assert_eq!(speeds, vec![9.0]);
    world.assert_consistent();
}

#[test]
fn an_optional_component_composes_with_a_filter() {
    let world = populated();

    let visited = sorted(
        world
            .query::<(&Position, Option<&Velocity>)>()
            .without::<Frozen>()
            .map(|(position, _)| position.x),
    );

    assert_eq!(visited, vec![1, 3, 5, 6]);
}

#[test]
#[should_panic(expected = "twice with mutable access")]
fn an_optional_component_still_conflicts_with_a_mutable_one() {
    // `Option<&Health>` reads `Health` wherever it exists, so pairing it with
    // `&mut Health` would hand out an aliasing pair on exactly the archetypes
    // where the option is `Some`.
    let mut world = world();

    let _ = world.query_mut::<(&mut Health, Option<&Health>)>();
}

#[test]
fn a_filtered_query_over_an_owning_component_reads_the_real_value() {
    let mut world = world();

    let named = world.spawn();
    world
        .insert(
            named,
            Name {
                text: "kept on the heap".to_owned(),
            },
        )
        .expect("ok");
    world.insert(named, Player {}).expect("ok");

    let bare = world.spawn();
    world
        .insert(
            bare,
            Name {
                text: "also on the heap".to_owned(),
            },
        )
        .expect("ok");

    let names: Vec<&str> = world
        .query::<&Name>()
        .with::<Player>()
        .map(|name| name.text.as_str())
        .collect();

    assert_eq!(names, vec!["kept on the heap"]);
    world.assert_consistent();
}
