# ADR-0001: Bowerbird — LLM-Assisted File Organization Tool

**Status:** Accepted (initial architecture, pre-implementation)
**Date:** 2026-08-29
**Binary name:** `bower`

## Context

Existing "AI organizes my downloads folder" scripts typically let an LLM handle
file operations directly, which is unsafe and unpredictable — hallucinated
paths, silent overwrites, no undo, no way to enforce a fixed category
taxonomy, and no protection against destructive actions.

Bowerbird's core premise: **the LLM never touches the filesystem.** It
proposes structured, constrained suggestions (category, filename tokens,
confidence, reasoning). A deterministic policy engine validates every
proposal against user-defined rules before a separate executor performs any
actual filesystem operation. The LLM is a classifier, not an operator.

Primary use cases:
- One-time cleanup of an unorganized directory (e.g. Downloads)
- Ongoing maintenance via cron across one or more directories, each with its
  own purpose/rules
- Eventually (not v1): media-library-style organization (movies/shows/music/
  books) akin to the *arr apps, and GUI/TUI front-ends on top of the same
  core

Non-goals for v1: watcher daemon, notification integrations, GUI, media
library renaming/tagging conventions. These are explicitly deferred, not
rejected — see Open Questions.

## Decisions

### 1. Language: Rust

Chosen over Go and Python. Rationale:
- The action space the LLM can trigger is modeled as a Rust enum with no
  destructive variants (e.g. no `Delete`), making unsafe actions
  unrepresentable at compile time rather than only rejected at runtime.
- `lofty` provides strong, unified audio/tag metadata extraction across
  formats, which matters for filename-poor files (music, audiobooks).
- Static binaries, easy cross-compilation (`cross`/`cargo zigbuild`), clean
  container-native distribution.
- The concurrency shape needed here (worker pool → LLM call → structured
  result → policy engine) is a well-trodden `tokio` pattern, not the kind of
  complex shared-mutable-state async code that makes Rust painful.
- Go was a reasonable second choice (faster iteration, simpler concurrency
  model) but was passed over because the type-system guarantee is directly
  relevant to the tool's core safety promise, and team fluency was reported
  as roughly equal between the two.

### 2. Architecture: five-layer pipeline

1. **Scanner** — walks configured directories, builds file records (path,
   size, mtime, extension, MIME via magic bytes, optional deep metadata).
   Always excludes each profile's `destination_root` (even when nested
   inside `path`) from the source list, to prevent the tool from re-scanning
   its own output as new unorganized input.
2. **Context builder** — assembles what's sent to the LLM per file/batch,
   gated by per-profile config toggles (MIME only vs. deep metadata vs.
   content snippet).
3. **LLM client** — pluggable adapter per API family (OpenAI-compatible vs.
   Anthropic-compatible are not wire-compatible; each needs its own adapter).
   Requests are **batched** (default batch size configurable per profile);
   **validation of responses is per-item**, so one malformed entry in a batch
   does not sink the rest.
4. **Policy engine** — the enforcement layer. Validates every LLM proposal
   against config and resolves it into a closed `ResolvedAction`. Can only
   downgrade trust (e.g. force manual review), never escalate it.
5. **Executor** — the only component that touches the filesystem. Performs
   atomic moves, writes to the journal before and after each operation, never
   calls delete/unlink directly (see Recycle Bin, below).

### 3. LLM proposal contract

The LLM never emits a path — only structured tokens. This eliminates most
path-traversal and injection risk before validation logic even runs.

```json
{
  "file_id": "f_0af3c1",
  "category": "Invoices",
  "is_new_category": false,
  "name_tokens": { "date": "2024-03-15", "vendor": "Acme", "doc_type": "invoice" },
  "confidence": 0.87,
  "reasoning": "Extracted text contains invoice number and vendor name matching pattern."
}
```

- `file_id` references the scanner's stable index, not a filesystem path.
- `name_tokens` are filled into a **user-configurable filename template**
  (syntax deliberately left undesigned for now — deferred, see Open
  Questions) — the LLM fills blanks, it does not compose filenames or paths.
- `confidence` and `reasoning` are required; they drive review thresholds and
  the audit journal, and are never executed as instructions.
- A distinct proposal type, `SuggestDelete { reason, confidence }`, exists
  for the recycle-bin flow (see below) and is handled with stricter rules
  than categorization proposals.

### 4. Policy engine / resolved action space

```rust
enum ResolvedAction {
    Move { dest: PathBuf },
    MoveAndRename { dest: PathBuf },
    Quarantine { reason: String },       // conflict or review parking, not deletion
    RecycleSuggested { reason: String, confidence: f32 }, // always manual, never auto
    NeedsManualReview { reason: String, raw: RawProposal },
    NoOp,
}
```

No variant can express permanent deletion or a write outside a profile's
`destination_root`. Pipeline stages, applied in order (any stage can route to
`NeedsManualReview`; none can skip a later stage):

1. Parse/schema validation (malformed → one retry with error fed back to the
   model → still bad → manual review)
2. Staleness check (file unchanged since scan? if not, discard)
3. Category resolution against `categories` / `allow_dynamic_categories`
4. Filename token sanitization + template rendering (skipped entirely if
   `rename.enabled = false`)
5. Deterministic path construction — the only place a path is built, entirely
   in code
6. Collision check — hash comparison against any existing file at `dest`
   (identical → skip as duplicate; different → **never overwrite**, apply
   `on_conflict` policy)
7. Confidence gate against `confidence_threshold` (delete suggestions ignore
   this gate entirely — always manual, unconditionally)
8. Dry-run gate

### 5. Config format: TOML

Chosen for clean `serde` integration, flatter structure than YAML (avoids
YAML's implicit-typing footguns in a file that governs filesystem mutation),
and natural fit for "one global section + an array of profile tables."

```toml
config_version = 1

[general]
dry_run = true
journal_path = "/var/lib/bowerbird/journal.db"
lock_file_dir = "/var/lib/bowerbird/locks"   # one lock file per profile, not global
log_level = "info"
default_batch_size = 25
default_confidence_threshold = 0.75
review_placement = "in_place"                # or "quarantine"
quarantine_dir = "/data/_review"
recycle_dir = "/data/_recycled"

[[llm_backends]]
name = "local-llama"
provider = "openai_compatible"
endpoint = "http://localhost:8080/v1"
api_key_env = ""
model = "llama-3.1-8b-instruct"
timeout_secs = 30
max_retries = 2

[[llm_backends]]
name = "anthropic-cloud"
provider = "anthropic_compatible"
endpoint = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-haiku-4-5"
timeout_secs = 30
max_retries = 2

[[profiles]]
name = "downloads"
path = "/data/downloads"
description = "General downloads folder. Mixed file types, no fixed structure expected."
enabled = true
llm_backend = "local-llama"
# destination_root omitted => defaults to `path` (in-place reorg into
# category subfolders). Set explicitly to route output elsewhere entirely —
# this is NOT required, just the common in-place case.
categories = ["Documents", "Images", "Installers", "Archives", "Media"]
allow_dynamic_categories = true
allow_delete_suggestions = true       # opt-in per profile; off by default
batch_size = 25
confidence_threshold = 0.7
on_conflict = "quarantine"            # skip | suffix | quarantine
stability_wait_minutes = 15
exclude_patterns = ["*.part", "*.crdownload", ".DS_Store"]
include_subdirs = false

[profiles.rename]
enabled = false

[profiles.metadata]
detect_mime = true
extract_exif = false
extract_audio_tags = false
extract_pdf_metadata = false
content_sniff_bytes = 0

[[profiles]]
name = "personal-docs"
path = "/data/documents/inbox"
description = "Scanned personal documents: invoices, tax records, medical, insurance."
enabled = true
llm_backend = "anthropic-cloud"
destination_root = "/data/documents/organized"   # explicit: route elsewhere
categories = ["Invoices", "Tax", "Medical", "Insurance", "Legal"]
allow_dynamic_categories = false
allow_delete_suggestions = false      # sensitive docs: never ask this question
confidence_threshold = 0.85
on_conflict = "quarantine"
content_sniff_bytes = 4000

[profiles.rename]
enabled = true
template = "{date}-{doc_type}-{vendor}{ext}"   # template syntax TBD, see Open Questions

[profiles.metadata]
detect_mime = true
extract_pdf_metadata = true
```

Notes:
- `destination_root` defaults to `path` (in-place organization into
  subfolders) when omitted. **This must be documented prominently**: it's an
  important behavioral default, and users should know they are not required
  to organize in place — pointing `destination_root` elsewhere is equally
  supported and often preferable.
- `api_key_env` only — secrets are read from environment variables, never
  stored inline in the config file.
- `llm_backend` is a name reference to `[[llm_backends]]`, enabling
  per-profile routing (e.g. local model for sensitive docs, cloud for
  generic downloads, or vice versa).

### 6. CLI / interaction model

**Cron-first, one-shot CLI as the core; daemon mode deferred but designed to
be additive later, not a rewrite** (a future `bowerbird watch` would call the
same per-profile run function, triggered by a debounced filesystem event
instead of a schedule).

```
bowerbird run --profile NAME [--profile NAME2 ...]   # one or more specific profiles
bowerbird run --all                                   # explicit, all enabled profiles
bowerbird run                                         # no flag:
                                                       #   exactly 1 profile defined -> run it
                                                       #   >1 profile defined -> error, list
                                                       #     profiles, require --profile/--all
```

- Locking is **per-profile** (`lock_file_dir/<profile-name>.lock`), not
  global, so overlapping schedules for different profiles never block each
  other. `--all` skips any profile currently locked (logs it, does not fail
  the whole batch); `--profile NAME` fails loudly if that profile's lock is
  already held.
- Exit codes are meaningful for unattended cron use:
  - `0` — clean run, nothing pending
  - `1` — hard error (bad config, LLM unreachable, filesystem error)
  - `2` — succeeded, but items are sitting in manual review or recycle
    suggestions — warrants human attention

### 7. Review queue and recycle bin

Two SQLite-backed tables (same file, different mutability guarantees):

- **Journal** — append-only record of every *executed* action (what actually
  happened). Never edited.
- **Review queue** — mutable table of pending decisions (`NeedsManualReview`
  and `RecycleSuggested` rows). Each row carries enough context to act
  without a re-scan: path, file hash at proposal time, proposed action,
  category, reasoning, confidence, timestamp, profile.

CLI surface (v1 target — plain, scriptable commands; a `ratatui` interactive
triage view is an explicit fast-follow, not a v1 requirement, since it would
just be a nicer front-end over the same approve/reject logic):

```
bowerbird review list [--profile NAME] [--type review|delete]
bowerbird review show <id>
bowerbird review approve <id>
bowerbird review reject <id> [--reason "..."]
bowerbird review approve --all --profile NAME   # bulk, prints summary + confirm prompt
```

- Rejections are remembered (keyed on file-hash + proposed-category) so an
  identical proposal isn't re-surfaced on the next cron run unless the file
  itself changes.
- Resolution re-validates file hash before executing (file may have changed
  or vanished between proposal and human approval days later).
- `review_placement` config controls whether pending items stay untouched at
  their original path (`in_place`) or get physically moved to a holding
  folder (`quarantine`) for non-CLI users to browse directly.

**Delete is a separate, more heavily guarded tier, not a config toggle on the
normal confidence gate:**
- Off by default per profile (`allow_delete_suggestions = false`); must be
  explicitly enabled, and most profiles (personal docs, media libraries)
  should likely never enable it.
- `SuggestDelete` proposals **always** land in the review queue —
  unconditionally, regardless of confidence, with no config path to
  auto-execute.
- Approving a delete moves the file into a recycle store (mirroring original
  path structure, so restore is a simple reverse-move) — never calls
  `unlink()` directly.
- Permanent removal only happens via an explicit, separate command, never
  automatically inside a normal `run`:

```
bowerbird recycle list
bowerbird recycle restore <id>
bowerbird recycle purge --older-than 30d [--dry-run]
```

### 8. Notifications — deferred

A `Notifier` trait (Slack webhook, email, desktop notification as future
implementations) should be stubbed into the architecture now so exit-code-2
events and delete suggestions have an obvious hook point, but no concrete
notifier ships in v1. Logs + exit codes are sufficient for the initial
unattended-cron use case.

### 9. First LLM backend adapter: OpenAI-compatible

Targets the most common self-hosted surface (llama.cpp server, Ollama, vLLM,
etc.) as the first working integration. Anthropic-compatible adapter follows
once the core pipeline is validated end-to-end against the OpenAI-style
adapter.

### 10. License

Open source, dual-licensed **MIT OR Apache-2.0** (standard convention for
Rust projects).

## Open Questions / Deferred Decisions

These are intentionally unresolved — flagged here so a future session picks
them up deliberately rather than by default:

- **Filename template syntax** — token format, escaping rules, and whether
  templates support conditional/fallback tokens (e.g. missing `vendor`).
- **Structured output enforcement strategy** — prefer tool-calling/JSON-mode
  where the backend supports it, with a prompt-and-parse fallback for models
  that don't (common on smaller local models via llama.cpp). Needs a
  per-backend capability flag.
- **Metadata extraction crate finalization** — confirm `lofty` (audio),
  `kamadak-exif` (images), a PDF metadata crate, and an epub reader as the
  v1 set; scope which are enabled by default vs. opt-in per profile.
- **Testing/mocking strategy** — particularly for filesystem operations and
  collision/conflict scenarios; likely an in-memory or temp-dir-based harness
  for the executor and policy engine.
- **Target platform matrix** for binary releases (Linux amd64/arm64 are
  certain; macOS and Windows priority TBD) and release tooling (e.g.
  `cargo-dist`).
- **Container distribution** — base image choice (distroless/scratch),
  registry target (GHCR likely).
- **Daemon mode (`bowerbird watch`)** — deferred by design; revisit once the
  one-shot/cron path is stable.
- **GUI/TUI front-ends** — deferred; CLI is the foundation everything else
  will be built on top of.
- **Media-library conventions** (movies/shows/music/audiobooks, *arr-style
  renaming) — explicitly out of scope until core categorization/rename/
  review/recycle flows are solid.

## Consequences

**Gains:**
- Destructive or path-unsafe actions are structurally difficult to express,
  not just runtime-checked.
- Cron-first design keeps the core mental model simple (pure function of
  state) and daemon mode remains a strict, low-cost addition later.
- Per-profile locking, backend routing, and thresholds let a single tool
  serve very different trust levels (throwaway downloads vs. sensitive
  personal documents) without compromise.
- Delete is fully reversible by construction (recycle bin, journaled,
  explicit purge command) despite being a core feature rather than an
  afterthought.

**Costs / risks accepted:**
- Rust's steeper authoring cost vs. Go, accepted in exchange for the
  type-system guarantee around the action space.
- Two-table SQLite store, per-profile lockfiles, and a formal journal add
  implementation surface earlier than a minimal script would need — accepted
  because they're foundational to safety and undo, not incidental.
- Deferring the daemon, GUI, notifications, and media-library conventions
  means v1 is CLI/cron-only — accepted deliberately to avoid scope creep
  before the core pipeline is proven.
