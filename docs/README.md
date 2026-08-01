# Slop Engine — Documentation

**Last updated:** 2026-08-01

## The four documents

| Document | Answers | Authoritative for |
|---|---|---|
| [DESIGN.md](DESIGN.md) | **What** we are building | Every architectural decision, with rationale |
| [PLAN.md](PLAN.md) | **When** | Milestones, task breakdown, environment, invariants |
| [CONVENTIONS.md](CONVENTIONS.md) | **How** code is written | Layout, naming, errors, `unsafe`, testing, logging |
| [architecture.md](architecture.md) | **How the pieces fit** | Cross-crate diagrams — layering, frame flow, data movement |

Read `DESIGN.md` first and in full before writing any code.

## Per-crate documentation

One directory per crate, named exactly as the crate is.

| Crate | Docs | Status |
|---|---|---|
| `slop-core` | [slop-core/](slop-core/) | Handles, storage, arena, time, determinism primitives landed |
| `slop-math` | [slop-math/](slop-math/) | Transforms, projections, portable scalars landed |
| `slop-reflect` | [slop-reflect/](slop-reflect/) | Type model, registry, derive landed |
| `slop-ecs` | [slop-ecs/](slop-ecs/) | Storage, world, queries landed; scheduling outstanding |
| `slop-rhi` | [slop-rhi/](slop-rhi/) | Vulkan backend through the bindless heap landed |
| `slop-app` | [slop-app/](slop-app/) | Window and surface only — the frame loop is M3 |
| `slop-verify` | [slop-verify/](slop-verify/) | Golden-image harness landed |
| `slop-cli` | — | The cook step; documented in `PLAN.md` §6.1 as provisional |
| `slop-reflect-derive` | — | Covered by [slop-reflect/](slop-reflect/); a proc macro must be its own crate |

Crates from `DESIGN.md` §4 that do not exist yet get a directory when they do,
not before.

## Conventions for these documents

Docs here and the rustdoc in the source have different jobs and should not
duplicate each other:

- **Rustdoc** — what an item does and how to call it. Lives with the code so it
  cannot drift from the signature.
- **These docs** — why a crate is shaped the way it is, how its pieces relate,
  and what a newcomer needs before reading the source. Diagrams live here
  because rustdoc cannot render them.

### Every crate document follows the same order

1. **Purpose** — one paragraph. What this crate owns and what it deliberately does not.
2. **Status** — what exists today, what is planned, and at which milestone.
3. **Module map** — a diagram of the crate's internal structure.
4. **Key types** — a table: type, role, and where its decision is recorded.
5. **Diagrams** — lifecycles, data flow, state machines. Whatever the crate's hard part is.
6. **Decisions** — links into `DESIGN.md` and `PLAN.md`. Reasoning lives there; this section points.
7. **Invariants** — what callers and future maintainers must not break.

Sections with nothing to say are omitted rather than left empty.

### Style

- **Prose states a claim and its reason.** The same rule `CONVENTIONS.md` uses:
  a statement without a reason is a statement that gets ignored.
- **Reasoning is never duplicated.** It lives in `DESIGN.md` or `PLAN.md` and is
  cited by section number. Two copies of a rationale become two different
  rationales.
- **Prefer a table to a list** when entries share a shape, and a diagram to a
  table when the subject is flow or state.
- **Cite sections, not page positions** — `DESIGN.md` §2.6, never "see above".

### Diagrams

- **Mermaid, always.** It renders natively on GitHub, diffs as text, and
  normalizes under `.gitattributes` like everything else. No binary image files
  and no external diagramming tools.
- **One diagram, one question.** A diagram that answers three questions answers
  none of them legibly. Split it.
- **No styling or colors.** Defaults render correctly in both light and dark
  themes; hand-picked colors do not.
- **Consistent shapes across the whole docset**, so a reader learns the
  vocabulary once:

| Shape | Means | Mermaid |
|---|---|---|
| Rectangle | A crate, module, or type | `A[slop-core]` |
| Rounded | A process or stage | `A(cook)` |
| Cylinder | Stored or owned data | `A[(assets)]` |
| Diamond | A decision or branch | `A{live?}` |
| Dashed arrow | Deferred, optional, or planned | `A -.-> B` |

- **Direction is meaningful.** `flowchart TD` for layering and hierarchy, where
  down means "depends on". `flowchart LR` for pipelines, where right means
  "later in time".
