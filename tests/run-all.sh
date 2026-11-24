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

# And this seat's OWN nexmesh identity.
#
# Running the suite from inside a MANAGED seat puts NEXMESH_AGENT_ID and
# friends in the environment, every test's shim inherits them, and each one
# registers claiming to be an agent the test's own node has never heard of:
#
#   the node refused registration: agent 'ag-...' is not known to this node
#
# Ten suites failed that way at once, with the SAME agent id in every
# message -- which is the tell, because a test-local agent id cannot repeat
# across independent runs. It does not happen from an ATTACHED seat, so the
# suite passed all day and broke the moment this seat was respawned as a
# managed one. The tests set these themselves where they need them.
unset NEXMESH_AGENT_ID NEXMESH_SLOT NEXMESH_SESSION NEXMESH_CLI \
      NEXMESH_ROLE NEXMESH_NAME NEXMESH_SOCK

# And the operator's own credentials. The ctl and the orchestrator both fall
# back to a PAIRED IDENTITY in ~/.nexmesh, so on the machine where the fleet
# is actually run a suite can silently reach the REAL hub, or an
# orchestrator meant to be unauthenticated picks up a real token. Point them
# at somewhere that cannot exist.
unset NEXMESH_OPERATOR_TOKEN NEXMESH_HUB_HTTP NEXMESH_CLIENT_CERT NEXMESH_CLIENT_KEY
export NEXMESH_IDENTITY=/nonexistent/nexmesh-identity

# cargo, which is not always on the PATH that runs this.
#
# rustup here was installed with --no-modify-path, so a non-login shell, a
# cron line, or a background process started from an editor has no cargo in
# it. The unit step then exits 127 and the verdict reads "unit FAILED", which
# sends you looking for a broken test rather than a missing PATH.
if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# Ports a previous run left held.
#
# Every suite binds fixed ports in 179xx, and an interrupted run leaves its
# fake services and daemons behind. The next run's service then cannot bind,
# and the suite reports whatever check came first -- "the stand-in service is
# up", with nothing in it about a port. A six-hour-old fakecoderai on 17956
# cost exactly that.
#
# REPORTED, NOT KILLED. Matching processes by a substring of their command
# line is how a harness kills something that merely mentions the string --
# its own parent shell, in one draft of this -- and what is holding a port
# may be the fleet rather than a leftover. Whose process it is, is the
# operator's call.
if command -v ss >/dev/null 2>&1; then
    held=$(ss -ltnp 2>/dev/null | awk '$4 ~ /:179[0-9][0-9]$/ { print $4, $NF }')
    if [ -n "$held" ]; then
        echo "WARNING: these test ports are already in use, and the suites that want them"
        echo "         will fail on whichever check comes first:"
        printf '         %s\n' "$held"
        echo "         Stop them (they are from an interrupted run) and try again."
    fi
fi

# Reap what previous runs left behind.
#
# Every suite ends by printing "(logs: /tmp/nexmesh-...)" and deliberately
# leaves that directory, because the first thing anyone wants after a failure
# is the hub log. Nothing ever removed them: 2487 directories holding 3.2GB
# had accumulated here, on a filesystem at 99%, and a `cargo test` that
# cannot write its test binaries fails with an error this harness did not
# recognise -- so the verdict read FAILED with no explanation under it.
#
# Keep two days. That is long enough to still have yesterday's failure in
# front of you, and short enough that it cannot fill a disk.
#
# NEXMESH_KEEP_LOGS=1 keeps everything, for when you are bisecting.
if [ "${NEXMESH_KEEP_LOGS:-0}" != "1" ]; then
    reaped=$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'nexmesh-*' -type d -mtime +2 \
                  -print -exec rm -rf {} + 2>/dev/null | wc -l)
    [ "$reaped" -gt 0 ] && echo "(reaped $reaped test log dir(s) older than two days)"
fi

SUITES=(team spawn attach secure driver adopt codex opencode workspace
        control dashboard orchestrator mtls pairing revoke bind plugin mcpdoor
        capability memory codegraph coderai webauth reconnect ptyinject termlink
        oneshot run direct uploads xrroom xrtype prompts authority guests seatstore
        supervise seatprompt watchdog roles folds skills)
TIMEOUT=${TIMEOUT:-420}
failed=()
skipped=()

# BUILD THE BINARIES THE SUITES ACTUALLY LAUNCH, FIRST.
#
# Every e2e suite runs `target/debug/nexmesh-{hub,node,ctl,mcpd}`. Nothing
# here built them: the unit step below runs `cargo test --workspace`, which
# compiles each crate's UNIT-TEST executable into target/debug/deps and
# leaves the plain binaries alone. So the suites launched whatever happened
# to be in target/debug -- which could be, and on 24 Nov 2026 was, a build
# from three hours and several commits earlier.
#
# That makes "all 41 suites passed" mean nothing in the one case it matters
# most: straight after a change. A node-side fix was tested, reported green,
# and had never been in the binary under test; the one check that happened to
# disagree with the old behaviour is the only reason it was noticed.
#
# Failing here is a failure of the whole run. A suite cannot be trusted to
# report on code that was not compiled into what it started.
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    printf '%-14s ' "build"
    build=$(cargo build --workspace 2>&1)
    if [ $? -ne 0 ]; then
        echo "FAILED -- last lines:"
        printf '%s\n' "$build" > "${TMPDIR:-/tmp}/nexmesh-build-fail.log"
        printf '%s\n' "$build" | grep -E "^error" | tail -6 | sed 's/^/                 /'
        echo "                 (full output: ${TMPDIR:-/tmp}/nexmesh-build-fail.log)"
        echo
        echo "FAILED: build -- the suites would have tested the PREVIOUS binaries."
        exit 1
    fi
    echo "binaries up to date"
fi

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
    unit_rc=$?
    # A HERE-STRING, not a pipe.
    #
    # `printf '%s\n' "$unit" | grep -q ...` looks equivalent and is not. With
    # `set -o pipefail` (set at the top of this file) the pipeline reports the
    # LAST non-zero status in it -- and `grep -q` exits the instant it
    # matches, closing the pipe under printf, which then dies of SIGPIPE
    # (141). So a SUCCESSFUL match returned 141, `!` inverted it to true, and
    # this step reported FAILED with cargo's own exit status 0 and all 667
    # tests passing.
    #
    # It only fires once the output is big enough that printf has not
    # finished writing when grep gives up reading, which is why it failed
    # here every time and passed when the same command was run by hand.
    if [ $unit_rc -ne 0 ] || ! grep -q "^test result: ok" <<< "$unit"; then
        echo "FAILED (exit $unit_rc) -- last lines:"
        # The whole output is KEPT. A "FAILED" with nothing under it is the
        # same defect this step exists to catch: the grep below only matches
        # the shapes we have seen before, and a cargo failure that looks like
        # none of them -- a lock it could not take, a linker that ran out of
        # memory, a toolchain that moved -- printed a blank verdict and sent
        # the reader to re-run it by hand, where it passed.
        printf '%s\n' "$unit" > "${TMPDIR:-/tmp}/nexmesh-unit-fail.log"
        printf '%s\n' "$unit" | grep -E "^error|panicked at|test result: FAILED" | tail -6 \
            | sed 's/^/                 /'
        printf '%s\n' "$unit" | tail -4 | sed 's/^/                 | /'
        echo "                 (full output: ${TMPDIR:-/tmp}/nexmesh-unit-fail.log)"
        failed+=(unit)
    else
        total=$(grep "^test result: ok" <<< "$unit" \
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
    skip=$(printf '%s\n' "$out" | grep -E "^SKIP " | head -1)
    if [ $rc -eq 0 ] && [[ "$verdict" == ALL* ]]; then
        echo "$verdict"
    # A suite that needs something this machine does not have says so and is
    # not counted either way. It must still be the suite's own decision:
    # anything else that exits 0 without a verdict is a failure.
    elif [ $rc -eq 0 ] && [ -n "$skip" ] && [ -z "$verdict" ]; then
        echo "$skip"
        skipped+=("$t")
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
    if [ ${#skipped[@]} -gt 0 ]; then
        echo "$(( ${#SUITES[@]} - ${#skipped[@]} )) suites passed, skipped: ${skipped[*]}"
        exit 0
    fi
    echo "all ${#SUITES[@]} suites passed"
else
    echo "FAILED: ${failed[*]}"
    exit 1
fi
