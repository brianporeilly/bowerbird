# Bowerbird Roadmap

**Status: direction, not commitment.** Nothing below is scheduled. It exists so
that decisions already reasoned through are not re-litigated from scratch, and
so the reasoning survives if the people or the context do not.

Where a decision *has* been taken it is marked **Decided** and says why. Where it
is genuinely open it is marked **Open** and says what evidence would settle it.

---

## Where things stand

| Milestone | State |
| --------- | ----- |
| Config, scanner, policy engine, executor, per-profile locking | Shipped |
| SQLite state store: journal, review queue, rejections, recycle | Shipped |
| `bower review` / `bower recycle`, `review_placement` | Shipped |
| Context builder, OpenAI-compatible adapter, structured output | Shipped |
| Journal provenance ([ADR-0005](ADR-0005-journal-provenance.md)) | Shipped |
| Anthropic-compatible adapter | Next, per ADR-0001 §9 |

Deferred by original design, unchanged: watcher daemon, notifications, TUI/GUI,
media-library conventions.

---

## The generalizable insight

Strip away the filesystem and the pipeline is:

> take a set of entities, ask a model to classify each against an allowed
> taxonomy, validate the response, gate on confidence, execute through a
> reversible mechanism, journal everything.

"Files in a directory" is the first instantiation. If that pattern generalizes,
later tools become "write an adapter" rather than "start a project".

### Decided: do not build an `Organizable` trait yet

A trait designed from one concrete case is usually wrong in ways only a second
real case reveals. Finish the filesystem target concretely; extract the
abstraction when a second target actually exists, shaped by what the first one
needed.

### Decided: do not split `bower-core` into `core` + `fs` either

This was considered on "cheap early, awkward later" grounds and rejected. **A
crate boundary is the same abstraction commitment as a trait, written
differently** — and it is harder to move, because it constrains visibility and
dependency direction too. Splitting now would be building the abstraction we
just agreed to defer.

The split also would not fall where it first appears to. The policy engine is
roughly 40% domain-generic:

| Stage | Generic? |
| ----- | -------- |
| Schema validation, per item | yes |
| Staleness (`FileFacts`: size + mtime) | no |
| Taxonomy resolution | yes |
| Filename template rendering | no |
| Path construction (`DestPath`) | no |
| Collision via content hash | no |
| Confidence gate | yes |

More fundamentally, **the safety property is not generic.** `ResolvedAction` is
safe because `DestPath` proves containment under a root. Mail's equivalent — "a
label in the allowed set" — is a different proof with a different shape. What
generalizes is the *discipline* (pure policy, closed action space, journal
everything), and discipline does not live in a crate.

### Done: `bower-llm` is decoupled from the filesystem domain

`LlmBackend::classify` now takes a `BatchContext` — what the core has already
decided the model may see — rather than a `BatchRequest` carrying `&Profile` and
`&[FileRecord]`. `context::build` runs on the core side of the port.

A backend can no longer widen disclosure: not by convention, but because the
type does not carry the data. `bower-llm` no longer references `Profile` or
`FileRecord`. It still depends on `bower-config` for `Backend`, `Provider`, and
`StructuredOutput`, which are a backend's own settings and legitimately its
business.

Done on its own merits, as argued here: it removed a dependency that should not
have existed. It happens to be the seam a second target would need, which was a
reason to prefer this shape, not a reason to have done it sooner. See
[ADR-0006](ADR-0006-engine-tokens-and-the-backend-port.md).

### Open: a model-proposed date token

`{date}` is now filled by the engine from the file's mtime, because the model is
never told a timestamp and a date it supplied would be one it invented
([ADR-0006](ADR-0006-engine-tokens-and-the-backend-port.md)).

That means `{date}` always means *when the file was written*, never *the date on
the document*. For an invoice or a scanned letter, the second is usually what a
person wants in the filename.

The honest fix is a distinct model-proposed token — say `{doc_date}` — read out
of the content excerpt, kept separate from the engine's so the two can never be
confused. **Evidence that would settle it:** someone renaming documents and
finding mtime is the wrong date often enough to care. Not built, because nothing
yet needs it and a second date token is a thing users will get wrong.

---

## Filesystem scope (still the primary product)

### Rule-based fast path

Trivial extension/regex rules (`*.iso` → Installers) skip the model entirely for
obvious cases. Cheaper, faster, fully deterministic where no judgment is needed.

**Architectural note, already acted on:** this introduces a *second producer of
proposals*. The journal now records `origin` so model and rule decisions are
distinguishable — see [ADR-0005](ADR-0005-journal-provenance.md). That had to
land first because the journal is append-only and cannot be backfilled.

### Learned corrections promoted to rules

When a person repeatedly overrides the model for a recognizable pattern, promote
the correction into a static rule. The tool gets cheaper and more predictable as
it absorbs actual preferences.

Depends on `origin` **and** `decided_by`: correcting a rule means fix the rule;
correcting the model means adjust a prompt or a threshold. Both are now
recorded.

### Link modes: `link_mode = "symlink" | "hardlink" | "move"`

A symlink tree gives a non-destructive "virtual organized view" beside an
untouched source. Valuable on its own merits for nervous users and high-stakes
profiles, not merely as a migration path.

**Hardlink mode is nearly free today.** `exec.rs` already does
`hard_link(src, dest)` then `remove_file(src)` — hardlink mode is that path
minus the unlink.

**Symlink mode is not symmetric** and is a real feature, not a flag. Two
decisions need revisiting: the scanner skips symlinks by design, and the
collision check hashes the destination, which for a symlink is the *target's*
content.

### Other filesystem work

- **Proactive duplicate detection** beyond move-time collisions — perceptual
  hashing for images, audio fingerprinting for music.
- **Archive-aware scanning** — peek inside zip/tar without extracting.
- **External metadata lookups** (TMDB, MusicBrainz, Open Library) as an optional
  context-builder stage. This is the natural on-ramp to media-library mode and
  needs no new architecture — it slots into the existing context builder.
- **`bower journal`** — a read surface for the journal. `Store::journal_recent`
  exists; nothing consumes it from the CLI yet.

---

## Sibling targets

| Idea | Verdict |
| ---- | ------- |
| **Note vaults** (Obsidian/Logseq) | **Not a second target.** A vault *is* a directory of files. This is the existing filesystem target plus frontmatter parsing in the context builder — high value and cheap, but it proves nothing about an abstraction. |
| **Bookmarks / read-later** | **Would prove the wrong thing.** Attractive because low-stakes, but low stakes means low information: no containment, no rename, no collision, no destructive operation. An abstraction validated here fits bookmarks and files, then breaks on mail. |
| **Mail triage (IMAP)** | **The real test.** No paths, no containment-under-root, archive-not-delete, no atomic move. |
| **Photo library** | Technically files, but different enough to deserve its own pipeline (vision model on thumbnails, EXIF, perceptual-hash dedup, burst grouping). Its own product built *on* the filesystem target, not a bolt-on. |

### Mail breaks the current action space — worth knowing now

`ResolvedAction::Move { dest }` assumes **exactly one destination per entity**.
Mail labels are many-to-one: a message can carry several simultaneously.

That is not an adapter detail. It is the closed action space the entire safety
argument rests on. If a second target is ever taken seriously, the abstraction
is "a set of validated placements", not "a validated destination".

No action today — recorded so the eventual design starts from the right shape.

---

## Non-filesystem storage backends

S3/R2/B2, SMB, Google Drive — attractive once `Organizable` exists for a real
second case.

Object storage has no atomic rename, so "move" is copy-then-delete-source: the
same shape as the cross-device fallback the executor already implements.

**The copy was never the hard part.** `copy_no_clobber` gets its
never-overwrite guarantee from `O_EXCL`, and that has no universal object-store
equivalent. S3 added conditional writes via `If-None-Match`; R2, B2 and SMB
vary. Whatever `Organizable` looks like, **"the substrate can refuse to
overwrite"** is a capability it must require, not assume.

---

## Suggested sequencing

1. **Rule-based fast path and link modes.** Cheap, high value, purely additive,
   no new abstraction. Provenance groundwork is already in place.
2. **Note-vault support** — as a richer context builder for the filesystem
   target, correctly understood as a feature rather than a second target.
3. ~~**`bower-llm` decoupling**, on its own merits, once the branches are reviewed.~~
   Done — see above.
4. **Mail** only if a second domain becomes a real goal — and it is the one that
   would genuinely reshape the abstraction, which is exactly why it should not
   be approached casually.

---

## Open questions

- Is a second `Organizable` target an actual goal, or hypothetical for the next
  several milestones? Everything above assumes hypothetical. **Evidence that
  would settle it:** someone actually wanting mail triage badly enough to use it.
- Filename template syntax is still provisional (ADR-0001 open question):
  conditional and fallback tokens remain undesigned.
- Capability auto-probing for backends was considered and deferred in
  [ADR-0004](ADR-0004-structured-output-and-the-openai-adapter.md): it costs a round trip and its
  failure mode is a silent downgrade.
