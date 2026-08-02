//! [`WorldCell`] under Miri, driven by `std` threads rather than the pool.
//!
//! # Why this file exists separately from `schedule.rs`
//!
//! `WorldCell` is where the scheduler's entire soundness argument lives: it
//! hands out `&mut` to component data from a *shared* borrow of the world, and
//! the only thing making that sound is the promise that concurrent systems hold
//! disjoint access. That promise deserves a machine check, not just a comment.
//!
//! Miri cannot check it through [`Schedule`](slop_ecs::Schedule). The job pool is
//! `rayon`, and `rayon-core` and `crossbeam-epoch` both use patterns the
//! experimental aliasing models reject — Stacked Borrows objects to
//! `crossbeam-epoch`'s intrusive list, Tree Borrows to `rayon-core`'s scope
//! pointer. Neither is our code and neither is a bug in ours, but both abort the
//! run before reaching anything worth checking, so `schedule.rs` is skipped
//! under Miri.
//!
//! So this file reconstructs what the scheduler does — several `WorldCell`s over
//! one world, on real threads, mutating concurrently — using `std::thread::scope`,
//! which Miri handles. What gets verified is exactly the claim that matters:
//!
//! - Disjoint concurrent access through `WorldCell` is free of data races and
//!   aliasing violations.
//! - The `&mut` handed out by a mutating query really is exclusive.
//! - Change-detection stamps written from several threads at once do not tear.
//!
//! Rayon is then only the dispatcher. Nothing about *our* unsafe hides behind it.

use std::sync::atomic::{AtomicUsize, Ordering};

use slop_ecs::{Access, Entity, Tick, Ticks, World, WorldCell};
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

/// Owns a heap allocation, so a concurrent write that tore would be visible as
/// something worse than a wrong number.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Name {
    text: String,
}

fn world(count: u32) -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Velocity>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Name>().expect("fresh");

    for index in 0..count {
        let entity = world.spawn();
        world
            .insert(entity, Position { x: index as f32 })
            .expect("registered");
        world
            .insert(entity, Velocity { dx: 1.0 })
            .expect("registered");
        world
            .insert(entity, Health { value: 100 })
            .expect("registered");
        world
            .insert(
                entity,
                Name {
                    text: format!("entity {index}"),
                },
            )
            .expect("registered");
    }

    world
}

/// How many entities a volume test uses.
///
/// Miri interprets rather than executes and tracks provenance for every byte, so
/// volume costs minutes there while checking exactly the same paths — see
/// `docs/CONVENTIONS.md` §7.
fn entities() -> u32 {
    if cfg!(miri) { 12 } else { 64 }
}

fn ticks(world: &World) -> Ticks {
    Ticks::everything(world.tick())
}

#[test]
fn two_disjoint_cells_mutate_one_world_concurrently() {
    // The scheduler's core claim, checked directly: two systems whose declared
    // access is disjoint may hold mutating queries over the same world at the
    // same time.
    let count = entities();
    let world = world(count);
    let ticks = ticks(&world);

    let writes_position = [Access::write::<Position>(), Access::read::<Velocity>()];
    let writes_health = [Access::write::<Health>()];

    std::thread::scope(|scope| {
        let world = &world;

        scope.spawn(move || {
            // SAFETY: this cell names Position and Velocity; the other names
            // Health. No component is named by both, so nothing they hand out
            // can alias. Neither performs a structural change, and no
            // `&mut World` exists for the duration of this scope.
            let cell = unsafe { WorldCell::new(world, &writes_position, ticks) };

            for (mut position, velocity) in cell.query::<(&mut Position, &Velocity)>() {
                position.x += velocity.dx;
            }
        });

        scope.spawn(move || {
            // SAFETY: as above, with the roles reversed.
            let cell = unsafe { WorldCell::new(world, &writes_health, ticks) };

            for mut health in cell.query::<&mut Health>() {
                health.value -= 1;
            }
        });
    });

    let mut positions: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .collect();
    positions.sort_unstable();

    assert_eq!(positions, (1..=count).collect::<Vec<u32>>());
    assert!(world.query::<&Health>().all(|health| health.value == 99));
    world.assert_consistent();
}

#[test]
fn many_cells_reading_one_component_at_once() {
    // Shared access is the other half: any number of readers of the same
    // component may run together, which is what puts a whole render-extract
    // stage in one batch.
    let count = entities();
    let world = world(count);
    let ticks = ticks(&world);
    let reads = [Access::read::<Position>()];
    let total = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let world = &world;
        let reads = &reads;
        let total = &total;

        for _ in 0..4 {
            scope.spawn(move || {
                // SAFETY: every cell in this scope reads and none writes, so no
                // two of them can alias mutably.
                let cell = unsafe { WorldCell::new(world, reads, ticks) };

                let sum: f32 = cell.query::<&Position>().map(|position| position.x).sum();
                total.fetch_add(sum as usize, Ordering::Relaxed);
            });
        }
    });

    let one_pass: usize = (0..count as usize).sum();
    assert_eq!(total.load(Ordering::Relaxed), one_pass * 4);
}

#[test]
fn concurrent_change_stamps_do_not_tear() {
    // Every mutating query writes a stamp into its column's `Cell<Tick>`. Two
    // systems writing different components write into different columns' stamp
    // arrays; if those arrays were shared or aliased, this is where it would
    // show.
    let world = world(entities());
    let ticks = Ticks {
        last_run: Tick::ZERO,
        this_run: world.tick(),
    };

    let writes_position = [Access::write::<Position>()];
    let writes_name = [Access::write::<Name>()];

    std::thread::scope(|scope| {
        let world = &world;

        scope.spawn(move || {
            // SAFETY: disjoint from the cell below — Position against Name.
            let cell = unsafe { WorldCell::new(world, &writes_position, ticks) };

            for mut position in cell.query::<&mut Position>() {
                position.x *= 2.0;
            }
        });

        scope.spawn(move || {
            // SAFETY: as above.
            let cell = unsafe { WorldCell::new(world, &writes_name, ticks) };

            for mut name in cell.query::<&mut Name>() {
                name.text.push('!');
            }
        });
    });

    assert!(world.query::<&Name>().all(|name| name.text.ends_with('!')));
    world.assert_consistent();
}

#[test]
fn an_owning_component_can_be_replaced_from_a_cell() {
    // A `String` write frees the old allocation and installs a new one. If a
    // reader were running concurrently over the same column, this is the write
    // that would turn into a use-after-free — so it is worth doing under Miri
    // even single-threaded.
    let world = world(16);
    let ticks = ticks(&world);
    let access = [Access::write::<Name>()];

    // SAFETY: nothing else touches this world for the duration.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    for mut name in cell.query::<&mut Name>() {
        *name = Name {
            text: "replaced with a longer string than before".to_owned(),
        };
    }

    assert!(
        world
            .query::<&Name>()
            .all(|name| name.text.starts_with("replaced"))
    );
    world.assert_consistent();
}

#[test]
fn a_cell_query_yields_exclusive_references() {
    // What `Mut<T>` derefs to must genuinely be unaliased, since the whole
    // design hands it out from a shared borrow.
    let world = world(8);
    let ticks = ticks(&world);
    let access = [Access::write::<Position>()];

    // SAFETY: the only cell over this world.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    let mut addresses = Vec::new();
    for mut position in cell.query::<&mut Position>() {
        let reference: &mut Position = &mut position;
        reference.x += 1.0;
        addresses.push(std::ptr::from_mut(reference).addr());
    }

    addresses.sort_unstable();
    let before = addresses.len();
    addresses.dedup();

    assert_eq!(addresses.len(), before, "two rows shared an address");
}

#[test]
fn a_cell_can_read_entities_alongside_components() {
    let world = world(8);
    let ticks = ticks(&world);
    let access = [Access::read::<Position>()];

    // SAFETY: the only cell over this world, and it only reads.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    let seen: Vec<Entity> = cell
        .query::<(Entity, &Position)>()
        .map(|(e, _)| e)
        .collect();

    assert_eq!(seen.len(), 8);
    assert!(seen.iter().all(|entity| cell.contains(*entity)));
    assert_eq!(cell.len(), 8);
    assert!(!cell.is_empty());
}

#[test]
#[should_panic(expected = "without declaring it")]
fn a_cell_refuses_a_query_outside_its_declaration() {
    let world = world(4);
    let ticks = ticks(&world);
    let access = [Access::read::<Position>()];

    // SAFETY: the only cell over this world.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    let _ = cell.query::<&Health>().count();
}

#[test]
#[should_panic(expected = "mutably without declaring it")]
fn a_cell_refuses_to_widen_a_read_into_a_write() {
    let world = world(4);
    let ticks = ticks(&world);
    let access = [Access::read::<Position>()];

    // SAFETY: the only cell over this world.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    let _ = cell.query::<&mut Position>().count();
}

#[test]
fn a_cell_allows_reading_what_it_declared_mutably() {
    // Over-declaring is the safe direction and must not be rejected.
    let world = world(4);
    let ticks = ticks(&world);
    let access = [Access::write::<Position>()];

    // SAFETY: the only cell over this world.
    let cell = unsafe { WorldCell::new(&world, &access, ticks) };

    assert_eq!(cell.query::<&Position>().count(), 4);
    assert_eq!(cell.access().len(), 1);
    assert_eq!(cell.ticks().this_run, world.tick());
}
