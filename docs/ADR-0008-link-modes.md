# ADR-0008: Link modes

**Status:** Accepted
**Date:** 2026-11-09
**Relates to:** [ADR-0001](ADR-0001-bowerbird-architecture.md),
[ADR-0005](ADR-0005-journal-provenance.md), [ROADMAP](ROADMAP.md)

## Context

Bowerbird moves files. That is the right default, but it is a commitment: the
inbox is emptier afterwards, and a nervous user has no way to try the tool
against a directory they still need to look the way it did.

A link mode gives a **non-destructive organized view**. The source stays where
it is and the category directories fill with links to it.

The roadmap called hardlink mode "nearly free", since the executor already
hard-links before it unlinks. That was true of the executor and of nothing else.
Leaving the source in place changes what a *second run* sees, and that is where
all the design went.

## The decision

A profile gains `link_mode`:

| Mode | Effect | Constraint |
| ---- | ------ | ---------- |
| `move` (default) | Today's behaviour. | |
| `hardlink` | A hard link at the destination; the source stays. | Same filesystem as the source. |
| `symlink` | A symlink at the destination pointing at the source, which stays. | None; dangles if the source goes. |

Only the placement into the organized view honours it. Parking a file for
review, recycling, and restoring remain real moves: they exist to get a file out
of the way, and a link would leave it exactly where it was.

The journal records `hardlink` or `symlink` as the action, not `move`, so the
record says what happened without the reader needing to know the config of the
day.

### No fallback to copying

Moving across filesystems falls back to a copy, because the user asked for the
file to end up there. Someone who asked for *links* did not ask for a second
copy of every file, and quietly making one would defeat the mode while looking
like it worked. A hard link across filesystems is therefore an error that names
`symlink` as the way out. `link(2)` and `symlink(2)` both fail with `EEXIST`
rather than replacing anything, so the never-overwrite guarantee carries over
unchanged.

## The part that was actually hard: a second run

With the source left behind, the next run sees it as a new file. Three things go
wrong, and each is handled:

1. **The model is asked again**, which is the slowest thing the tool does, and
   if it answers differently the file is linked into a *second* category. Fixed
   by skipping a source the journal says was already linked, **while that link
   still exists** (`Store::linked_sources`, `exec::is_link_to`). The skip is
   reported as `already linked`. Deleting the link makes the next run file the
   source again, which is the behaviour a person who deleted it would expect.
2. **The tool's own link looks like a conflict.** The collision check treats a
   symlink as "something else" and hashes a hard link against itself. A link
   that already leads to the file being placed is now recognised as that file's
   own earlier placement and reported as identical, so it is not quarantined as
   "a different file already exists". This holds even when the journal has been
   lost, because it is decided from the disk, not from the record. A *different*
   file at the destination is still a conflict (tested).
3. **The organized view is re-ingested.** Not new: the scanner already excludes
   directories the journal says the profile wrote into, and skips symlinks.

## Approval honours the mode

Approving a queued item places it the way the profile says. Otherwise a person
could run a symlink profile and find that approving anything quietly moved the
file out of the inbox.

## Consequences

- `link_mode = "move"` is byte-for-byte the old behaviour.
- A hard-linked file edited in place changes under both names; one replaced by a
  new file (an editor that writes a temporary file and renames it over the old
  one) is a different file with the old link left holding the old content. The
  next run notices the link no longer matches and files the new file too. This
  is how hard links work, not something the tool can paper over.
- A symlink target is the absolute source path, so the link survives being
  copied within the tree but not the *source* being moved. The organized view and
  the inbox are independent trees, so a relative target would have been wrong.
- If someone moves a link elsewhere by hand, the next run does not find it where
  the journal says and files the source again. Documented, not defended against.
- Unix only for now: symlink mode returns a clear error elsewhere.

## Not done

- **Undo.** The journal has everything needed to remove the links a run made, and
  removing a link loses nothing. No command does it yet.
- **Re-pointing links** when a source moves.
