#!/usr/bin/env bash
# The gate: formatting, lints and tests, in that order.
#
#   scripts/check.sh           # everything, including the GPUI app
#   scripts/check.sh --fast    # skip swarmbot-ui (its build is minutes long)
#
# Every stage prints one line with its duration, and the script stops at the
# first failure: a lint error is cheaper to fix before the tests have run.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

FAST=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        -h | --help)
            sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg (try --fast)" >&2
            exit 2
            ;;
    esac
done

# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh" >/dev/null

# `--exclude` only applies to workspace selections, so it rides along with
# `--workspace` on clippy and test. fmt has no package selection to narrow.
EXCLUDE=()
if [ "$FAST" -eq 1 ]; then
    EXCLUDE=(--exclude swarmbot-ui)
fi

stage() {
    local name="$1"
    shift
    local start=$SECONDS
    if "$@"; then
        printf 'ok    %-28s %3ds\n' "$name" "$((SECONDS - start))"
    else
        printf 'FAIL  %-28s %3ds\n' "$name" "$((SECONDS - start))" >&2
        exit 1
    fi
}

printf 'check %s\n' "$([ "$FAST" -eq 1 ] && echo '(fast: swarmbot-ui excluded)' || echo '(full workspace)')"

stage "fmt" cargo fmt --all -- --check
stage "clippy" cargo clippy --workspace "${EXCLUDE[@]}" --all-targets -- -D warnings
stage "test" cargo test --workspace "${EXCLUDE[@]}"

printf 'done  %ss total\n' "$SECONDS"
