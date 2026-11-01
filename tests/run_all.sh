#!/bin/bash
# Runs every automated test: server side, then browser side.   Usage: tests/run_all.sh
cd "$(dirname "$0")" || exit 1
./server_tests.sh | grep -E "FAIL|RESULT"; s1=${PIPESTATUS[0]}
./ui_tests.sh | grep -E "FAIL|RESULT|HARNESS"; s2=${PIPESTATUS[0]}
[ "$s1" -eq 0 ] && [ "$s2" -eq 0 ] && echo "ALL TESTS PASSED" || { echo "SOME TESTS FAILED"; exit 1; }
