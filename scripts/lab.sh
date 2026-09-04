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

fmt_duration() {
    local s=$1
    if [ "$s" -ge 3600 ]; then
        printf '%dh%02dm' $((s / 3600)) $(((s % 3600) / 60))
    elif [ "$s" -ge 60 ]; then
        printf '%dm%02ds' $((s / 60)) $((s % 60))
    else
        printf '%ds' "$s"
    fi
}

elapsed_since() { fmt_duration $(($(date +%s) - $1)); }

# Draws one line, rewritten in place, until `pid` exits.
#
# Progress is read from the state database rather than extrapolated from an
# assumed per-file rate: the store is in WAL mode, so polling never blocks the
# run, and a file counts once it has actually reached a terminal state. The
# estimate then comes from the rate observed *in this run*, which matters
# because the same corpus takes wildly different times on different models.
#
# Falls back to a plain elapsed clock if the count is unavailable for any
# reason -- no python3, or a schema this script does not know. Progress
# reporting must never be the thing that fails a run.
watch_progress() {
    local model="$1" total="$2" started="$3" pid="$4"
    local done=0

    # Not a terminal: no rewriting. A line every 30s, so a piped or CI run
    # leaves a readable trace instead of thousands of escape sequences.
    if [ ! -t 2 ]; then
        while kill -0 "$pid" 2>/dev/null; do
            sleep 30
            kill -0 "$pid" 2>/dev/null || break
            done=$(progress_count "$model" "$started")
            printf '    %s: %s/%s files, %s elapsed\n' \
                "$model" "$done" "$total" "$(elapsed_since "$started")" >&2
        done
        return 0
    fi

    # Work lands in bursts of `batch_size`, so the rate must be measured at the
    # moments files actually complete. Measuring against "now" between batches
    # makes the estimate climb while nothing is happening.
    local at_last_change=0
    while kill -0 "$pid" 2>/dev/null; do
        local seen
        seen=$(progress_count "$model" "$started")
        [ "$seen" -gt "$total" ] && seen="$total"
        if [ "$seen" -ne "$done" ]; then
            done="$seen"
            at_last_change=$(($(date +%s) - started))
        fi
        draw_progress "$model" "$done" "$total" "$started" "$at_last_change"
        sleep 2
    done

    # Clear the line so the run's own output starts clean.
    printf '\r\033[K' >&2
}

progress_count() {
    local model="$1" started="$2"
    python3 "$REPO/scripts/lab_compare.py" \
        --completed "$model" "$started" "$LAB/state.db" 2>/dev/null || echo 0
}

# `at_last_change` is the elapsed time when the count last moved. The estimate
# is computed from that, not from the current clock: files complete in bursts of
# `batch_size`, so dividing by a clock that keeps ticking between bursts makes
# the estimate grow while nothing is happening -- observed climbing from
# ~12m47s to ~20m21s across one batch, on a run whose first estimate was right.
draw_progress() {
    local model="$1" done="$2" total="$3" started="$4" at_last_change="$5"
    local width=24 filled=0 pct=0 elapsed remaining=""

    elapsed=$(($(date +%s) - started))
    if [ "$total" -gt 0 ]; then
        pct=$((done * 100 / total))
        filled=$((done * width / total))
    fi

    if [ "$done" -eq 0 ]; then
        # Nothing has finished yet, and on a slow model the first batch can be
        # over a minute. Say why the bar is empty rather than looking hung.
        remaining="  first batch in flight"
    elif [ "$done" -ge 2 ] && [ "$done" -lt "$total" ] && [ "$at_last_change" -gt 0 ]; then
        # At least two completions, and only from this run's own rate. One file
        # is a single sample carrying model warm-up and the whole first batch's
        # latency; extrapolating it says "1h06m left" on a ten-minute run.
        remaining="  ~$(fmt_duration $(((total - done) * at_last_change / done))) left"
    fi

    local bar="" i=0
    while [ "$i" -lt "$width" ]; do
        if [ "$i" -lt "$filled" ]; then bar="$bar#"; else bar="$bar."; fi
        i=$((i + 1))
    done

    printf '\r\033[K    %s [%s] %3d%%  %s/%s  %s%s' \
        "$model" "$bar" "$pct" "$done" "$total" "$(fmt_duration "$elapsed")" "$remaining" >&2
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

    local total
    total=$(ls -1 "$inbox" | wc -l)
    note "--- $model: $total file(s) ---"

    local started output rc=0
    started=$(date +%s)
    output=$(mktemp)

    # Run detached so a progress line can be drawn while it works. The run's own
    # output is held back until the end: interleaving it with a line being
    # rewritten in place produces a mess.
    "$BIN" --config "$CONFIG" run --profile "$model" --execute >"$output" 2>&1 &
    local pid=$!
    watch_progress "$model" "$total" "$started" "$pid"
    # Bowerbird's own exit code 2 means "items need a human", which is a
    # perfectly good outcome for a lab run, so it is not an error here.
    wait "$pid" || rc=$?

    cat "$output" >&2
    rm -f "$output"
    note "    $(elapsed_since "$started")"
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
