# ADR-0005: Record proposal provenance in the journal

**Status:** Accepted
**Date:** 2026-08-30
**Relates to:** [ADR-0001](ADR-0001-bowerbird-architecture.md) §7,
[ADR-0003](ADR-0003-state-store-amendments.md) §1, [ROADMAP](ROADMAP.md)

## Context

The journal records what the executor did. Until now it recorded *what*
happened — action, source, destination, hash — but not *what asked for it*.

Today every proposal comes from a model, so the field would have one value and
look like ceremony. Two items on the roadmap change that:

- A **rule-based fast path** introduces a second thing that produces proposals.
  A journal that cannot distinguish a rule match from a model call cannot answer
  "how often did we actually need the model?", which is the entire justification
  for building the fast path.
- **Learning from corrections** requires knowing whether a person overrode a
  *model* or a *rule*. Those imply different responses: one is a prompt or
  threshold problem, the other is a bug in a rule.

A third distinction already exists in the current code and was also unrecorded:
an operation that cleared the confidence gate unattended, versus one a person
approved through `bower review`. Both write a `move` row, and nothing
distinguished them.

## The decision

Schema v2 adds three columns to `journal`:

| Column | Values | Meaning |
| ------ | ------ | ------- |
| `origin` | `model`, `rule`, `human`, `unknown` | What produced the proposal |
| `decided_by` | `auto`, `human`, `unknown` | Who allowed it to proceed |
| `confidence` | REAL, nullable | The model's confidence, when a model proposed it |

In code these are `state::Provenance`, constructed through three named
constructors — `model_auto`, `model_approved`, `human` — rather than assembled
field by field, so a call site cannot accidentally claim a person approved
something nobody looked at.

`Origin::Rule` is defined but never written by this build. It exists so the
fast path does not require a second migration.

## Why now, rather than with the feature that needs it

**The journal is append-only.** A migration can add a column; it cannot
reconstruct a fact that was never captured. Every run between now and the rule
engine would write rows whose provenance is permanently unknowable — and those
are exactly the rows a learned-corrections feature would most want, since they
are the real usage history.

The cost of adding it now is one migration and three columns. The cost of adding
it later is the same migration, plus a permanent hole in the record. This is the
rare case where speculative generality is cheaper than the alternative, because
the alternative is not "add it later" but "add it later and lose the history".

## Rows written before this migration say `unknown`

They are not backfilled with a plausible value. Almost all of them were in fact
`model`/`auto`, and guessing that would have been right most of the time — which
is precisely the problem. The journal's only real value is that it can be
trusted, and a table that is *mostly* accurate about how it came by its contents
is worse than one that says plainly where its knowledge ends.

`unknown` is a first-class value in both enums, and an unrecognised value read
from a future release also degrades to `unknown` rather than failing the read: a
row written by a newer build is still worth showing.

## A gap this surfaced

Writing the tests exposed that the review queue only stored `confidence` for
*recycle* items. A row queued by the confidence gate — where the confidence is
the entire reason it is queued — stored `NULL`. When a person later approved it,
the journal inherited that `NULL`, so the number was lost permanently at exactly
the moment it mattered most.

`Pending::Review` now carries `Option<f32>`, taken from the proposal the policy
engine already had in hand. `None` remains correct for a malformed or absent
proposal: those have no confidence, and reporting one would invent a number the
model never gave.

## Consequences

- Schema version is 2. Old files migrate forward on open; there is no downgrade,
  and a file from a newer release is still refused rather than damaged.
- `Store::journal_recent` reads rows back, so provenance is not write-only.
- No CLI surface yet. A `bower journal` command is the obvious consumer and is
  listed on the roadmap, but the recording is what is time-sensitive; the
  reporting is not.
