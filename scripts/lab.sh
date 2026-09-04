#!/usr/bin/env bash
#
# Bowerbird model-comparison lab.
#
# Runs the same corpus past several models and shows where they disagree.
# Everything lives under lab/, which is gitignored: your files and your results
# are never staged.
#
# The safety rule this script enforces: **nothing ever runs against your
# originals.** `corpus` copies files in, and each run works on a fresh copy of
# the corpus. Bowerbird moves files, so a run is destructive to its own inbox by
# design -- which is fine when the inbox is a copy and is not fine otherwise.
#
#   ./scripts/lab.sh init                  scaffold lab/ and write lab/lab.toml
#   ./scripts/lab.sh corpus ~/Documents    copy real files in (never moves them)
#   ./scripts/lab.sh run qwen3b            one model
#   ./scripts/lab.sh run --all             every enabled profile, in sequence
#   ./scripts/lab.sh compare               where the models disagree
#   ./scripts/lab.sh reset                 wipe runs and state, keep the corpus

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LAB="${BOWER_LAB:-$REPO/lab}"
CONFIG="$LAB/lab.toml"
BIN="${BOWER_BIN:-$REPO/target/release/bower}"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*" >&2; }

need_bin() {
    [ -x "$BIN" ] || die "no bower binary at $BIN. Run: cargo build --release"
}

need_config() {
    [ -f "$CONFIG" ] || die "no $CONFIG. Run: ./scripts/lab.sh init"
}

# Profile names, which are also the run-directory names.
#
# Parses the [[profiles]] blocks rather than matching names that also appear as
# a backend. A profile is often named after its model, but not when you are
# varying a *setting* -- two profiles on one backend with different
# content_sniff_bytes is the whole point -- and those names appear once.
profiles() {
    need_config
    awk '
        /^[[:space:]]*\[\[profiles\]\]/ { in_profile = 1; next }
        /^[[:space:]]*\[\[/              { in_profile = 0 }
        in_profile && /^[[:space:]]*name[[:space:]]*=/ {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                in_profile = 0
            }
        }
    ' "$CONFIG"
}

cmd_init() {
    local force=""
    [ "${1:-}" = "--force" ] && force=1

    mkdir -p "$LAB/corpus" "$LAB/runs" "$LAB/locks"

    if [ -f "$CONFIG" ] && [ -z "$force" ]; then
        note "$CONFIG already exists; leaving it alone (--force to regenerate)"
    else
        local categories description
        categories='["Invoices", "Reports", "Correspondence", "Manuals", "Photos", "Archives", "Installers"]'
        description="Mixed personal document inbox."
        sed -e "s|__LAB__|$LAB|g" \
            -e "s|__CATEGORIES__|$categories|g" \
            -e "s|__DESCRIPTION__|$description|g" \
            "$REPO/scripts/lab.template.toml" > "$CONFIG"
        note "wrote $CONFIG"
    fi

    cat >&2 <<EOF

lab is at $LAB

next:
  1. edit $CONFIG -- categories, and drop any backend you cannot reach
  2. ./scripts/lab.sh corpus <a directory of real files>
  3. ./scripts/lab.sh run --all
  4. ./scripts/lab.sh compare

Nothing here is committed: lab/ is gitignored.
EOF
}

# Copies files into the corpus. Never moves, never touches the source.
cmd_corpus() {
    local src="${1:-}"
    [ -n "$src" ] || die "usage: lab.sh corpus <directory>"
    [ -d "$src" ] || die "not a directory: $src"
    mkdir -p "$LAB/corpus"

    local limit="${LAB_CORPUS_LIMIT:-200}"
    local n=0
    while IFS= read -r -d '' f; do
        cp -n -- "$f" "$LAB/corpus/" 2>/dev/null || true
        n=$((n + 1))
        [ "$n" -ge "$limit" ] && break
    done < <(find "$src" -maxdepth 1 -type f -print0)

    note "copied up to $n file(s) into $LAB/corpus ($(ls -1 "$LAB/corpus" | wc -l) total)"
    note "source directory untouched; raise LAB_CORPUS_LIMIT to take more"
}

# Resets one model's inbox from the corpus and runs that profile.
run_one() {
    local model="$1"
    local inbox="$LAB/runs/$model/inbox"

    [ -n "$(ls -A "$LAB/corpus" 2>/dev/null || true)" ] \
        || die "corpus is empty. Run: ./scripts/lab.sh corpus <directory>"

    rm -rf "$LAB/runs/$model"
    mkdir -p "$inbox" "$LAB/runs/$model/organized"
    cp -a "$LAB/corpus/." "$inbox/"

    note "--- $model: $(ls -1 "$inbox" | wc -l) file(s) ---"
    local started
    started=$(date +%s)
    # Bowerbird's own exit code 2 means "items need a human", which is a
    # perfectly good outcome for a lab run, so it is not an error here.
    local rc=0
    "$BIN" --config "$CONFIG" run --profile "$model" --execute || rc=$?
    note "    $(( $(date +%s) - started ))s"
    # 2 is "items need a human", which is a fine outcome for a lab run.
    [ "$rc" -eq 0 ] || [ "$rc" -eq 2 ]
}

cmd_run() {
    need_bin; need_config
    local target="${1:-}"
    [ -n "$target" ] || die "usage: lab.sh run <model|--all>"

    if [ "$target" = "--all" ]; then
        local any="" failed=()
        while read -r m; do
            [ -n "$m" ] || continue
            any=1
            # One unreachable backend must not cost you the whole comparison:
            # a missing API key or a model that timed out is the common case,
            # and the other models' results are still worth having.
            if ! run_one "$m"; then
                failed+=("$m")
                note "    FAILED -- continuing with the rest"
            fi
        done < <(profiles)
        [ -n "$any" ] || die "no profiles found in $CONFIG"

        if [ ${#failed[@]} -gt 0 ]; then
            note ""
            note "${#failed[@]} model(s) failed: ${failed[*]}"
            note "compare still works; those columns will simply be empty."
        fi
    else
        run_one "$target"
    fi

    note ""
    note "done. ./scripts/lab.sh compare"
}

cmd_compare() {
    need_config
    [ -f "$LAB/state.db" ] || die "no runs yet. Run: ./scripts/lab.sh run --all"
    python3 "$REPO/scripts/lab_compare.py" "$LAB/state.db"
}

cmd_reset() {
    rm -rf "$LAB/runs" "$LAB/state.db" "$LAB/locks" "$LAB/quarantine"
    mkdir -p "$LAB/runs" "$LAB/locks"
    note "wiped runs and state; corpus and lab.toml kept"
}

case "${1:-}" in
    init)    shift; cmd_init "$@" ;;
    corpus)  shift; cmd_corpus "$@" ;;
    run)     shift; cmd_run "$@" ;;
    compare) shift; cmd_compare "$@" ;;
    reset)   shift; cmd_reset "$@" ;;
    *)
        sed -n '3,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac
