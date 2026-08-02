//! Change detection, end to end.
//!
//! Two properties, and they fail in opposite directions:
//!
//! - **No false negatives.** A component that was written must be reported. A
//!   missed stamp is a system that silently stops seeing updates, which presents
//!   as stale rendering or physics rather than as a crash.
//! - **No false positives.** A component that was *not* written must not be
//!   reported. Change detection exists to let work be skipped; reporting
//!   everything makes the machinery cost without buying anything.
//!
//! The second is the harder one, and most of this file is aimed at it: reading
//! through a query, migrating between archetypes, and touching a neighbouring
//! entity all have to leave a component's stamps alone.

use slop_ecs::{Added, Changed, Entity, Tick, World};
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
    world.register::<Frozen>().expect("fresh");
    world.register::<Name>().expect("fresh");

    world
}

/// Record when a caller ran, and let time pass before it looks again.
///
/// The two halves are separate on purpose. A caller's own writes carry the tick
/// it ran at, and a stamp equal to `last_run` is not newer — so a test that
/// captured the tick *after* advancing would be asking about a window its own
/// writes fall exactly on, and would see nothing.
fn ran(world: &mut World) -> Tick {
    let ran_at = world.tick();
    world.advance_tick();

    ran_at
}

/// Spawn an entity holding a position.
fn placed(world: &mut World, x: f32) -> Entity {
    let entity = world.spawn();
    world.insert(entity, Position { x }).expect("registered");

    entity
}

/// The `x` values a `Changed<Position>` query since `last_run` reports.
fn changed_since(world: &World, last_run: Tick) -> Vec<u32> {
    let mut values: Vec<u32> = world
        .query::<&Position>()
        .since(last_run)
        .filtered::<Changed<Position>>()
        .map(|position| position.x as u32)
        .collect();
    values.sort_unstable();

    values
}

/// The same for `Added<Position>`.
fn added_since(world: &World, last_run: Tick) -> Vec<u32> {
    let mut values: Vec<u32> = world
        .query::<&Position>()
        .since(last_run)
        .filtered::<Added<Position>>()
        .map(|position| position.x as u32)
        .collect();
    values.sort_unstable();

    values
}

#[test]
fn a_query_that_has_never_run_sees_everything_as_changed() {
    // The default `last_run` is `Tick::ZERO`, which is what a caller with no
    // history should see.
    let mut world = world();
    placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let visited: usize = world
        .query::<&Position>()
        .filtered::<Changed<Position>>()
        .count();

    assert_eq!(visited, 2);
    assert_eq!(
        world
            .query::<&Position>()
            .filtered::<Added<Position>>()
            .count(),
        2
    );
}

#[test]
fn nothing_is_changed_when_nothing_has_happened_since() {
    let mut world = world();
    placed(&mut world, 1.0);

    let seen = ran(&mut world);

    assert_eq!(changed_since(&world, seen), Vec::<u32>::new());
    assert_eq!(added_since(&world, seen), Vec::<u32>::new());
}

#[test]
fn a_write_through_a_query_is_reported() {
    let mut world = world();
    placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let seen = ran(&mut world);

    for mut position in world.query_mut::<&mut Position>() {
        if position.x > 1.5 {
            position.x += 10.0;
        }
    }

    assert_eq!(changed_since(&world, seen), vec![12]);
}

#[test]
fn reading_through_a_mutable_query_does_not_mark_anything() {
    // The reason `&mut T` yields `Mut<T>` rather than `&mut T`. Visiting a row
    // is not writing it, and a loop that writes one row in a hundred must mark
    // one row.
    let mut world = world();
    for x in 1..=5 {
        placed(&mut world, x as f32);
    }

    let seen = ran(&mut world);

    let mut total = 0.0;
    for position in world.query_mut::<&mut Position>() {
        total += position.x;
    }

    assert_eq!(total, 15.0, "every row was visited");
    assert_eq!(
        changed_since(&world, seen),
        Vec::<u32>::new(),
        "and none of them was written"
    );
}

#[test]
fn insert_marks_a_component_added_and_changed() {
    let mut world = world();
    let seen = ran(&mut world);

    world.advance_tick();
    placed(&mut world, 3.0);

    assert_eq!(changed_since(&world, seen), vec![3]);
    assert_eq!(added_since(&world, seen), vec![3]);
}

#[test]
fn overwriting_a_component_changes_it_without_adding_it() {
    // The distinction that makes `Added` worth storing separately: an insert is
    // also a write, so everything added is changed — but overwriting is not
    // gaining.
    let mut world = world();
    let entity = placed(&mut world, 1.0);

    let seen = ran(&mut world);
    world
        .insert(entity, Position { x: 9.0 })
        .expect("registered");

    assert_eq!(changed_since(&world, seen), vec![9]);
    assert_eq!(added_since(&world, seen), Vec::<u32>::new());
}

#[test]
fn get_mut_marks_the_component_it_hands_out() {
    let mut world = world();
    let entity = placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(entity).expect("it has one").x = 7.0;

    assert_eq!(changed_since(&world, seen), vec![7]);
}

#[test]
fn get_does_not_mark_anything() {
    let mut world = world();
    let entity = placed(&mut world, 1.0);

    let seen = ran(&mut world);
    let read = world.get::<Position>(entity).copied();

    assert_eq!(read, Some(Position { x: 1.0 }));
    assert_eq!(changed_since(&world, seen), Vec::<u32>::new());
}

#[test]
fn a_component_keeps_its_stamps_when_the_entity_migrates() {
    // The false positive worth most: gaining an unrelated component must not
    // make every other component of that entity look written. Without this,
    // tagging entities through a command buffer would dirty the whole world.
    let mut world = world();
    let entity = placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let seen = ran(&mut world);
    world.insert(entity, Frozen {}).expect("registered");

    assert_eq!(
        changed_since(&world, seen),
        Vec::<u32>::new(),
        "Position was relocated, not written"
    );
    assert_eq!(added_since(&world, seen), Vec::<u32>::new());

    // The component it actually gained is the one reported.
    let frozen = world
        .query::<Entity>()
        .since(seen)
        .filtered::<Added<Frozen>>()
        .count();
    assert_eq!(frozen, 1);

    world.assert_consistent();
}

#[test]
fn stamps_survive_a_removal_that_migrates_the_entity_back() {
    let mut world = world();
    let entity = placed(&mut world, 1.0);
    world.insert(entity, Frozen {}).expect("registered");

    let seen = ran(&mut world);
    assert!(world.remove::<Frozen>(entity));

    assert_eq!(
        changed_since(&world, seen),
        Vec::<u32>::new(),
        "removing a sibling does not write Position"
    );
    world.assert_consistent();
}

#[test]
fn stamps_follow_the_entity_a_swap_remove_moves() {
    // A despawn moves the last row into the hole. If the stamps did not move
    // with it, the surviving entity would inherit the dead one's change history.
    let mut world = world();
    let doomed = placed(&mut world, 1.0);
    let survivor = placed(&mut world, 2.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(survivor).expect("it has one").x = 20.0;

    world.advance_tick();
    assert!(world.despawn(doomed));

    assert_eq!(
        changed_since(&world, seen),
        vec![20],
        "the survivor kept its own history through the swap"
    );
    world.assert_consistent();
}

#[test]
fn a_neighbouring_entity_is_not_marked() {
    let mut world = world();
    let one = placed(&mut world, 1.0);
    placed(&mut world, 2.0);
    placed(&mut world, 3.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(one).expect("it has one").x = 11.0;

    assert_eq!(changed_since(&world, seen), vec![11]);
}

#[test]
fn a_changed_filter_skips_archetypes_without_the_component() {
    let mut world = world();
    placed(&mut world, 1.0);

    let bare = world.spawn();
    world
        .insert(bare, Velocity { dx: 5.0 })
        .expect("registered");

    let visited = world
        .query::<Entity>()
        .filtered::<Changed<Position>>()
        .count();

    assert_eq!(visited, 1, "the velocity-only entity has no Position stamp");
}

#[test]
fn set_if_neq_only_marks_when_the_value_differs() {
    // The idiom for a system that recomputes the same answer most frames.
    let mut world = world();
    placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let seen = ran(&mut world);

    let mut wrote = 0;
    for mut position in world.query_mut::<&mut Position>() {
        if position.set_if_neq(Position { x: 1.0 }) {
            wrote += 1;
        }
    }

    assert_eq!(wrote, 1, "one of the two already held that value");
    assert_eq!(changed_since(&world, seen), vec![1]);
}

#[test]
fn bypassing_change_detection_writes_without_marking() {
    let mut world = world();
    placed(&mut world, 1.0);

    let seen = ran(&mut world);

    for mut position in world.query_mut::<&mut Position>() {
        position.bypass_change_detection().x = 42.0;
    }

    let values: Vec<f32> = world.query::<&Position>().map(|p| p.x).collect();

    assert_eq!(values, vec![42.0], "the write landed");
    assert_eq!(
        changed_since(&world, seen),
        Vec::<u32>::new(),
        "and was deliberately not reported"
    );
}

#[test]
fn into_inner_marks_the_component() {
    let mut world = world();
    placed(&mut world, 1.0);

    let seen = ran(&mut world);

    for position in world.query_mut::<&mut Position>() {
        let position: &mut Position = position.into_inner();
        position.x = 5.0;
    }

    assert_eq!(changed_since(&world, seen), vec![5]);
}

#[test]
fn a_systems_own_writes_are_not_reported_back_to_it() {
    // A stamp equal to `last_run` is not newer. Without that rule a system would
    // see its own writes forever and never converge.
    let mut world = world();
    placed(&mut world, 1.0);

    let ran_at = world.advance_tick();
    for mut position in world.query_mut::<&mut Position>() {
        position.x += 1.0;
    }

    assert_eq!(
        changed_since(&world, ran_at),
        Vec::<u32>::new(),
        "the writes carry exactly the tick the run happened at"
    );
}

#[test]
fn change_detection_composes_with_the_other_filters() {
    let mut world = world();
    let frozen = placed(&mut world, 1.0);
    world.insert(frozen, Frozen {}).expect("registered");
    let moving = placed(&mut world, 2.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(frozen).expect("has one").x = 11.0;
    world.get_mut::<Position>(moving).expect("has one").x = 22.0;

    let visited: Vec<u32> = world
        .query::<&Position>()
        .since(seen)
        .filtered::<Changed<Position>>()
        .without::<Frozen>()
        .map(|position| position.x as u32)
        .collect();

    assert_eq!(visited, vec![22]);
}

#[test]
fn a_mutable_query_may_be_filtered_on_the_component_it_writes() {
    // Not an aliasing conflict: a filter reads a stamp, not the component, and
    // filter access is deliberately kept out of the query's own conflict check.
    // "For everything that changed, react to it" is the common shape.
    let mut world = world();
    let entity = placed(&mut world, 1.0);
    placed(&mut world, 2.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(entity).expect("has one").x = 10.0;

    world.advance_tick();
    for mut position in world
        .query_mut::<&mut Position>()
        .since(seen)
        .filtered::<Changed<Position>>()
    {
        position.x *= 2.0;
    }

    let mut values: Vec<u32> = world.query::<&Position>().map(|p| p.x as u32).collect();
    values.sort_unstable();

    assert_eq!(values, vec![2, 20]);
}

#[test]
fn an_owning_component_is_tracked_like_any_other() {
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(
            entity,
            Name {
                text: "first".to_owned(),
            },
        )
        .expect("registered");

    let seen = ran(&mut world);

    for mut name in world.query_mut::<&mut Name>() {
        name.text = "second".to_owned();
    }

    let changed: Vec<&str> = world
        .query::<&Name>()
        .since(seen)
        .filtered::<Changed<Name>>()
        .map(|name| name.text.as_str())
        .collect();

    assert_eq!(changed, vec!["second"]);
    world.assert_consistent();
}

#[test]
fn a_command_buffer_insert_is_stamped_at_the_sync_point() {
    let mut world = world();
    let seen = ran(&mut world);

    let mut commands = slop_ecs::CommandBuffer::new();
    let entity = commands.spawn();
    commands.insert(entity, Position { x: 4.0 });

    world.advance_tick();
    world.apply(&mut commands).expect("registered");

    assert_eq!(added_since(&world, seen), vec![4]);
    assert_eq!(changed_since(&world, seen), vec![4]);
    world.assert_consistent();
}

#[test]
fn the_window_is_fixed_when_the_query_is_built() {
    let mut world = world();
    let entity = placed(&mut world, 1.0);

    let seen = ran(&mut world);
    world.get_mut::<Position>(entity).expect("has one").x = 5.0;

    // Two queries over the same world, asking about different windows.
    let since_seen = changed_since(&world, seen);
    let since_now = changed_since(&world, world.tick());

    assert_eq!(since_seen, vec![5]);
    assert_eq!(since_now, Vec::<u32>::new());
}

#[test]
#[should_panic(expected = "must be set before it is iterated")]
fn setting_the_window_after_iterating_is_rejected() {
    let mut world = world();
    placed(&mut world, 1.0);

    let mut query = world.query::<&Position>();
    query.next();

    let _ = query.since(Tick::ZERO);
}

#[test]
fn many_entities_churn_without_a_stamp_landing_on_the_wrong_row() {
    // The stamps are two more arrays that have to stay in lockstep with the
    // columns. This drives every operation that can break that — insert, remove,
    // despawn, and the swap-remove each of them performs.
    let mut world = world();
    // Miri interprets rather than executes, so volume costs minutes there while
    // checking the same paths — `docs/CONVENTIONS.md` §7.
    let count = if cfg!(miri) { 12 } else { 64 };
    let mut entities: Vec<Entity> = (0..count).map(|x| placed(&mut world, x as f32)).collect();

    for (index, &entity) in entities.iter().enumerate() {
        if index % 3 == 0 {
            world.insert(entity, Frozen {}).expect("registered");
        }
        if index % 5 == 0 {
            world
                .insert(entity, Velocity { dx: 1.0 })
                .expect("registered");
        }
    }
    world.assert_consistent();

    let seen = ran(&mut world);

    // Write to exactly the frozen ones, then churn the structure around them.
    for mut position in world.query_mut::<&mut Position>().with::<Frozen>() {
        position.x += 1000.0;
    }

    for (index, &entity) in entities.iter().enumerate() {
        if index % 7 == 0 {
            world.remove::<Velocity>(entity);
        }
    }
    for entity in entities.drain(..).step_by(11) {
        world.despawn(entity);
    }
    world.assert_consistent();

    // Every surviving position above 1000 is one that was written, and every
    // reported change must be one of them.
    let reported = changed_since(&world, seen);
    let expected: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .filter(|x| *x >= 1000)
        .collect();

    let mut expected = expected;
    expected.sort_unstable();

    assert_eq!(reported, expected);
    assert!(!reported.is_empty(), "the test would prove nothing empty");
}
