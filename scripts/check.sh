#!/usr/bin/env bash
# Run every gate CI runs, in CI's order, and stop at the first failure.
#
#   scripts/check.sh               # everything (what must pass before merging)
#   scripts/check.sh --fast        # fmt + clippy only (workspace + desktop)
#   scripts/check.sh --no-desktop  # skip desktop/src-tauri and the UI JS checks
#
# Works from any directory. Honours CARGO_TARGET_DIR (keep build output off the
# repo, see docs/DEVELOPMENT.md). The desktop crate is not a workspace member, so
# it builds into its own target dir: $DESKTOP_TARGET_DIR if set, else
# "$CARGO_TARGET_DIR-desktop" when CARGO_TARGET_DIR is set, else its default
# (desktop/src-tauri/target, git-ignored).
set -euo pipefail

fast=0
desktop=1
for arg in "$@"; do
    case "$arg" in
        --fast) fast=1 ;;
        --no-desktop) desktop=0 ;;
        -h | --help)
            sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "check.sh: unknown argument '$arg' (try --help)" >&2
            exit 2
            ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP="$ROOT/desktop/src-tauri"
cd "$ROOT"

if [ -t 1 ]; then
    bold=$'\e[1m' red=$'\e[31m' green=$'\e[32m' reset=$'\e[0m'
else
    bold='' red='' green='' reset=''
fi

step_no=0
# step <description> <command...> — run one gate; on failure, name it and stop.
step() {
    local desc="$1"
    shift
    step_no=$((step_no + 1))
    printf '%s==> [%d] %s%s\n' "$bold" "$step_no" "$desc" "$reset"
    if ! "$@"; then
        printf '\n%sFAILED at step %d: %s%s\n' "$red$bold" "$step_no" "$desc" "$reset" >&2
        printf '   command: %s\n' "$*" >&2
        case "$desc" in
            *fmt*) printf '   fix: run `cargo fmt --all` (workspace) or `cargo fmt` in desktop/src-tauri\n' >&2 ;;
        esac
        exit 1
    fi
}

# Run a command inside the desktop crate with its own target dir.
desktop_cargo() {
    local tgt="${DESKTOP_TARGET_DIR:-}"
    if [ -z "$tgt" ] && [ -n "${CARGO_TARGET_DIR:-}" ]; then
        tgt="${CARGO_TARGET_DIR}-desktop"
    fi
    if [ -n "$tgt" ]; then
        (cd "$DESKTOP" && CARGO_TARGET_DIR="$tgt" cargo "$@")
    else
        (cd "$DESKTOP" && env -u CARGO_TARGET_DIR cargo "$@")
    fi
}

# The CLI is run through cargo so the binary always matches the tree.
opentune() {
    cargo run -p opentune-cli --quiet -- "$@"
}

# The CI privacy scan over the data directories (VINs, e-mails, serials).
privacy_scan_data() {
    local d dirs=()
    for d in ecudefs packs ctrldefs catalog database samples android/app/src/main/assets/ecudefs; do
        [ -d "$ROOT/$d" ] && dirs+=("$d")
    done
    opentune privacy-scan "${dirs[@]}"
}

node_check_ui() {
    local f rc=0
    shopt -s nullglob
    for f in "$ROOT"/desktop/ui/src/*.js "$ROOT"/desktop/preview/*.js; do
        node --check "$f" || { echo "  syntax error in ${f#"$ROOT"/}" >&2; rc=1; }
    done
    shopt -u nullglob
    return "$rc"
}

# UDS/DoIP conformance of the simulator against python-udsoncan/doipclient
# (tools/conformance/). Needs both packages; OPENTUNE_CONFORMANCE_PYTHON picks
# the interpreter (e.g. a venv), default python3.
conformance_py() {
    local py="${OPENTUNE_CONFORMANCE_PYTHON:-python3}"
    cargo build -q -p opentune-udssim --example doip_tcp_entity || return 1
    local tgt
    tgt="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    "$py" "$ROOT/tools/conformance/udsoncan_harness.py" --entity "$tgt/debug/examples/doip_tcp_entity" 2>/dev/null
}

# SA2 seed/key VM vs the public reference implementation (tools/sa2/). Opt-in:
# runs only when OPENTUNE_SA2_PYTHON names an interpreter with sa2-seed-key.
sa2_differential() {
    cargo build -q -p opentune-sa2 --example sa2_eval || return 1
    local tgt
    tgt="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    "$OPENTUNE_SA2_PYTHON" -I "$ROOT/tools/sa2/sa2_differential.py" check \
        --fixture "$ROOT/crates/opentune-sa2/tests/fixtures/sa2_vectors.txt" \
        --eval "$tgt/debug/examples/sa2_eval" --fresh "${OPENTUNE_SA2_FRESH:-2000}"
}

# Pass test files explicitly: Node's directory/glob handling differs by version.
node_test_ui() {
    local files=()
    mapfile -d '' -t files < <(find "$ROOT/desktop/ui/tests" -type f \( -name '*.js' -o -name '*.mjs' \) -print0 | sort -z)
    if [ "${#files[@]}" -eq 0 ]; then
        echo "  (no test files)"
        return 0
    fi
    node --test "${files[@]}"
}

start=$SECONDS

# --- workspace (CI job: check) ---------------------------------------------
step "workspace fmt" cargo fmt --all -- --check
step "workspace clippy -D warnings" cargo clippy --workspace --all-targets -- -D warnings
if [ "$fast" = 0 ]; then
    step "workspace test" cargo test --workspace
    # --- data gates (CI job: ecudefs) --------------------------------------
    step "ecudefs validate" opentune ecudefs validate ecudefs --require-provenance
    step "packs validate" opentune packs validate packs
    if [ -d "$ROOT/ctrldefs" ]; then
        step "controller defs validate" opentune controller validate ctrldefs
    fi
    step "privacy scan of data dirs" privacy_scan_data
    if [ -d "$ROOT/database" ] && opentune db --help >/dev/null 2>&1; then
        # --require-access: units listed in database/units/.access-pending
        # warn, any other unit without an access profile fails.
        step "db validate" opentune db validate --require-access
    fi
    # The Android app bundles a copy of ecudefs/; it must match the root set.
    if [ -x "$ROOT/android/sync-ecudefs.sh" ]; then
        step "android ecudefs in sync" "$ROOT/android/sync-ecudefs.sh" --check
    fi
    # --- simulator conformance (CI job: conformance) ------------------------
    if "${OPENTUNE_CONFORMANCE_PYTHON:-python3}" -c 'import udsoncan, doipclient' 2>/dev/null; then
        step "udssim conformance vs python-udsoncan" conformance_py
    else
        echo "check.sh: skipping the udsoncan conformance gate (CI runs it): pip install -r tools/conformance/requirements.txt, or set OPENTUNE_CONFORMANCE_PYTHON" >&2
    fi
    if [ -n "${OPENTUNE_SA2_PYTHON:-}" ]; then
        step "sa2 VM vs reference sa2-seed-key" sa2_differential
    fi
fi

# --- desktop (CI job: desktop) ---------------------------------------------
if [ "$desktop" = 1 ]; then
    step "desktop fmt" desktop_cargo fmt -- --check
    step "desktop clippy (default features)" desktop_cargo clippy --all-targets -- -D warnings
    step "desktop clippy (--features alpha)" desktop_cargo clippy --all-targets --features alpha -- -D warnings
    if [ "$fast" = 0 ]; then
        step "desktop test (--features alpha)" desktop_cargo test --features alpha
        # --- UI JavaScript (CI job: ui) ------------------------------------
        if command -v node >/dev/null 2>&1; then
            step "node --check desktop UI + preview JS" node_check_ui
            if [ -d "$ROOT/desktop/ui/tests" ]; then
                step "node --test desktop/ui/tests" node_test_ui
            fi
        else
            echo "check.sh: node not found — UI JS checks are REQUIRED in CI; install Node.js" >&2
            exit 1
        fi
    fi
fi

mode=full
[ "$fast" = 1 ] && mode=fast
[ "$desktop" = 0 ] && mode="$mode, no desktop"
printf '\n%sAll %d gates passed (%s) in %ds.%s\n' "$green$bold" "$step_no" "$mode" "$((SECONDS - start))" "$reset"
