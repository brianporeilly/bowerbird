#!/usr/bin/env python3
"""Score each lab profile against a hand-labelled ground truth.

`lab_compare.py` shows where models disagree. It cannot say who is right, and
"the confidence gate fires" is a different claim from "the gate fires on the
wrong files". This script measures the second.

Labels live in a TOML file (default lab/labels.toml, gitignored with the rest
of the lab, since the filenames are yours):

    [labels]
    "acme-invoice-2024.txt" = "Invoices"
    "Z87 ROM.zip"           = ["Archives", "Installers"]   # either is fine
    "untitled-scan-0043.txt" = "?"                          # see below

`"?"` means the taxonomy has no right answer for this file, so the *correct*
behaviour is to hold it for a human. Filing one automatically is a failure no
matter which category was chosen.

For each profile, the latest outcome per file wins (by timestamp, across both
the journal and the review queue), so re-running a model replaces its earlier
answer without anything being deleted.

The number that matters most is **silent errors**: files the tool moved with no
human involved that were wrong, plus "?" files it moved at all. A held file is
the system working; a wrongly moved one is the system failing quietly.
"""

import sqlite3
import sys
import tomllib
from pathlib import Path

UNANSWERABLE = "?"


def load_labels(path):
    with open(path, "rb") as f:
        raw = tomllib.load(f).get("labels", {})
    labels = {}
    for name, value in raw.items():
        accepted = {value} if isinstance(value, str) else set(value)
        labels[name] = accepted
    return labels


def load_outcomes(db):
    """{(profile, filename): (kind, category, confidence)}, latest per key.

    kind is "moved" or "held". For a held file the category is what the model
    *proposed*, which tells us whether approving it would have been right.
    """
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    events = []
    for r in con.execute(
        "SELECT profile, source, dest_dir, confidence, at FROM journal "
        "WHERE phase = 'committed'"
    ):
        category = Path(r["dest_dir"]).name if r["dest_dir"] else ""
        events.append((r["at"], r["profile"], Path(r["source"]).name,
                       "moved", category, r["confidence"]))
    for r in con.execute(
        "SELECT profile, original_path, category, confidence, created_at "
        "FROM review_queue WHERE kind = 'review'"
    ):
        events.append((r["created_at"], r["profile"], Path(r["original_path"]).name,
                       "held", r["category"], r["confidence"]))
    con.close()

    events.sort(key=lambda e: e[0])
    out = {}
    for _, profile, name, kind, category, conf in events:
        out[(profile, name)] = (kind, category, conf)
    return out


def mean(xs):
    return sum(xs) / len(xs) if xs else None


def fmt(x):
    return "  - " if x is None else f"{x:.2f}"


def main():
    argv = sys.argv[1:]
    verbose = "-v" in argv
    argv = [a for a in argv if a != "-v"]
    if len(argv) < 2:
        print("usage: lab_score.py [-v] <state.db> <labels.toml> [profile ...]",
              file=sys.stderr)
        return 2

    db, labels_path, wanted = argv[0], argv[1], set(argv[2:])
    if not Path(labels_path).is_file():
        print(f"no labels at {labels_path}", file=sys.stderr)
        return 1

    labels = load_labels(labels_path)
    outcomes = load_outcomes(db)
    profiles = sorted({p for p, _ in outcomes if not wanted or p in wanted})
    if not profiles:
        print("no matching runs recorded")
        return 0

    answerable = {n for n, a in labels.items() if UNANSWERABLE not in a}
    unanswerable = {n for n, a in labels.items() if UNANSWERABLE in a}

    rows = []
    details = {}
    for p in profiles:
        moved_ok, moved_bad, held_ok, held_bad = [], [], [], []
        trap_held, trap_moved = [], []
        conf_ok, conf_bad = [], []
        for name, accepted in labels.items():
            got = outcomes.get((p, name))
            if got is None:
                continue
            kind, category, conf = got
            if name in unanswerable:
                (trap_moved if kind == "moved" else trap_held).append(name)
                continue
            right = category in accepted
            if kind == "moved":
                (moved_ok if right else moved_bad).append(name)
                if conf is not None:
                    (conf_ok if right else conf_bad).append(conf)
            else:
                (held_ok if right else held_bad).append(name)

        scored = len(moved_ok) + len(moved_bad) + len(held_ok) + len(held_bad)
        moved = len(moved_ok) + len(moved_bad)
        silent = len(moved_bad) + len(trap_moved)
        rows.append((
            p, scored + len(trap_held) + len(trap_moved), moved,
            len(moved_ok) / moved if moved else None,
            len(moved_bad), len(trap_moved), silent,
            len(held_ok), len(held_bad), len(trap_held),
            mean(conf_ok), mean(conf_bad),
        ))
        details[p] = (moved_bad, trap_moved, held_ok)

    headers = ["profile", "files", "moved", "precision", "wrong", "trap-moved",
               "SILENT", "held-ok", "held-wrong", "trap-held", "conf(ok)", "conf(bad)"]
    table = []
    for r in rows:
        table.append([
            r[0], str(r[1]), str(r[2]), fmt(r[3]), str(r[4]), str(r[5]),
            str(r[6]), str(r[7]), str(r[8]), str(r[9]), fmt(r[10]), fmt(r[11]),
        ])
    widths = [max(len(h), *(len(t[i]) for t in table)) for i, h in enumerate(headers)]
    print("  ".join(h.ljust(w) for h, w in zip(headers, widths)))
    print("  ".join("-" * w for w in widths))
    for t in table:
        print("  ".join(c.ljust(w) for c, w in zip(t, widths)))

    print()
    print(f"{len(answerable)} answerable file(s), {len(unanswerable)} unanswerable "
          f"(\"?\"), {len(labels)} labelled in total.")
    print("  precision   of files moved automatically, the share filed correctly")
    print("  wrong       moved automatically into an unaccepted category")
    print("  trap-moved  \"?\" files that were moved at all (should have been held)")
    print("  SILENT      wrong + trap-moved: errors no human saw. Lower is better.")
    print("  held-ok     held for review, and the model's proposal was right")
    print("  held-wrong  held for review, and the proposal was wrong (gate earned its keep)")
    print("  trap-held   \"?\" files correctly held")
    print("  conf(ok/bad) mean confidence of correct vs wrong automatic moves; a gap")
    print("              is the gate's discrimination, a missing 'bad' means no errors")

    if verbose:
        for p in profiles:
            moved_bad, trap_moved, held_ok = details[p]
            if not (moved_bad or trap_moved):
                continue
            print()
            print(f"{p}: silent errors")
            for n in sorted(moved_bad):
                kind, cat, conf = outcomes[(p, n)]
                want = "/".join(sorted(labels[n]))
                print(f"  wrong      {n}: filed {cat} {fmt(conf)}, wanted {want}")
            for n in sorted(trap_moved):
                kind, cat, conf = outcomes[(p, n)]
                print(f"  trap-moved {n}: filed {cat} {fmt(conf)}, should have been held")

    unlabelled = sorted({n for (_, n) in outcomes} - set(labels))
    if unlabelled:
        print()
        print(f"{len(unlabelled)} file(s) in the runs have no label and were not scored:")
        for n in unlabelled[:10]:
            print(f"  {n}")
        if len(unlabelled) > 10:
            print(f"  ... and {len(unlabelled) - 10} more")
    return 0


if __name__ == "__main__":
    sys.exit(main())
