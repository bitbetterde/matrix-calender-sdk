#!/usr/bin/env bash
# Runs the end-to-end tests (tests/e2e.rs) against a live homeserver.
#
# Usage: scripts/e2e.sh [test name filter] [libtest options, e.g. --test-threads=1]
#
# Environment:
#   MATRIX_E2E_HOMESERVER       homeserver base URL (default: http://matrix.ess.localhost)
#   MATRIX_E2E_PASSWORD_PREFIX  password prefix; the password of user X is prefix + X
#                               (default: calendar-e2e-)
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

export MATRIX_E2E_HOMESERVER="${MATRIX_E2E_HOMESERVER:-http://matrix.ess.localhost}"
export MATRIX_E2E_PASSWORD_PREFIX="${MATRIX_E2E_PASSWORD_PREFIX:-calendar-e2e-}"

if ! command -v cargo >/dev/null 2>&1 && [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
fi

if command -v curl >/dev/null 2>&1; then
    if ! curl -fsS --max-time 10 "$MATRIX_E2E_HOMESERVER/_matrix/client/versions" >/dev/null; then
        echo "error: no Matrix homeserver reachable at $MATRIX_E2E_HOMESERVER" >&2
        exit 1
    fi
fi

echo "Running end-to-end tests against $MATRIX_E2E_HOMESERVER"
exec cargo test --test e2e -- --nocapture "$@"
