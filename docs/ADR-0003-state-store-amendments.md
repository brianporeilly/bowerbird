# ADR-0003: The state store, and what it changed about ADR-0001

**Status:** Accepted
**Date:** 2026-08-30
**Amends:** [ADR-0001](ADR-0001-bowerbird-architecture.md), [ADR-0002](ADR-0002-implementation-amendments.md)

## Context

Milestone 2 implemented ADR-0001 §7: the SQLite journal, review queue,
remembered rejections, and recycle store, plus the `bower review` and
`bower recycle` command groups that act on them.

Building it surfaced one place where ADR-0001 is self-contradicting, one where
its action space is too narrow to support its own review flow, and three
choices worth recording. It also closed the gap ADR-0002 left open. Everything
not listed here stands as written.

## 1. The journal records *operations*, not *actions*

**ADR-0001 §7** says the journal is an "append-only record of every *executed*
action (what actually happened). Never edited."
**ADR-0001 §2** says the executor "writes to the journal before and after each
operation."

These cannot both hold of a single row. A row written *before* the operation
describes an intent, not something that happened; and if the journal is never
edited, that row can never be updated to say how it ended.

The tension resolves by journalling operations with two rows rather than
actions with one:

- an `intent` row, written before the filesystem is touched;
- a `committed` or `failed` row, written after, linked by an op id.

Both are appends. Nothing is edited. And the combination is strictly more
useful than either reading alone: **an intent with no result is the visible
signature of a crash mid-move**, which is precisely what a before-write exists
to provide. `Store::unfinished_operations` reports exactly those.

A journal that will not accept the intent aborts the operation rather than
proceeding unrecorded.

## 2. Deferred actions carry the destination they would have used

**ADR-0001 §4** gives `NeedsManualReview` a `reason` and the raw proposal, and
`Quarantine` only a `reason`. Neither keeps the destination the engine already
resolved.

That makes ADR-0001 §7's own review flow unimplementable as specified.
Approving a queued row means filing the file where the run would have filed it
— but with the destination discarded, `bower review approve` would have to
re-run the entire pipeline, *including another call to the model*, to recover
an answer the engine had already computed and validated days earlier. Worse,
the model is not deterministic, so the second answer need not match the one the
human actually approved.

Both variants now carry `proposed: Option<DestPath>`, and `ResolvedAction`
grows `proposed_dest()` alongside `dest()`. The two are deliberately distinct:

- `dest()` — where this action **will** write. Empty for everything the
  executor will not perform on its own.
- `proposed_dest()` — where it **would** write if a human approved it.

This does not widen what the action space can express. A `DestPath` is still
proof of containment, `NeedsManualReview` and `Quarantine` still execute
nothing, and the containment property test now runs over `proposed_dest()` —
a strictly larger set of paths than it covered before.

Only two stages can populate it, which is the point. A proposal held back by
the confidence gate cleared every other stage, so its destination is settled. A
quarantined conflict names the destination it could not take. Anything that
failed earlier — malformed output, an undeclared category, a template that
would not render — proposes nothing, because there is nothing honest to
propose. Approving such a row is refused with a message saying to reject it
instead.

The stored category is the **resolved** spelling, not the model's, so approving
a row files the file exactly where a confident run would have rather than
fragmenting a category on the model's casing.

## 3. Approval re-validates against the world as it is now

ADR-0001 §7 requires re-validating the file hash before executing a resolution.
Implemented, and extended in two ways that follow from the same reasoning:

- **A stale row is discarded, not left.** If the file changed or vanished, the
  proposal was about a file that no longer exists in that form. Leaving the row
  invites someone to approve it later without noticing; the next run will make
  a fresh proposal if one is still warranted.
- **The config is authoritative, not the row.** Approval re-resolves the
  category against the profile as it stands *now*, via the same
  `policy::resolve_category` the run path uses. A category removed from a
  profile since the proposal was made is not filed into just because a row
  remembers it.

Approval also walks suffixes if something occupied the destination while the
decision was pending. A human saying yes is not a licence to overwrite.

## 4. Closing the ADR-0002 §1 gap

ADR-0002 left one case open: an in-place profile with
`allow_dynamic_categories = true` creates category directories the config never
names, so the scanner could not know they were output rather than input.

The journal now records `dest_dir` — the directory each operation wrote into —
explicitly rather than deriving it, so `Store::managed_dirs` answers the
question directly and without parsing paths at query time. `run_profile` merges
that list into the scanner's excluded roots on every run.

Note the scoping: `managed_dirs` is per profile, so one profile's output
directory does not become another profile's blind spot.

## 5. Rejections are scoped by profile, and prefiltered by size

ADR-0001 §7 keys remembered rejections on "file-hash + proposed-category".
Two refinements:

- **Also scoped by profile.** A refusal in a downloads folder should not
  silence the same question in a document inbox; they are different directories
  with different purposes and, often, different people's expectations.
- **`file_size` is stored as a prefilter.** Honouring rejections means knowing a
  file's hash, and hashing every file on every run to find the rare match would
  make the feature cost more than it saves. Size is already free from the
  scanner, so a run only reads a file whose size matches some rejection.
  The hash remains the authority; size only decides whether to bother computing
  it.

The policy engine consults rejections without losing its purity: the caller
looks them up and hands them in as `PriorRejections` data, exactly as it already
does for `Occupancy`. The purity test still passes.

## 6. The recycle store is flat, not a mirror

ADR-0001 §7 says approving a delete moves the file into a recycle store
"mirroring original path structure, so restore is a simple reverse-move".

The store is instead laid out as `<recycle_dir>/<profile>/<name>`, with the
original path recorded in the database row.

Restore does not need a mirror — the row already knows where the file came
from, so the reverse move is a lookup rather than a path computation.
Mirroring, meanwhile, would mean constructing a deep path out of arbitrary
source components, which is exactly what `DestPath` exists to prevent: it only
ever names `<root>/<category>/<file>`, and the containment property test
asserts precisely two components below the root. A flat layout keeps every
recycled file inside the same guarantee that protects a filed one.

Restoring is the one move that writes outside a destination root, and it is
deliberately not expressible as a `DestPath`. It is not a policy decision being
executed but the undoing of one, back to a path the store recorded before
anything moved.

## 7. Deletion remains confined, and it is tested

`bower recycle purge` is the only operation that destroys anything. Two tests
hold the line: a behavioural one asserting that approving a deletion leaves the
bytes intact and only purge removes them, and a source-level one that greps
every `remove_file` in the crate against an audited allowlist and asserts the
single call in `review.rs` sits inside `purge` rather than in approve, reject,
or restore.

## 8. The static build needs a musl C toolchain

`rusqlite`'s `bundled` feature compiles SQLite from C, so
`--target x86_64-unknown-linux-musl` now needs a musl C cross-compiler, which a
pure-Rust dependency tree did not. `musl-tools` installs it as `musl-gcc`, but
`cc-rs` looks for `x86_64-linux-musl-gcc` first and fails before trying that
fallback, so CI and the `justfile` set `CC_x86_64_unknown_linux_musl`
explicitly.

`bundled` is still the right choice: it is what makes the shipped binary
genuinely self-contained, and linking the host's libsqlite3 would trade a build
dependency for a runtime one on every machine the binary lands on.

### Do not also override the linker

Setting `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc` alongside
`CC_` seems like the obvious companion change. It is not: it makes `musl-gcc`
the linker *driver*, which links against `/lib/ld-musl-x86_64.so.1` and costs us
the static binary that is this target's entire purpose. `rustc`'s own
self-contained linking is what produces `static-pie`; `CC_` is only needed so
`cc-rs` can compile the SQLite amalgamation. Set `CC_` alone.

Measured, on the same tree:

| Linker | `file` says |
| ------ | ----------- |
| rustc self-contained (`CC_` only) | `static-pie linked` |
| `musl-gcc` driver (`CC_` + `..._LINKER`) | `dynamically linked, interpreter /lib/ld-musl-x86_64.so.1` |

### `ldd` cannot verify this

The original CI assertion grepped `ldd` output for `=>`. A musl-dynamic binary
with no shared library dependencies still reports `statically linked`, so that
check passes on a binary that cannot run without an interpreter — it reported
success on exactly the regression above.

The load-bearing check is the absence of a `PT_INTERP` program header
(`readelf -l | grep INTERP`), and the proof is running the binary: a runner with
no musl loader executes a genuinely static one and fails a dynamic one.
