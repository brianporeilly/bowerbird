# ADR-0004: Structured output enforcement, and the OpenAI-compatible adapter

**Status:** Accepted
**Date:** 2026-08-30
**Closes:** ADR-0001 open question, "Structured output enforcement strategy"
**Amends:** [ADR-0001](ADR-0001-bowerbird-architecture.md) §2, §3, §4

## Context

ADR-0001 §9 made the OpenAI-compatible adapter the first real backend, and
left structured output as a named open question: prefer tool-calling or JSON
mode where the backend supports it, with a prompt-and-parse fallback for models
that do not, gated by a per-backend capability flag. Implementing that surfaced
three further points ADR-0001 either left ambiguous or did not anticipate.

## 1. Structured output: three modes, defaulting to the weakest

`[[llm_backends]]` gains `structured_output`:

| Mode | Sends | Where it works |
| ---- | ----- | -------------- |
| `prompt` (default) | nothing extra | anything speaking `/chat/completions` |
| `json_object` | `response_format: {"type": "json_object"}` | recent llama.cpp, Ollama, vLLM, OpenAI |
| `json_schema` | `response_format` with a strict schema | vLLM, recent llama.cpp, OpenAI |

Per backend rather than global, because a single install routinely routes one
profile to a cloud API and another to a local server, and those do not have the
same capabilities.

**The default is deliberately the weakest option.** An endpoint that does not
recognise `response_format` generally rejects the whole request rather than
ignoring the field, so a stronger default would mean failing on first contact
with exactly the self-hosted servers ADR-0001 §9 names as the primary target.
Opting up is a one-line change once you know what your server supports;
diagnosing a 400 produced by a default nobody chose is a much worse first
experience. The prompt-and-parse path is correspondingly forgiving: it recovers
JSON from code fences and from surrounding prose.

Automatic probing — try `json_schema`, fall back on a 400 that looks like an
unsupported-parameter error — was considered and deferred. It costs a round
trip on first use, and its failure mode is a silent downgrade that is hard to
notice. An explicit flag says what is happening.

## 2. ADR-0001 conflates two different retries

§4 stage 1 says a malformed response gets "one retry with error fed back to the
model". §5 gives each backend a `max_retries`. ADR-0001 never distinguishes
them, and they are not the same mechanism:

- **Transport retries** (`max_retries`) cover a request that never received a
  real answer: connection failure, timeout, 429, 5xx. The *same* request is
  sent again.
- **The reformat retry** covers a request that was answered with something
  unusable. It is a *different* request — the bad reply and a description of
  what was wrong are appended to the conversation — and there is exactly one.

Implemented as separate budgets. Each HTTP call, including the reformat's, gets
its own transport allowance; there is never a second reformat. Conflating them
would let a model that reliably emits bad JSON consume the entire transport
budget producing bad JSON, which is not what `max_retries` is for.

A 4xx other than 429 is fatal rather than retried: repeating a request the
server has rejected will not improve it.

## 3. ADR-0001 §3 does not say what happens to an unrecognised `file_id`

§3 says validation is per item so one malformed entry does not sink a batch,
but not how to treat an entry naming a file that was never sent. Decided:
**discarded, not trusted.** An id we did not send is either a hallucination or
an attempt to reach a file this batch is not about, and neither is worth
acting on. The count is logged.

Two further per-item rules follow from "the engine may only lower trust":

- **Two entries for one file** is a contradiction, not a preference. Both are
  discarded and the file goes to review.
- **A missing `action` tag defaults to `categorize`.** Models routinely emit
  the categorization shape without the discriminant. This is safe in exactly
  one direction: the destructive variant must be named explicitly, so an
  untagged entry can never become a deletion suggestion.

A file the model never mentioned triggers the reformat retry, then reaches
review. ADR-0001 did not say whether silence warrants a retry; it does, because
one extra request is cheap next to a human's attention.

## 4. The context builder had no implementation, and no stated rules

ADR-0001 §2 names a context builder as the second pipeline layer, but nothing
implemented it — the stub backend ignored the profile's metadata toggles
entirely. It now exists in `bower-core`, and produces a data structure rather
than a prompt string: what a model is allowed to see is a policy question and
belongs in the core, while how it is framed on the wire is the adapter's
business.

ADR-0001 §3 says the model never *emits* a path. It does not say whether the
model may *see* one. It may not: only the file name and its directory relative
to the scan root are disclosed, and a test asserts neither the scan root nor
the destination root appears in the serialized context. Keeping absolute paths
out of the conversation entirely means there is no path for a compromised model
to echo back.

### Prompt injection

`content_sniff_bytes` puts a file's own bytes into the prompt, which makes every
scanned file a potential injection vector. Three things happen, in increasing
order of how much they matter:

1. The excerpt is sanitized — control characters stripped, chat-template
   sentinels split so they stop being structural, blank-line padding collapsed.
   The text itself is kept, because it is the evidence being classified.
2. The system prompt frames excerpts as material to classify rather than
   instructions to follow, and says that an excerpt which appears to address
   the model is itself evidence about the file.
3. **The policy engine is the actual protection.** Neither of the above is
   relied on. `tests/injection.rs` grants the attacker a hostile file *and* a
   model that complies with it completely at confidence 1.0, then asserts the
   pipeline holds anyway: traversal and absolute categories are refused, a
   category outside a closed taxonomy is refused, a forbidden deletion does not
   touch the file, hostile filename tokens cannot add a path component, and
   nothing is ever created outside the two roots.

Points 1 and 2 reduce how easy the attempt is. Point 3 is why it does not
matter much either way.

## 5. Transport: `ureq`

Blocking, which matches the sync core — the pipeline is sequential per profile,
so no async runtime needs to enter the tree. It defaults to rustls with
`native-tls` off, so there is no `openssl-sys` and nothing requiring a
cross-compiled OpenSSL for the static musl target.

One `cargo deny` allow-list addition was needed: `webpki-roots` ships the
Mozilla CA set under `CDLA-Permissive-2.0`, a permissive licence for *data*
with no copyleft. Bundling the roots rather than reading the host trust store
is what lets the static binary do TLS on a distroless image that has no CA
store, which ADR-0001 §1 asks for.

**The static musl build is unverified for this change.** It adds `ring`, which
compiles C and assembly, to a target that already compiles C for SQLite. See
ADR-0003 §8 for how that build has gone wrong before while still reporting
success.

## Consequences

- The prompt-and-parse default works everywhere and is the least reliable
  option. Users who know their server should raise it; the reformat retry
  absorbs the difference at the cost of one extra request per bad batch.
- The filename template syntax remains provisional. The context builder now
  tells the model which tokens a template wants, which is a better answer than
  the stub's guessing, but ADR-0001's open question about fallback and
  conditional tokens still stands.
- The Anthropic-compatible adapter remains unimplemented, as ADR-0001 §9
  intended: it follows once the core pipeline is proven against this one.
