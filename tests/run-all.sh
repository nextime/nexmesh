#!/usr/bin/env bash
# Run every end-to-end suite.
#
# A suite that prints no verdict is a FAILURE, not a pass. One of them once
# died on a traceback, printed nothing the grep matched, and a check for the
# word "FAILED" let it through to a commit. Silence is not success.
set -uo pipefail
cd "$(dirname "$0")/.."

SUITES=(team spawn attach secure driver adopt codex opencode workspace
        control dashboard orchestrator mtls pairing)
TIMEOUT=${TIMEOUT:-420}
failed=()

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
