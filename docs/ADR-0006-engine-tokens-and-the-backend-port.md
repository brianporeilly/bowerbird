# ADR-0006: Engine-filled filename tokens, and what a backend receives

**Status:** Accepted
**Date:** 2026-09-03
**Relates to:** [ADR-0001](ADR-0001-bowerbird-architecture.md) §2 §3 §4,
[ADR-0004](ADR-0004-structured-output-and-the-openai-adapter.md) §3,
[ROADMAP](ROADMAP.md)

## Context

Two changes, made together because the first is what makes the second clean.

### `{date}` could only ever have been a hallucination

ADR-0001's worked example of a filename template is
`"{date}-{doc_type}-{vendor}{ext}"`, and the shipped example config uses it
verbatim. Only `{ext}` was ever bound to the file; every other token had to come
from the model's `name_tokens`.

But [ADR-0004](ADR-0004-structured-output-and-the-openai-adapter.md) §3 settled
that the context builder discloses no timestamp — only name, relative
directory, extension, size, MIME, and a content excerpt. So the model was being
asked for a date it had never been told.

That leaves two outcomes, and both are bad:

- The model declines, `render` returns `MissingToken`, and the file goes to
  manual review. Every renaming profile, every file, every run.
- The model complies by inventing one. A fabricated date lands in a filename,
  where it is indistinguishable from a true one and outlives every other record
  of how it got there.

The second is worse, and it is the one a capable model actually does. The stub
backend concealed the whole problem, because it reached into
`FileRecord.facts.mtime` directly — data no real backend can see.

### A backend was handed the domain it should not know

`LlmBackend::classify` took a `BatchRequest`, which carries `&Profile` and
`&[FileRecord]`. The adapter's first act was to call `context::build` and
discard both. So the crate that should only know how to talk to models could
reach a file's absolute path, and could consult a policy setting the context
builder had deliberately chosen not to disclose.

## The decision

### 1. `{date}` is an engine token

`{ext}` and `{date}` are filled by the policy engine from the file itself.
`template_tokens` no longer names them, so the model is not asked; `render`
takes the date as an argument and ignores any `date` the model supplied anyway.

`FileFacts::modified_date` produces `YYYY-MM-DD` in UTC. The civil-date
arithmetic moved out of the stub backend into `bower-core`, which is where it
belonged once the engine became the thing that needs it.

A date is a fact the scanner already holds. Asking a model to guess it, when
the answer is sitting in a `stat` result, is not a use of a model.

### 2. `classify` takes a `BatchContext`

The trait now receives what the core has already decided the model may see.
`context::build` runs on the core side of the port, in `run::run_profile`.

A backend therefore *cannot* widen disclosure. Not "should not" — the type does
not carry the data. That is the same argument `DestPath` makes about
containment: possessing the value is the proof.

`BatchRequest` survives as the input to the context builder, which is what it
always was.

## Consequences

`bower-llm` no longer references `Profile` or `FileRecord`. It still depends on
`bower-config` for `Backend`, `Provider`, and `StructuredOutput` — a backend's
own configuration, which is legitimately its business. The roadmap item asked to
decouple the crate from the *filesystem domain*, not from its own settings.

The stub no longer proposes a `date` token, and its token vocabulary is now
fixed at `name`, `vendor`, `doc_type` rather than derived from the template. A
template asking for anything else gets a `MissingToken` and the file goes to
review — which is exactly what a real model declining a token does, and worth
exercising rather than papering over.

`{date}` now always means the file's mtime, never a date read out of the
document. For an invoice, the document's own date is often what a person wants.
That is a real limitation, and the honest place to solve it is a distinct
model-proposed token, which nothing yet needs. Recorded as open in the
[ROADMAP](ROADMAP.md).

A profile that was already renaming will now produce different names: the
engine's date replaces whatever the model was proposing. There is no migration,
because until this change a renaming profile against a real backend was either
filing nothing or filing under invented dates. Nothing correct is being broken.
