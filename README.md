# Bowerbird

LLM-assisted file organization for directories that accumulate mess — a
downloads folder, a scanner inbox, anything you keep meaning to sort out.

**The model never touches your filesystem.** It only proposes: a category, some
filename tokens, a confidence score, and its reasoning. A deterministic policy
engine validates every proposal against your configuration and resolves it into
one of a fixed, closed set of actions. A separate executor is the only component
that writes anything.

The binary is `bower`.

> **Status: pre-1.0, under active development.** The pipeline runs end to end
> against the built-in offline classifier, with the journal, review queue and
> recycle store in place. No real LLM backend adapter has shipped yet — see
> [Roadmap](#roadmap).

## Why this design

The usual version of this tool hands an LLM a shell and asks it to tidy up.
That gets you hallucinated paths, silent overwrites, no undo, and no way to hold
a model to a fixed taxonomy. Bowerbird inverts it. The model is a classifier,
not an operator, and the interesting engineering is in the layer that refuses
to do what the model asks.

Concretely:

- **The model never emits a path.** It returns a category name and filename
  tokens. Paths are built in exactly one place, in code, from validated
  components. Most path-traversal and injection concerns are gone before any
  validation logic runs.
- **The action space has no destructive variant.** `ResolvedAction` cannot
  express permanent deletion or a write outside a profile's destination root.
  Unsafe operations are not rejected at runtime — they cannot be named.
- **The policy engine performs no I/O.** It is a pure function of its inputs, so
  its behaviour is exhaustively testable without a filesystem. A test in CI
  enforces the property mechanically.
- **Nothing is ever overwritten.** The executor does not use `rename(2)`, which
  silently replaces its destination. It links-then-unlinks, or copies into a
  file opened `O_EXCL`. A lost race is reported, never resolved by destroying
  something.
- **Deletion is a separate, guarded tier.** Off by default per profile. A
  deletion suggestion always lands in the review queue regardless of confidence,
  with no configuration that auto-executes it. Approving one moves the file to a
  recycle store; permanent removal needs an explicit, separate command.

## Install

Requires Rust 1.97 or newer.

```sh
git clone https://github.com/brianporeilly/bowerbird
cd bowerbird
cargo build --release
```

For a static binary you also need a musl C toolchain, since the bundled SQLite
compiles C (`apt install musl-tools`, or `dnf install musl-gcc`):

```sh
just release-static
```

## Quick start

```sh
cp bowerbird.example.toml bowerbird.toml
$EDITOR bowerbird.toml

bower config check                              # validate without running
bower run --profile downloads --stub-llm        # dry run, offline classifier
```

`--stub-llm` uses a built-in deterministic classifier that needs no model, key,
or network. It exists so you can see exactly what the pipeline would do to your
files before pointing a real model at them.

The config ships with `dry_run = true`. Nothing is written until you pass
`--execute`.

## In place, or somewhere else

**`destination_root` defaults to `path`.** Omit it and files are organized *in
place*: sorted into category subdirectories of the directory they already live
in.

```toml
[[profiles]]
name = "downloads"
path = "/data/downloads"
# destination_root omitted -> /data/downloads/Documents/, /data/downloads/Images/, ...
```

**You are not required to organize in place, and often shouldn't.** Point
`destination_root` somewhere else and the source directory stays a pure inbox:

```toml
[[profiles]]
name = "personal-docs"
path = "/data/documents/inbox"
destination_root = "/data/documents/organized"
```

Either way, Bowerbird never re-ingests its own output. When organizing in place
it skips the category subdirectories it manages; when routing elsewhere it skips
the destination root.

## Commands

```
bower run --profile NAME [--profile NAME2 ...]   one or more named profiles
bower run --all                                  every enabled profile
bower run                                        exactly one profile defined -> run it
                                                 more than one -> error, list them

bower config check                               validate config, list profiles

bower review list [--profile N] [--type T]       what is waiting on a decision
bower review show <id>                           one item in full
bower review approve <id> | --all [--yes]        carry the decision out
bower review reject <id> [--reason "..."]        refuse it, and remember

bower recycle list                               what has been recycled
bower recycle restore <id>                       put it back
bower recycle purge --older-than 30d [--dry-run] permanently delete
```

Flags: `--execute` writes (overriding `dry_run = true`), `--dry-run` forces a
preview, `--stub-llm` uses the offline classifier, `-v` repeats for more logging.

Exit codes are meaningful for unattended cron use:

| Code | Meaning |
| ---- | ------- |
| `0`  | Clean run, nothing pending |
| `1`  | Hard error — bad config, backend unreachable, filesystem failure |
| `2`  | Succeeded, but items are waiting on a human |

Locking is per profile (`lock_file_dir/<name>.lock`), so schedules for different
directories never block each other. `--all` skips a profile that is already
running; `--profile NAME` fails loudly, because you asked for that one.

## How a file moves through it

```
scanner ──▶ context builder ──▶ LLM client ──▶ policy engine ──▶ executor
   │                                               │                 │
   │                                    the only component      the only
   │                                    that decides            component
   │                                                            that writes
   └── never reads the tool's own output directories
```

The policy engine's stages, in order — any stage can route a file to manual
review, none can skip a later one:

1. Schema validation
2. Staleness check — did the file change since it was scanned?
3. Category resolution against `categories` / `allow_dynamic_categories`
4. Filename token sanitization and template rendering
5. Path construction — the only place a path is built
6. Collision check — content hash comparison; identical is a duplicate, different
   is never an overwrite
7. Confidence gate (deletion suggestions ignore this entirely — always manual)

Anything the engine will not decide lands in the review queue, carrying the
destination it would have used, so approving it later is a replay of a decision
already made rather than a fresh trip to the model.

## Layout

| Crate | Role |
| ----- | ---- |
| `bower-config` | TOML schema, defaulting, validation |
| `bower-core` | Scanner, policy engine, executor, state store, locking, orchestration |
| `bower-llm` | Backend adapters; the trait itself lives in `bower-core` |
| `bower-cli` | The `bower` binary |

## Development

```sh
just            # fmt check, clippy, tests -- the full local gate
just test
just cov        # coverage summary
just mutants    # are the tests testing anything?
just deny       # advisories, licenses, sources
just demo       # dry run against a throwaway directory tree
```

## Roadmap

Shipped:

- Config, scanner, policy engine, executor, per-profile locking
- Dry-run and execute paths, offline `--stub-llm` classifier
- SQLite state store: append-only journal, review queue, remembered rejections,
  recycle store — with `bower review` and `bower recycle`

Next:

- OpenAI-compatible backend adapter, then Anthropic-compatible
- Filename template syntax, still provisional (ADR-0001, Open Questions)

Deferred by design — see [ADR-0001](docs/ADR-0001-bowerbird-architecture.md):
watcher daemon, notifications, TUI/GUI front-ends, media-library conventions.

[ADR-0002](docs/ADR-0002-implementation-amendments.md) and
[ADR-0003](docs/ADR-0003-state-store-amendments.md) record where the
implementation amends ADR-0001, and why.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual-licensed as above, without any
additional terms or conditions.
