#!/usr/bin/env python3
"""Prototype: classify the lab corpus by embedding similarity instead of generation.

Each file's metadata and each category are embedded, and a file goes to the
nearest category by cosine similarity. Nothing is moved and nothing is written:
this only prints what it would have chosen, next to what the generative
profiles in the lab's state database actually chose.

The model sees the same things the context builder discloses at
content_sniff_bytes = 0 -- file name and MIME type -- so the comparison with
the generative columns is like for like.

    python3 scripts/lab_embed.py                      # bare category names
    python3 scripts/lab_embed.py --described          # with one-line descriptions

With hand labels (lab/labels.toml, see lab_score.py) it also sweeps the margin
gate against ground truth rather than against another model's choices, and
reports the number that matters: files filed automatically that were wrong.

Needs an OpenAI-compatible /v1/embeddings endpoint (llama.cpp --embeddings).
"""

import argparse
import json
import math
import sqlite3
import subprocess
import sys
import time
import tomllib
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
GATES = (0.0, 0.01, 0.02, 0.03, 0.04, 0.05, 0.07, 0.10)

# Hypothetical. The config has no per-category description today; these exist
# to measure whether adding one would be worth a config field.
DESCRIPTIONS = {
    "Invoices": "bills, invoices, receipts, statements and payment records",
    "Reports": "reports, summaries, analyses and exported results",
    "Correspondence": "letters, emails, claims and messages to or from a person",
    "Manuals": "user manuals, guides, instructions and reference documentation",
    "Photos": "photographs, pictures, images, screenshots and graphics",
    "Archives": "zip files, compressed archives, backups and bundles of files",
    "Installers": "software installers, setup programs, packages and executables",
}


def embed(endpoint, texts):
    req = urllib.request.Request(
        f"{endpoint}/embeddings",
        data=json.dumps({"input": texts}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=300) as resp:
        data = json.load(resp)["data"]
    return [d["embedding"] for d in sorted(data, key=lambda d: d["index"])]


def cosine(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    return dot / (math.sqrt(sum(x * x for x in a)) * math.sqrt(sum(y * y for y in b)))


def mime_of(path):
    out = subprocess.run(
        ["file", "-b", "--mime-type", str(path)], capture_output=True, text=True, check=False
    )
    return out.stdout.strip() or "unknown"


def generative_columns(db):
    """Latest committed category per file per profile, as lab_compare reads it."""
    if not Path(db).exists():
        return {}, []
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = con.execute(
        "SELECT profile, source, dest_dir FROM journal WHERE phase='committed' ORDER BY at"
    ).fetchall()
    con.close()
    cells, profiles = {}, set()
    for profile, source, dest_dir in rows:
        profiles.add(profile)
        cells[(Path(source).name, profile)] = Path(dest_dir).name if dest_dir else "?"
    return cells, sorted(profiles)


def score_against_labels(rows, labels_path):
    """Sweep the margin gate against hand labels.

    A file whose margin clears the gate is "filed automatically"; the rest are
    held for a human. Filing a wrong answer, or filing a file the taxonomy has
    no answer for ("?"), is a silent error. Holding is always safe.
    """
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from lab_score import UNANSWERABLE, load_labels

    labels = load_labels(labels_path)
    scored = [(m, cat, labels[name]) for m, name, cat, _, _ in rows if name in labels]
    traps = sum(1 for _, _, acc in scored if UNANSWERABLE in acc)
    print(f"  against {Path(labels_path).name}: {len(scored)} labelled files "
          f"({len(scored) - traps} answerable, {traps} with no right answer)")
    print(f"    {'margin >=':<10} {'filed':>5} {'right':>5} {'wrong':>5} {'trap':>5} "
          f"{'SILENT':>6} {'held':>5}")
    for gate in GATES:
        filed = [(c, acc) for m, c, acc in scored if m >= gate]
        right = sum(1 for c, acc in filed if UNANSWERABLE not in acc and c in acc)
        trap = sum(1 for c, acc in filed if UNANSWERABLE in acc)
        wrong = len(filed) - right - trap
        print(f"    {gate:<10.2f} {len(filed):>5} {right:>5} {wrong:>5} {trap:>5} "
              f"{wrong + trap:>6} {len(scored) - len(filed):>5}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", default="http://127.0.0.1:8089/v1")
    ap.add_argument("--lab", default=str(REPO / "lab"))
    ap.add_argument("--corpus", help="directory of files to classify (default <lab>/corpus)")
    ap.add_argument("--described", action="store_true")
    ap.add_argument("--labels", default=str(REPO / "lab" / "labels.toml"),
                    help="hand labels to score against (default lab/labels.toml)")
    ap.add_argument("--reference", help="profile to treat as the reference when scoring the margin gate")
    args = ap.parse_args()

    lab = Path(args.lab)
    config = tomllib.loads((lab / "lab.toml").read_text())
    categories = config["profiles"][0]["categories"]
    corpus = Path(args.corpus) if args.corpus else lab / "corpus"
    files = sorted(p for p in corpus.iterdir() if p.is_file())

    # EmbeddingGemma's documented prompt for classification.
    prefix = "task: classification | query: "
    if args.described:
        cat_texts = [f"{prefix}{c}: {DESCRIPTIONS.get(c, c)}" for c in categories]
    else:
        cat_texts = [f"{prefix}{c}" for c in categories]
    file_texts = [f"{prefix}file name: {p.name} | type: {mime_of(p)}" for p in files]

    started = time.monotonic()
    cat_vecs = embed(args.endpoint, cat_texts)
    file_vecs = embed(args.endpoint, file_texts)
    took = time.monotonic() - started

    cells, profiles = generative_columns(lab / "state.db")

    width = min(max(len(p.name) for p in files), 46)
    print(f"{'file':<{width}}  {'embedding':<16} {'sim':>5} {'margin':>6}  "
          + "  ".join(f"{p:<16}" for p in profiles))
    print("-" * (width + 34 + 18 * len(profiles)))

    agree = {p: [0, 0] for p in profiles}
    rows = []
    for path, vec in zip(files, file_vecs):
        scored = sorted(((cosine(vec, cv), c) for cv, c in zip(cat_vecs, categories)), reverse=True)
        (top, cat), (second, _) = scored[0], scored[1]
        others = [cells.get((path.name, p), "·") for p in profiles]
        for p, o in zip(profiles, others):
            if o != "·":
                agree[p][1] += 1
                agree[p][0] += o == cat
        rows.append((top - second, path.name, cat, top, others))

    for margin, name, cat, top, others in sorted(rows, key=lambda r: r[1].lower()):
        print(f"{name[:width]:<{width}}  {cat:<16} {top:5.2f} {margin:6.3f}  "
              + "  ".join(f"{o:<16}" for o in others))

    print()
    print(f"{len(files)} files and {len(categories)} categories embedded in {took:.1f}s "
          f"({'described' if args.described else 'bare'} category names)")
    for p in profiles:
        hit, total = agree[p]
        if total:
            print(f"  agrees with {p:<32} {hit}/{total}")
    # Would the margin work as a gate? Unlike a model's self-reported
    # confidence it is measured, so the question is whether the files it is
    # unsure about are the ones it gets wrong. Scored against one generative
    # profile, which is a reference, not ground truth.
    if args.reference in profiles:
        idx = profiles.index(args.reference)
        scored = [(m, cat == o[idx]) for m, _, cat, _, o in rows if o[idx] != "·"]
        print(f"  margin gate, scored against {args.reference} ({len(scored)} files it committed):")
        for gate in (0.0, 0.02, 0.03, 0.04, 0.05):
            kept = [ok for m, ok in scored if m >= gate]
            if kept:
                print(f"    margin >= {gate:.2f}: files {len(kept):>2}/{len(scored)}, "
                      f"agree {sum(kept)}/{len(kept)}")
    if Path(args.labels).is_file():
        score_against_labels(rows, args.labels)
    margins = sorted(r[0] for r in rows)
    print(f"  margin (top1 - top2): min {margins[0]:.3f}, median "
          f"{margins[len(margins) // 2]:.3f}, max {margins[-1]:.3f}")


if __name__ == "__main__":
    main()
