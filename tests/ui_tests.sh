#!/bin/bash
# Browser-side tests (needs node + `npm install` once in this folder).
source "$(dirname "$0")/lib.sh"
stack_fresh
export NODE_PATH="${NODE_PATH:-$TESTS/node_modules}"
timeout 240 node "$TESTS/ui_tests.js"; status=$?
stack_stop
exit $status
