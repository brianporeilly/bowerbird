#!/usr/bin/env python3
"""Compare what each model did with the same corpus.

Reads the lab's state database directly. Everything needed is already
recorded: the journal carries the profile that proposed each operation, the
directory it chose, and -- since ADR-0005 -- the confidence behind it. The
review queue carries the files no model would commit to.

Only the most recent run per profile counts, so re-running one model does not
leave its earlier answers in the table. Nothing is deleted to achieve that; the
journal stays append-only and the query just takes the latest row per file.
"""

import sqlite3
import sys
from pathlib import Path

HELD = "—"


def completed(db, profile, since):
    """How many files of `profile`'s current run have reached a terminal state.

    Read-only, and safe to call while a run is in flight: the store opens in
    WAL mode, so a reader never blocks the writer.

    `since` is a unix timestamp taken just before the run started, and it is not
    optional. The journal is append-only and the lab does not wipe it between
    runs, so an unscoped count includes every previous run of the same profile
    -- which on a corpus run repeatedly reads as progress well past 100%.

    A file counts once it has either a non-intent journal row (it moved, or the
    move failed) or a review-queue row (a human has to decide). An `intent` with
    no result is a file still in flight, and is deliberately not counted.

    Returns 0 rather than raising if the database is not there yet -- on a first
    run it is created a moment after the process starts.
    """
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    except sqlite3.Error:
        return 0
    try:
        done = con.execute(
            "SELECT COUNT(DISTINCT source) FROM journal "
            "WHERE profile = ? AND phase != 'intent' AND at >= ?",
            (profile, since),
        ).fetchone()[0]
        queued = con.execute(
            "SELECT COUNT(*) FROM review_queue WHERE profile = ? AND created_at >= ?",
            (profile, since),
        ).fetchone()[0]
        return done + queued
    except sqlite3.Error:
        # Mid-migration, or the schema is newer than this script. Progress is
        # not worth failing a run over.
        return 0
    finally:
        con.close()


def load(db):
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row

    committed = con.execute(
        """
        SELECT profile, source, dest_dir, confidence, at
        FROM journal
        WHERE phase = 'committed'
        ORDER BY at ASC
        """
    ).fetchall()

    queued = con.execute(
        """
        SELECT profile, original_path, kind, reason, confidence
        FROM review_queue
        """
    ).fetchall()
    con.close()

    # Later rows overwrite earlier ones, so a re-run wins without deleting
    # anything.
    cells, profiles, files = {}, set(), set()
    for r in committed:
        name = Path(r["source"]).name
        profiles.add(r["profile"])
        files.add(name)
        category = Path(r["dest_dir"]).name if r["dest_dir"] else "?"
        cells[(name, r["profile"])] = (category, r["confidence"])

    for r in queued:
        name = Path(r["original_path"]).name
        profiles.add(r["profile"])
        files.add(name)
        cells.setdefault((name, r["profile"]), (HELD, r["confidence"]))

    return sorted(files), sorted(profiles), cells


def main():
    argv = sys.argv[1:]

    # `--completed PROFILE DB` is used by lab.sh's progress line, which polls
    # while a run is in flight.
    if argv and argv[0] == "--completed":
        # --completed PROFILE SINCE DB
        print(completed(argv[3], argv[1], int(argv[2])))
        return 0

    db = argv[0] if argv else "lab/state.db"
    files, profiles, cells = load(db)

    if not files:
        print("no runs recorded yet")
        return 0

    def render(cell):
        if cell is None:
            return "·"
        category, confidence = cell
        if category == HELD:
            return HELD
        return f"{category} {confidence:.2f}" if confidence is not None else category

    width = max([len(f) for f in files] + [4])
    # Widest actual cell, not just the widest header: "Correspondence 0.98" is
    # longer than any profile name and would otherwise run into the next column.
    rendered = [render(cells.get((f, p))) for f in files for p in profiles]
    colw = max([len(p) for p in profiles] + [len(c) for c in rendered] + [6]) + 2

    print(f"{'file':<{width}}  " + "".join(f"{p:<{colw}}" for p in profiles))
    print("-" * (width + 2 + colw * len(profiles)))

    agree = disagree = held = 0
    for name in files:
        row = [cells.get((name, p)) for p in profiles]
        print(f"{name:<{width}}  " + "".join(f"{render(c):<{colw}}" for c in row))

        categories = {c[0] for c in row if c is not None}
        if HELD in categories:
            held += 1
        elif len(categories) > 1:
            disagree += 1
        elif len(categories) == 1:
            agree += 1

    print()
    if len(profiles) < 2:
        # "7 agreed" across one model is not a fact about anything.
        print(f"{len(files)} file(s), one model. Nothing to compare against yet -- "
              f"run a second model to get a disagreement column.")
    else:
        print(f"{len(files)} file(s) across {len(profiles)} model(s): "
              f"{agree} agreed, {disagree} disagreed, "
              f"{held} held for review by at least one")

    if len(profiles) > 1 and (disagree or held):
        print()
        print("Disagreement is the interesting column. A file every model files")
        print("identically tells you little; one they split on is where the")
        print("taxonomy is ambiguous or the metadata is too thin to decide.")

    # Confidence is only meaningful if it varies. The live run against
    # Qwen2.5-3B returned 0.95-0.99 for everything, including a misfile.
    for p in profiles:
        scores = [
            c[1] for (_, prof), c in cells.items()
            if prof == p and c[1] is not None and c[0] != HELD
        ]
        if len(scores) >= 3:
            spread = max(scores) - min(scores)
            if spread < 0.10:
                print()
                print(f"note: {p} returned confidence {min(scores):.2f}-{max(scores):.2f} "
                      f"across {len(scores)} files.")
                print("      A gate set anywhere in that band never fires. Self-reported")
                print("      confidence from this model is not a usable signal.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
