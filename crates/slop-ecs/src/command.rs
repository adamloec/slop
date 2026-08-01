//! Deferred structural change — `docs/DESIGN.md` §2.10.
//!
//! Adding a component, removing one, spawning, and despawning all move an
//! entity's data between archetype tables, so every one of them needs
//! `&mut World`. A system running in parallel with other systems cannot have
//! that: it holds a query, and a query is a borrow. §2.10 calls the resolution
//! **required for safe parallel system execution regardless** — record the
//! change now, apply it at an explicit sync point.
//!
//! ```ignore
//! // During the parallel phase — no `&mut World` in sight.
//! for (entity, health) in world.query::<(Entity, &Health)>() {
//!     if health.0 <= 0 {
//!         commands.despawn(entity);
//!     }
//! }
//!
//! // At the sync point.
//! world.apply(&mut commands)?;
//! ```
//!
//! # A spawned id is not real until the buffer is applied
//!
//! [`CommandBuffer::spawn`] returns a [`Target`], not an [`Entity`]. That is the
//! one part of this design worth arguing for, because the conventional engine
//! answer is the opposite: Bevy and Unity's `EntityCommandBuffer` both hand back
//! a usable entity id immediately, reserving it from the allocator through an
//! atomic.
//!
//! **`docs/DESIGN.md` §2.14 rules that out.** Atomic reservation means two
//! systems spawning on two threads receive ids in whatever order the hardware
//! resolved the contention, so the same build on the same machine assigns
//! different ids on different runs. Every recorded replay and every golden image
//! of a scene that spawns anything would then be timing-dependent.
//!
//! Deferring assignment removes the race rather than tolerating it: each buffer
//! numbers its own spawns from zero, buffers are applied in schedule order, and
//! ids are drawn from the allocator one at a time on one thread. The ordering is
//! a property of the schedule, which is fixed.
//!
//! What it costs: a `Target` cannot be stored *inside* a component, so wiring a
//! freshly spawned child into its parent's component takes either the direct
//! `&mut World` path or a second frame. Recorded in `docs/PLAN.md` §6.1.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::ptr::NonNull;

use slop_reflect::{Reflect, TypeId};

use crate::{EcsError, Entity, World};

/// Bytes reserved by the staging area's first allocation.
const INITIAL_STAGING_CAPACITY: usize = 256;

/// What a recorded command acts on.
///
/// Either an entity that already exists, or one the same buffer will create.
/// [`From`] impls mean call sites rarely name this type:
///
/// ```ignore
/// commands.insert(existing, Position::ZERO);   // Entity
/// let spawned = commands.spawn();
/// commands.insert(spawned, Position::ZERO);    // Target
/// ```
///
/// A `Target::Pending` belongs to the buffer that produced it. Handing one to a
/// different buffer is a bug: it will address whatever entity *that* buffer's
/// spawn of the same ordinal created, or nothing at all. This is not a soundness
/// problem, and detecting it would mean stamping every buffer with an identity
/// that exists only to catch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// An entity that exists now.
    Existing(Entity),
    /// The nth [`spawn`](CommandBuffer::spawn) recorded in this buffer.
    Pending(u32),
}

impl From<Entity> for Target {
    fn from(entity: Entity) -> Self {
        Self::Existing(entity)
    }
}

/// One recorded structural change.
#[derive(Debug)]
enum Command {
    /// Draw the next entity id. Ordinals are implicit: the nth `Spawn` in a
    /// buffer is what `Target::Pending(n)` refers to.
    Spawn,
    Despawn {
        target: Target,
    },
    Insert {
        target: Target,
        staged: Staged,
    },
    Remove {
        target: Target,
        type_id: TypeId,
    },
}

/// A component value parked in the buffer's staging area.
///
/// Carries its own destructor rather than looking one up in the registry,
/// because a buffer that is dropped without being applied still owns these
/// values and has no world to ask.
#[derive(Debug, Clone, Copy)]
struct Staged {
    type_id: TypeId,
    offset: usize,
    drop_in_place: Option<unsafe fn(*mut u8)>,
}

impl Staged {
    /// Destroy the value at `pointer`.
    ///
    /// # Safety
    ///
    /// `pointer` must be this staged value's slot, still initialized, and this
    /// must run at most once for it.
    unsafe fn drop_value(self, pointer: *mut u8) {
        if let Some(drop_in_place) = self.drop_in_place {
            // SAFETY: the caller guarantees the slot is an initialized value of
            // the type this glue was monomorphized for, and that this runs once.
            unsafe { drop_in_place(pointer) };
        }
    }
}

/// `drop_in_place` for `T`, type-erased.
///
/// # Safety
///
/// `pointer` must be an initialized, properly aligned `T` that nothing else
/// will drop.
unsafe fn drop_glue<T>(pointer: *mut u8) {
    // SAFETY: the caller's guarantee, verbatim.
    unsafe { std::ptr::drop_in_place(pointer.cast::<T>()) };
}

/// Structural changes recorded now and applied later.
///
/// Reusable: [`World::apply`] empties the buffer but keeps its allocations, so a
/// frame loop holds one buffer per system and pays no allocation per frame
/// (`docs/CONVENTIONS.md` §8).
///
/// A buffer dropped without being applied destroys every component it staged.
/// Nothing is silently leaked, and nothing reaches the world.
#[derive(Debug, Default)]
pub struct CommandBuffer {
    commands: Vec<Command>,
    staging: Staging,
    spawns: u32,
}

// SAFETY: the same argument `Column` makes. A `CommandBuffer` owns its staging
// allocation exclusively and hands out raw pointers only through methods that
// borrow it. Whether a staged component type is itself `Send` is not knowable
// here — a runtime-declared type has no Rust type to ask — so it is asserted a
// level up: `docs/DESIGN.md` §2.3 makes component data plain data.
//
// `Send` and not `Sync` because this exists to be filled by one system on one
// thread and applied on another; recording takes `&mut self`, so there is
// nothing a shared reference could usefully do.
unsafe impl Send for CommandBuffer {}

impl CommandBuffer {
    /// An empty buffer that has allocated nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many commands are recorded.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Record the creation of an entity with no components.
    ///
    /// The returned [`Target`] addresses it in later commands on this same
    /// buffer. It is not an [`Entity`] and does not become one until
    /// [`World::apply`] runs — see the module documentation for why.
    ///
    /// # Panics
    ///
    /// If a single buffer records more than `u32::MAX` spawns.
    pub fn spawn(&mut self) -> Target {
        let ordinal = self.spawns;
        self.spawns = self
            .spawns
            .checked_add(1)
            .expect("a command buffer recorded more than u32::MAX spawns");

        self.commands.push(Command::Spawn);

        Target::Pending(ordinal)
    }

    /// Record the destruction of an entity and its components.
    pub fn despawn(&mut self, target: impl Into<Target>) {
        self.commands.push(Command::Despawn {
            target: target.into(),
        });
    }

    /// Record giving an entity a component.
    ///
    /// Takes ownership immediately: the value lives in this buffer's staging
    /// area until it is applied or the buffer is dropped.
    ///
    /// Whether `T` is registered is not checked here — a buffer has no registry
    /// to ask. [`World::apply`] reports it.
    pub fn insert<T: Reflect>(&mut self, target: impl Into<Target>, component: T) {
        let offset = self.staging.allocate(Layout::new::<T>());

        // SAFETY: `allocate` returned space for exactly one `T`, aligned for it
        // and referred to by nothing else. `ManuallyDrop` stops the local
        // destructor running after the buffer takes ownership of the bytes.
        unsafe {
            let component = std::mem::ManuallyDrop::new(component);

            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&*component).cast::<u8>(),
                self.staging.at(offset),
                size_of::<T>(),
            );
        }

        self.commands.push(Command::Insert {
            target: target.into(),
            staged: Staged {
                type_id: T::type_id(),
                offset,
                // Storing `None` for a type with no destructor is not an
                // optimization — `Column` does the same, and it keeps a
                // trivially-copyable component from carrying a function pointer
                // that would never be called.
                drop_in_place: std::mem::needs_drop::<T>()
                    .then_some(drop_glue::<T> as unsafe fn(*mut u8)),
            },
        });
    }

    /// Record taking a component away from an entity.
    pub fn remove<T: Reflect>(&mut self, target: impl Into<Target>) {
        self.remove_by_id(target, T::type_id());
    }

    /// Record taking a component away, naming it at runtime.
    ///
    /// The untyped counterpart of [`remove`](Self::remove), for §2.4's guest
    /// components.
    pub fn remove_by_id(&mut self, target: impl Into<Target>, type_id: TypeId) {
        self.commands.push(Command::Remove {
            target: target.into(),
            type_id,
        });
    }

    /// Discard every recorded command, destroying any staged component.
    ///
    /// Keeps the allocations, so the buffer is immediately reusable.
    pub fn clear(&mut self) {
        self.drop_staged();

        self.commands.clear();
        self.staging.clear();
        self.spawns = 0;
    }

    /// Destroy every component still staged.
    ///
    /// Called when the buffer is cleared or dropped. [`World::apply`] takes the
    /// command list first, so nothing reaches here twice.
    fn drop_staged(&mut self) {
        for command in &self.commands {
            if let Command::Insert { staged, .. } = command {
                // SAFETY: an `Insert` still in the list has not been applied, so
                // its slot is initialized, and each command appears once.
                unsafe { staged.drop_value(self.staging.at(staged.offset)) };
            }
        }
    }
}

impl Drop for CommandBuffer {
    fn drop(&mut self) {
        self.drop_staged();
    }
}

impl World {
    /// Apply every recorded command, in order, and empty the buffer.
    ///
    /// This is the sync point. The buffer keeps its allocations and is ready to
    /// record again.
    ///
    /// Commands addressing an entity that is no longer alive are **skipped**,
    /// and any component they carried is destroyed. That is not leniency — a
    /// system recording a change to something another system despawned in the
    /// same phase is the ordinary case, and `docs/PLAN.md` §4.1-C chose checked
    /// access over panicking for exactly it.
    ///
    /// # Errors
    ///
    /// [`EcsError::UnregisteredComponent`] if a staged component's type is not
    /// registered. The remaining commands are still applied and the first error
    /// is the one returned: stopping half way would leave the buffer's changes
    /// partly applied with no way to describe which half, and an unregistered
    /// type is a wiring bug rather than a condition to recover from.
    pub fn apply(&mut self, commands: &mut CommandBuffer) -> Result<(), EcsError> {
        // Taken rather than borrowed, so the loop can reach the staging area
        // through `commands` freely. Put back at the end to keep the capacity.
        let mut list = std::mem::take(&mut commands.commands);

        let mut spawned: Vec<Entity> = Vec::with_capacity(commands.spawns as usize);
        let mut first_error = None;

        for command in list.drain(..) {
            match command {
                Command::Spawn => spawned.push(self.spawn()),

                Command::Despawn { target } => {
                    if let Some(entity) = self.resolve(target, &spawned) {
                        self.despawn(entity);
                    }
                }

                Command::Insert { target, staged } => {
                    let pointer = commands.staging.at(staged.offset);

                    let Some(entity) = self.resolve(target, &spawned) else {
                        // A dead target is routine, and the component it carried
                        // is now unreachable.
                        //
                        // SAFETY: the slot is initialized and this command has
                        // been drained, so nothing else will reach it.
                        unsafe { staged.drop_value(pointer) };
                        continue;
                    };

                    // SAFETY: the slot holds an initialized value of the type
                    // `staged.type_id` names, written by `CommandBuffer::insert`,
                    // and this is the only place it is read. On `Ok` the world
                    // took ownership of it.
                    if let Err(error) = unsafe { self.insert_raw(entity, staged.type_id, pointer) }
                    {
                        // Not moved out, so the buffer still owns it.
                        //
                        // SAFETY: as above — the failed insert left the slot
                        // untouched.
                        unsafe { staged.drop_value(pointer) };

                        first_error.get_or_insert(error);
                    }
                }

                Command::Remove { target, type_id } => {
                    if let Some(entity) = self.resolve(target, &spawned) {
                        self.remove_by_id(entity, type_id);
                    }
                }
            }
        }

        commands.commands = list;
        commands.staging.clear();
        commands.spawns = 0;

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The live entity a target names, if there is one.
    fn resolve(&self, target: Target, spawned: &[Entity]) -> Option<Entity> {
        let entity = match target {
            Target::Existing(entity) => entity,
            // Out of range only if the target came from a different buffer,
            // which `Target` documents as a bug. Skipping beats addressing an
            // arbitrary entity.
            Target::Pending(ordinal) => *spawned.get(ordinal as usize)?,
        };

        self.contains(entity).then_some(entity)
    }
}

/// A bump-allocated staging area for component values.
///
/// A `Vec<u8>` cannot serve. Its allocation is aligned to 1, so a component
/// needing 8- or 16-byte alignment placed at an 8-aligned *offset* is still
/// misaligned in memory, and reading it there is undefined — the kind of bug
/// that works on every machine until it does not. This owns its allocation and
/// tracks the strictest alignment any staged component has demanded.
///
/// # Invariants
///
/// 1. `data` is aligned to `align`, and points at an allocation of `capacity`
///    bytes when `capacity > 0`. When `capacity == 0` it is dangling at `align`,
///    which is what a zero-sized component needs and all it needs.
/// 2. `align` is a power of two.
/// 3. Bytes `0..len` have been handed out; `len..capacity` have not.
///
/// Whether a handed-out region is *initialized* is the caller's business — a
/// `CommandBuffer` writes into every region it allocates, immediately.
struct Staging {
    data: NonNull<u8>,
    len: usize,
    capacity: usize,
    align: usize,
}

impl Staging {
    /// Reserve space for one value of `layout` and return its offset.
    ///
    /// Offsets survive reallocation: growing copies the whole prefix verbatim,
    /// and the new base is aligned at least as strictly as the old one, so
    /// `base + offset` stays correctly aligned for what was placed there.
    fn allocate(&mut self, layout: Layout) -> usize {
        let offset = align_up(self.len, layout.align());
        let end = offset
            .checked_add(layout.size())
            .expect("command buffer staging area overflowed");

        if layout.align() > self.align || end > self.capacity {
            self.reserve(end, layout.align());
        }

        self.len = end;

        offset
    }

    /// A pointer to the region at `offset`.
    fn at(&self, offset: usize) -> *mut u8 {
        debug_assert!(offset <= self.len, "offset was never handed out");

        // SAFETY: `offset` is within `len`, which is within the allocation by
        // invariant 3. For an empty staging area the offset is zero and the
        // dangling-but-aligned pointer is returned unchanged, which is what Rust
        // expects of a pointer to a zero-sized value.
        unsafe { self.data.as_ptr().add(offset) }
    }

    /// Hand back every region. The allocation is kept.
    ///
    /// Does not destroy anything — the caller is what knows the types.
    fn clear(&mut self) {
        self.len = 0;
    }

    /// Grow to hold `needed` bytes, aligned to at least `align`.
    fn reserve(&mut self, needed: usize, align: usize) {
        let align = self.align.max(align);

        let capacity = if needed == 0 {
            // Only the alignment changed. A zero-sized component demands a
            // correctly aligned pointer and no memory at all.
            self.capacity
        } else {
            let mut capacity = self.capacity.max(INITIAL_STAGING_CAPACITY);
            while capacity < needed {
                capacity = capacity
                    .checked_mul(2)
                    .expect("command buffer staging area overflowed");
            }
            capacity
        };

        if capacity == 0 {
            self.data = dangling(align);
            self.align = align;
            return;
        }

        let layout = Layout::from_size_align(capacity, align)
            .expect("a power-of-two alignment and a capacity that fits in isize");

        // SAFETY: `capacity` is non-zero here, so the layout has non-zero size.
        let data = unsafe { alloc(layout) };
        let Some(data) = NonNull::new(data) else {
            handle_alloc_error(layout)
        };

        if self.capacity > 0 {
            // SAFETY: the old allocation holds `len` handed-out bytes and the
            // new one is at least as large; the two are distinct allocations.
            unsafe { std::ptr::copy_nonoverlapping(self.data.as_ptr(), data.as_ptr(), self.len) };

            // SAFETY: `self.data` came from `alloc` with exactly this layout,
            // and every byte has been relocated.
            unsafe { dealloc(self.data.as_ptr(), self.layout()) };
        }

        self.data = data;
        self.capacity = capacity;
        self.align = align;
    }

    /// The layout the current allocation was made with.
    fn layout(&self) -> Layout {
        Layout::from_size_align(self.capacity, self.align)
            .expect("this layout was already used to allocate")
    }
}

impl Default for Staging {
    fn default() -> Self {
        Self {
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
            align: 1,
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.capacity > 0 {
            // SAFETY: `CommandBuffer::drop` destroyed every staged value before
            // this field is dropped, `data` came from `alloc` with this layout,
            // and this runs once.
            unsafe { dealloc(self.data.as_ptr(), self.layout()) };
        }
    }
}

impl std::fmt::Debug for Staging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Staging")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .field("align", &self.align)
            .finish_non_exhaustive()
    }
}

/// A dangling pointer aligned to `align`, for an allocation that does not exist.
fn dangling(align: usize) -> NonNull<u8> {
    debug_assert!(align.is_power_of_two(), "alignments are powers of two");

    NonNull::new(std::ptr::without_provenance_mut(align))
        .expect("a power-of-two alignment is never zero")
}

/// Round `value` up to a multiple of `align`, which must be a power of two.
fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "alignments are powers of two");

    value
        .checked_add(align - 1)
        .expect("command buffer staging area overflowed")
        & !(align - 1)
}
