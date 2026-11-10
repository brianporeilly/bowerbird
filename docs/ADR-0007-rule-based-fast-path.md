# ADR-0007: A rule-based fast path

**Status:** Accepted
**Date:** 2026-11-09
**Relates to:** [ADR-0001](ADR-0001-bowerbird-architecture.md),
[ADR-0005](ADR-0005-journal-provenance.md), [ROADMAP](ROADMAP.md)

## Context

Some files do not need judgment. `*.nzb` and `*.torrent` go in one place, and so
does a family of files whose names say what they are. Asking a model costs
seconds per file on a local server, and the lab measured the cost directly: the
same 41 files took 4m30s through a 27B model, and longer with content sniffing
on. A small model also gets the obvious ones wrong, and wrong *confidently*: a
3B filed a utility bill under Photos at 0.99.

A person can state these decisions in advance. A rule is that statement.

## The decision

A profile may carry `[[profiles.rules]]`. A file that matches one is filed by it
and never reaches the model.

```toml
[[profiles.rules]]
name = "usenet"
category = "Downloads"
extensions = ["nzb", "torrent"]
```

Criteria are `extensions`, `name_glob` (case-insensitive, against the file name)
and `mime` (exact, or `image/*` for a family). **All** the criteria a rule sets
must hold; at least one is required. The first matching rule wins, in the order
written.

### There are no built-in rules

This was deliberate. Which category a `.zip` belongs in is the owner's call
(an installer, an archive, a 3D-print model), and a default would be a guess
presented as a fact. A rule is allowed to act without a confidence gate because
the person has already made the decision the gate would approximate; a rule
nobody wrote has no such standing.

### A rule is a proposal, not a side door

A match produces the same `Proposal` a model would, with confidence `1.0`, and
goes through the same policy engine: category resolution, remembered
rejections, filename rendering, path containment, collision handling. The only
stages it skips are the model and, effectively, the gate, because a constant
`1.0` has nothing to be unsure about.

The safety argument therefore does not change. `ResolvedAction` is still the
closed set it was, a rule still cannot name a path, and a rule that collides
with an existing file is quarantined, never overwritten (tested).

### Category is checked at config time

A rule whose category is not declared (and `allow_dynamic_categories = false`),
is not a safe directory name, has no criteria, has a bad glob, repeats another
rule's name, or asks for `mime` while `detect_mime = false` is a config error.
A rule acts unattended, so one that cannot do what it says should stop the run
before anything moves. The policy engine still re-checks the category, so a
hand-built profile gets the same protection.

## Provenance

`Origin::Rule` existed since [ADR-0005](ADR-0005-journal-provenance.md), unused,
so that this feature would not need a journal migration. A rule move is
journalled `origin = rule`, `decided_by = auto`, with **no confidence**. The
policy engine is given `1.0` to make it treat the match as certain, but that is
not something anyone measured, and recording it would pass a constant off as a
measurement in the one table that is supposed to be trustworthy.

### Schema v3: the review queue remembers who proposed

ADR-0005 recorded provenance on the journal but not the queue, which was fine
while only a model could queue anything. A rule can: a collision under
`on_conflict = "quarantine"`, or a missing filename token. Approving such an
item would then have been journalled as a **model** decision, because the queue
forgot where it came from.

`review_queue.origin` fixes that, and `Provenance::approved(origin, confidence)`
replaces `model_approved`, which is removed because its default was the wrong
one. Unlike the journal's v2 columns, existing queue rows are backfilled to
`model`: before this migration nothing else could have queued them, so it is a
fact and not a guess (tested, so it stops being true loudly if that changes).

## What rules do not do

- **They do not rename.** A rule supplies no filename tokens. On a profile with
  `rename` enabled, a template that needs a model-supplied token sends the file
  to review, exactly as it does when a model omits one. A rule-only filename
  would need its own design.
- **They do not match on content.** Only the name, extension and detected MIME
  type. Matching on content would make a rule a classifier, which is what the
  model is for.
- **They are not learned.** Promoting repeated human corrections into rules is
  on the roadmap and depends on the `origin` and `decided_by` columns, but it is
  a separate feature.

## Consequences

- A run with rules makes fewer model calls, and none when every file is claimed.
- `bower run` marks rule-filed files `[rule]` and reports how many.
- Schema version is 3. Old files migrate forward; there is no downgrade.
- Because rules bypass the gate, a *wrong* rule moves files wrongly with no
  second look. The journal makes it findable (`origin = rule`) and the move is
  reversible, but rules deserve the same care as any config that governs where
  files end up. Dry run first.
