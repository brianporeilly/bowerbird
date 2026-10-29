# Task: review-the-repo-propose-documentatio-094a

## Goal

Review the repo, propose documentation updates and what should be tested or built out on the roadmap next.

## Constraints

_(none)_

## Done

- Reviewed repo end to end: README (full), ROADMAP, all 6 ADRs, bowerbird.example.toml, CI (github/workflows), justfile, Cargo workspace.
- Reviewed all crates: bower-config (schema/validate), bower-core (scanner, pure policy engine, executor, SQLite state store, locking), bower-llm (openai stub + real adapter), bower-cli.
- Verified the safety claims hold in code: pure policy (policy_purity.rs), contained deletion only (deletion_is_contained.rs), injection-resistance (bower-llm/tests/injection.rs), no-overwrite via O_EXCL/hard-link (exec.rs), closed action space (ResolvedAction).
- Confirmed test + CI breadth: unit + integration across every crate; CI runs fmt, clippy, tests on ubuntu+macOS, MSRV 1.97, static musl build, cargo-deny, and a weekly audit.
- Fixed the one factual doc error: README status banner said "No real LLM backend adapter has shipped" but the OpenAI-compatible adapter is implemented in crates/bower-llm/src/openai (~1000 lines, tested) and is listed as shipped in the same README's Roadmap section. Banner corrected to "OpenAI-compatible adapter has shipped; Anthropic-compatible is next."

## Decisions

- README banner is the only factual error found; everything else (README, ROADMAP, ADRs, config comments) is coherent and accurate.
- Anthropic: provider is declared in bower-config and documented end-to-end, but the factory in bower-llm/src/lib.rs returns BuildError::NotImplemented — consistent with ROADMAP ("Anthropic-compatible adapter | Next"), so a roadmap item, not a bug. Left as-is.
- Corrected my own draft claim: `bower journal --limit N` IS implemented (default 20, crates/bower-cli/src/cli.rs:165, consumed in journal.rs). No doc drift there; do not assert it.

## Open questions

- Anthropic adapter: implement it, or mark the `anthropic-cloud` block in bowerbird.example.toml as "adapter not yet served" until it lands?
- scripts/lab.sh (model-comparison lab) is gitignored and not in CI — should it run in CI to continuously validate the confidence-gate claim and filename-template behavior?
- Filename template syntax ({doc_date} model-proposed token; conditional/fallback tokens) — design now or defer?

## Next step

Push the corrected branch (README banner fix) to origin so PR #8 reflects the real change; await maintainer review.
