# ADR-0002: Amendments to ADR-0001 found during implementation

**Status:** Accepted
**Date:** 2026-08-29
**Amends:** [ADR-0001](ADR-0001-bowerbird-architecture.md)

## Context

ADR-0001 was written before any code existed. Implementing the scanner, policy
engine, and executor surfaced four points where the design as written was
either self-contradicting, ambiguous, or worse than an available alternative.
This ADR records the changes so the code and the architecture document do not
quietly disagree. Everything not listed here stands as written.

## 1. The scanner excludes managed *category directories*, not `destination_root`

**ADR-0001 §2** says the scanner "always excludes each profile's
`destination_root` (even when nested inside `path`) from the source list".
**ADR-0001 §5** says `destination_root` defaults to `path`.

Together, for the default in-place profile — the common case, and the one the
ADR calls out as needing prominent documentation — these exclude the entire
scan root, and every run scans zero files.

The invariant the rule was reaching for is *never re-ingest our own output*.
That resolves differently in the two cases:

- **Routed elsewhere** (`destination_root != path`): exclude
  `destination_root`, as originally written. It only matters when the
  destination happens to sit inside the source.
- **In place** (`destination_root == path`): exclude the *category
  subdirectories* under it — the directories this profile creates and fills.

The quarantine and recycle stores are excluded the same way, since they are
equally this tool's own output.

### Known gap — closed in ADR-0003

With `allow_dynamic_categories = true` *and* in-place organization, a category
the model invented on an earlier run was not in `profile.categories` and so was
not excluded.

**Closed.** The journal now records the directory each committed operation wrote
into, so `Store::managed_dirs` can report exactly which directories a profile
has actually filed into, whether or not the config names them. The orchestrator
merges that list into the scanner's excluded roots on every run. See ADR-0003
§4.

## 2. `journal_path` is renamed `state_path`

ADR-0001 §7 puts the append-only journal and the mutable review queue in one
SQLite file, but §5 names the only config key for it `journal_path`. That name
describes one of the two tables and invites the assumption that the review
queue lives somewhere else. Renamed to `state_path`.

## 3. `content_sniff_bytes` belongs to `[profiles.metadata]`

The ADR's example config puts `content_sniff_bytes` under
`[profiles.metadata]` in the `downloads` profile and at profile top level in
`personal-docs`. It is a metadata-extraction toggle, so `[profiles.metadata]`
wins. The other position is a hard parse error rather than a silently ignored
key, since a misplaced key here would quietly disable content sniffing on a
profile the user believed had it on.

Deserialization sets `deny_unknown_fields` on every table for the same reason:
in a file that governs where documents end up, a typo should not be something
you discover later.

## 4. The dry-run gate moves from the policy engine to the executor

ADR-0001 §4 lists dry-run as pipeline stage 8, inside the policy engine.
Collapsing an action to `NoOp` there would discard exactly what a dry run
exists to show — a preview that only reports "would do nothing" is not a
preview.

Instead the engine always resolves to the real action, and the executor takes
a `Mode` (`DryRun` or `Execute`) and reports what it did *or would have done*.
The printed plan is therefore derived from the same `ResolvedAction` the
executor consumes on a real run, rather than from a parallel description of it.

## 5. Related implementation choices worth recording

These are not amendments — ADR-0001 left them open — but they are load-bearing
enough to write down.

- **The executor never calls `rename(2)`.** It silently replaces its
  destination, which would make the collision check advisory rather than
  binding. The executor links-then-unlinks on the same filesystem, and copies
  into an `O_EXCL` destination otherwise. A lost race is reported, never
  resolved by destroying something.
- **Two-phase policy resolution.** The collision check is the one stage that
  genuinely needs the disk. Rather than let the engine reach for `std::fs`,
  `plan()` returns `Decision::CheckCollision` and the caller answers with an
  `Occupancy`; `resolve_collision()` is itself pure. This keeps the engine
  exhaustively testable off-disk, which is enforced mechanically by a test.
- **`is_safe_component` vs `is_safe_filename`.** A leading dot is refused in a
  *category* — a model-proposed `.git` or `.ssh` is not a category anyone asked
  for — but permitted in a *filename*, since a dotfile cannot escape a
  directory and refusing to file `.bashrc` under its own name would be a
  surprise rather than a safeguard.
- **Filename template syntax remains provisional.** `{token}`, `{ext}`, and
  `{{`/`}}` escapes are implemented so the pipeline can be built and tested. A
  token the model did not supply is an error rather than an empty string; there
  is deliberately no fallback syntax yet. ADR-0001's open question stands.
