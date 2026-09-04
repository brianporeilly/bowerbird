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
#
# Needs a musl C cross-compiler, because rusqlite's bundled SQLite compiles C:
#   Debian/Ubuntu  apt install musl-tools
#   Fedora         dnf install musl-gcc
# cc-rs looks for `x86_64-linux-musl-gcc` before falling back, so point it at
# whatever the distro actually installed.
release-static:
    #!/usr/bin/env bash
    set -euo pipefail
    # CC_ only, for cc-rs to compile the bundled SQLite amalgamation. Do not
    # also set CARGO_TARGET_..._LINKER: that makes musl-gcc the linker driver,
    # which links against /lib/ld-musl-x86_64.so.1 and quietly costs us the
    # static binary that is the whole point of this target.
    CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}" \
        cargo build --release --target x86_64-unknown-linux-musl
    bin=target/x86_64-unknown-linux-musl/release/bower
    ls -lh "$bin"
    file "$bin"
    if readelf -l "$bin" | grep -q INTERP; then
        echo "error: binary requires a dynamic interpreter; it is not static" >&2
        exit 1
    fi

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

# --- Model-comparison lab -----------------------------------------------------
# Runs one corpus past several models and shows where they disagree. Everything
# lives in lab/, which is gitignored. See scripts/lab.sh.

# Scaffold lab/ and write lab/lab.toml.
lab-init:
    ./scripts/lab.sh init

# Copy real files into the corpus. Copies; never touches the source.
lab-corpus dir:
    ./scripts/lab.sh corpus {{dir}}

# Run one model, or --all. Needs a release binary: `just build-release`.
lab-run target="--all":
    ./scripts/lab.sh run {{target}}

# Show where the models disagreed.
lab-compare:
    ./scripts/lab.sh compare

# Wipe runs and state, keeping the corpus and config.
lab-reset:
    ./scripts/lab.sh reset

build-release:
    cargo build --release
