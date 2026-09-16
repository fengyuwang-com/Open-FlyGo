#!/usr/bin/env bash
# FlyGo regression harness - full quality gates for the merged workspace.
# Usage: bash scripts/regression.sh
# Exit 0 = all gates pass; non-zero = at least one gate failed.
# Gates:
#   1. git hygiene         - no tracked secrets/config files
#   2. secrets scan        - scripts/check-secrets.sh
#   3. panel tests         - cargo test, all suites, zero failures
#   4. control-plane tests - cargo test
#   5. clippy zero         - both crates, any warning = failure
#   6. build               - cargo build both crates (panel embeds static/ via include_str!)
#   7. version file        - root Version matches YYYY.MM.DD-HHMM
#   8. winproc guard       - CREATE_NO_WINDOW defined only in winproc.rs

set -u
cd "$(dirname "$0")/.."
LOG=/tmp/flygo-regression.log
: > "$LOG"

PASS=0
FAIL=0
RESULTS=""

gate() { # gate <name> <bash-cmd>
    local name="$1" cmd="$2"
    echo "==== GATE: $name ===="
    if ( eval "$cmd" ) >> "$LOG" 2>&1; then
        RESULTS="${RESULTS}PASS  $name"$'\n'
        PASS=$((PASS + 1))
        echo "  -> PASS"
    else
        RESULTS="${RESULTS}FAIL  $name"$'\n'
        FAIL=$((FAIL + 1))
        echo "  -> FAIL (tail of log:)"
        tail -15 "$LOG" | sed 's/^/    /'
    fi
}

# test totals helper appended to cargo test gates
sum_tests() { # file - ok if no FAILED suite and no compile error
    grep -q "test result: FAILED" "$1" && return 1
    grep -qE "^error(\[|:)" "$1" && return 1
    echo "  tests total: $(grep -o 'test result: ok\. [0-9]* passed' "$1" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')"
}

gate "git hygiene (no tracked secrets)" \
  '! git ls-files | grep -E "(^|/)(term_token\.env|alist_pass\.txt|docker-compose\.yml\.bak)$|\.env$"'

gate "check-secrets" "bash scripts/check-secrets.sh"

gate "panel cargo test" \
  'cd panel && cargo test > /tmp/reg_panel.log 2>&1 && sum_tests /tmp/reg_panel.log'

gate "control-plane cargo test" \
  'cd control-plane && cargo test > /tmp/reg_cp.log 2>&1 && sum_tests /tmp/reg_cp.log'

gate "panel clippy zero warnings" \
  'cd panel && cargo clippy --all-targets -- -D warnings > /dev/null 2>&1'

gate "control-plane clippy zero warnings" \
  'cd control-plane && cargo clippy --all-targets -- -D warnings > /dev/null 2>&1'

gate "panel cargo build" \
  'cd panel && cargo build > /dev/null 2>&1'

gate "control-plane cargo build" \
  'cd control-plane && cargo build > /dev/null 2>&1'

gate "Version file format" 'grep -Eq "^[0-9]{4}\.[0-9]{2}\.[0-9]{2}-[0-9]{4}$" Version'

gate "winproc CREATE_NO_WINDOW single source" \
  '[ "$(grep -rliE "0x0800_0000|0x08000000" panel/src --include="*.rs" | wc -l)" -eq 1 ] && grep -q "0x0800_0000" panel/src/winproc.rs'

echo ""
echo "================ REGRESSION SUMMARY ================"
printf '%s' "$RESULTS"
echo "----------------------------------------------------"
echo "PASS: $PASS  FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then echo "ALL GATES GREEN"; else echo "REGRESSION DETECTED (full log: $LOG)"; fi
exit "$FAIL"
