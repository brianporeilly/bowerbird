# Bowerbird development tasks. Run `just` for the full local gate.

set shell := ["bash", "-uc"]

# Everything CI runs, in the order that fails fastest.
default: fmt-check lint test

# Format.
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Lints, as errors. The workspace lint table is the source of truth.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Tests.
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Coverage report; opens nothing, prints a summary.
cov:
    cargo llvm-cov nextest --workspace --summary-only

cov-html:
    cargo llvm-cov nextest --workspace --html
    @echo "report: target/llvm-cov/html/index.html"

# Are the tests actually testing anything? Slow; run before a release.
mutants:
    cargo mutants --workspace --in-place

# Supply chain: advisories, licenses, sources.
deny:
    cargo deny check

# Build the static binary that gets shipped.
release-static:
    cargo build --release --target x86_64-unknown-linux-musl
    @ls -lh target/x86_64-unknown-linux-musl/release/bower

# Check the MSRV declared in Cargo.toml still builds.
msrv:
    cargo +1.97 check --workspace

# Dry-run the example config against a throwaway directory tree.
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' EXIT
    mkdir -p "$dir/downloads"
    printf 'invoice\n'  > "$dir/downloads/acme-invoice.pdf"
    printf 'png\n'      > "$dir/downloads/holiday.png"
    printf 'archive\n'  > "$dir/downloads/backup.zip"
    printf 'partial\n'  > "$dir/downloads/big.part"
    # Also zero stability_wait_minutes: the files were created a second ago and
    # would otherwise all be "still settling", which makes for a poor demo.
    sed "s|/data/downloads|$dir/downloads|; s|/var/lib/bowerbird|$dir|; \
         s|/data/_review|$dir/_review|; s|^stability_wait_minutes = .*|stability_wait_minutes = 0|" \
        bowerbird.example.toml > "$dir/config.toml"
    cargo run --quiet -- --config "$dir/config.toml" run --profile downloads --stub-llm
