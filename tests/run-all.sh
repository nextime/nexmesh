#!/usr/bin/env bash
# Run every end-to-end suite.
#
# A suite that prints no verdict is a FAILURE, not a pass. One of them once
# died on a traceback, printed nothing the grep matched, and a check for the
# word "FAILED" let it through to a commit. Silence is not success.
set -uo pipefail
cd "$(dirname "$0")/.."

# Every suite drives nexmesh-mcpd, and a CLI exports its conversation id to
# its MCP children. Running the suite from inside such a CLI would give every
# shim the same identity and make them all claim one seat.
unset CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID OPENCODE_SESSION_ID \
      QWEN_SESSION_ID GEMINI_SESSION_ID NEXMESH_CLI_SESSION

# And the operator's own credentials. The ctl and the orchestrator both fall
# back to a PAIRED IDENTITY in ~/.nexmesh, so on the machine where the fleet
# is actually run a suite can silently reach the REAL hub, or an
# orchestrator meant to be unauthenticated picks up a real token. Point them
# at somewhere that cannot exist.
unset NEXMESH_OPERATOR_TOKEN NEXMESH_HUB_HTTP NEXMESH_CLIENT_CERT NEXMESH_CLIENT_KEY
export NEXMESH_IDENTITY=/nonexistent/nexmesh-identity

SUITES=(team spawn attach secure driver adopt codex opencode workspace
        control dashboard orchestrator mtls pairing revoke bind plugin mcpdoor
        capability memory codegraph coderai webauth reconnect ptyinject termlink)
TIMEOUT=${TIMEOUT:-420}
failed=()

# Unit tests first, and a build failure in them counts as a failure.
#
# They used to be run separately, by hand. A test helper that did not compile
# then went unnoticed: the e2e suites all passed, because they exercise the
# binaries rather than the test build, and `cargo test` output was being
# grepped for results that were never produced. Silence is not success here
# either.
if [ "${SKIP_UNIT:-0}" != "1" ]; then
    printf '%-14s ' "unit"
    unit=$(cargo test --workspace 2>&1)
    if [ $? -ne 0 ] || ! printf '%s\n' "$unit" | grep -q "^test result: ok"; then
        echo "FAILED -- last lines:"
        printf '%s\n' "$unit" | grep -E "^error|panicked at|test result: FAILED" | tail -6 \
            | sed 's/^/                 /'
        failed+=(unit)
    else
        total=$(printf '%s\n' "$unit" | grep "^test result: ok" \
                | awk '{ for (i = 1; i <= NF; i++) if ($i == "passed;") s += $(i-1) } END { print s }')
        echo "$total unit tests passed"
    fi
fi

# The browser terminal's parser, in node. Not a python suite: it drives the
# same term.js the dashboard serves, with a stubbed DOM, because a VT state
# machine is exactly the kind of code where "it looked right" is not evidence.
printf '%-14s ' "termjs"
if ! command -v node >/dev/null 2>&1; then
    echo "SKIP  node is not installed"
else
    tout=$(timeout 60 node tests/term_test.js 2>&1)
    tverdict=$(printf '%s\n' "$tout" | grep -E "ALL .* PASSED|^FAILED" | head -1)
    if [[ "$tverdict" == ALL* ]]; then
        echo "$tverdict"
    else
        echo "FAILED -- last lines:"
        printf '%s\n' "$tout" | grep -E "FAIL|Error" | tail -5 | sed 's/^/                 /'
        failed+=(termjs)
    fi
fi

for t in "${SUITES[@]}"; do
    printf '%-14s ' "$t"
    out=$(timeout "$TIMEOUT" python3 -u -I "tests/e2e_$t.py" 2>&1)
    rc=$?
    verdict=$(printf '%s\n' "$out" | grep -E "ALL .* PASSED|FAILED \(" | head -1)
    if [ $rc -eq 0 ] && [[ "$verdict" == ALL* ]]; then
        echo "$verdict"
    else
        failed+=("$t")
        if [ -z "$verdict" ]; then
            echo "NO VERDICT (exit $rc) -- last lines:"
            printf '%s\n' "$out" | tail -6 | sed 's/^/                 /'
        else
            echo "$verdict"
        fi
    fi
done

echo
if [ ${#failed[@]} -eq 0 ]; then
    echo "all ${#SUITES[@]} suites passed"
else
    echo "FAILED: ${failed[*]}"
    exit 1
fi
