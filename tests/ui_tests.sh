#!/bin/bash
# Browser-side tests (needs node + `npm install` once in this folder).
source "$(dirname "$0")/lib.sh"
stack_fresh
# A second fake Orthanc, for the checks with several servers (the same studies as the first).
mkdir -p "$WORK/m2"
MOCK_LOG_DIR="$WORK/m2" "$PYTHON3" "$TESTS/mock_orthanc.py" 18043 > "$WORK/mock2.out" 2>&1 & remember mock2 $!
export NODE_PATH="${NODE_PATH:-$TESTS/node_modules}"
timeout 240 node "$TESTS/ui_tests.js"; status=$?
stop_pid mock2
stack_stop
exit $status
