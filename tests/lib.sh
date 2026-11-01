#!/bin/bash
# Shared helpers of the test scripts: start/stop the fake Orthanc and the portal, tiny assertions.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TESTS="$ROOT/tests"
PORTAL_BIN="${PORTAL_BIN:-$ROOT/target/debug/orthanc-portal}"
export MOCK_LOG_DIR="${MOCK_LOG_DIR:-/tmp/portal-tests}"
WORK="$MOCK_LOG_DIR"
F=http://127.0.0.1:18080     # intranet listener (full)
R=http://127.0.0.1:18081     # remote listener (view only)
H='X-Requested-With: portal'; J='Content-Type: application/json'
SID=aaaaaaaa-bbbbbbbb-cccccccc-dddddddd-eeeeeeee
SER=11111111-22222222-33333333-44444444-55555555
SR=99999999-88888888-77777777-66666666-55555555
PASS=0; FAIL=0

# NOTE: the portal is matched by its exact process name (-x), not with -f: the project folder is
# also called "orthanc-portal", so `pkill -f` would kill the test script itself.
stack_stop() { pkill -f "mock_orthanc.py" 2>/dev/null; pkill -x "$(basename "$PORTAL_BIN")" 2>/dev/null; sleep 0.8; }
mock_start() { python3 "$TESTS/mock_orthanc.py" > "$WORK/mock.out" 2>&1 & sleep 0.6; }
# portal_start [ENV=value ...]  (first start: environment values seed the settings)
portal_start() {
  env ORTHANC_URL=http://127.0.0.1:18042 ORTHANC_USERNAME=orthanc ORTHANC_PASSWORD=s3cr3tpw DATABASE_PATH="$WORK/portal.db" \
      BIND_ADDR=127.0.0.1:18080 REMOTE_BIND_ADDR=127.0.0.1:18081 ADMIN_PASSWORD=SuperSecret123 "$@" "$PORTAL_BIN" >> "$WORK/portal.log" 2>&1 &
  sleep 2.5
}
stack_fresh() { stack_stop; rm -rf "$WORK"; mkdir -p "$WORK"; mock_start; portal_start "$@"; }

ok()    { if [ "$2" == "$3" ]; then PASS=$((PASS+1)); echo "PASS  $1"; else FAIL=$((FAIL+1)); echo "FAIL  $1  (got: $2 | expected: $3)"; fi; }
has()   { if echo "$2" | grep -qi -- "$3"; then PASS=$((PASS+1)); echo "PASS  $1"; else FAIL=$((FAIL+1)); echo "FAIL  $1  (missing: $3 in: ${2:0:240})"; fi; }
hasnt() { if echo "$2" | grep -qi -- "$3"; then FAIL=$((FAIL+1)); echo "FAIL  $1  (found: $3)"; else PASS=$((PASS+1)); echo "PASS  $1"; fi; }
py()    { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
# login BASE USER PASS [extra curl args]  -> the session cookie value
login() { local base=$1 u=$2 p=$3; shift 3; curl -s -D - -o /dev/null "$@" -H "$H" -H "$J" -d "{\"username\":\"$u\",\"password\":\"$p\"}" "$base/api/login" | grep -i set-cookie | sed 's/.*portal_session=\([0-9a-f]*\);.*/\1/'; }
# code BASE COOKIE METHOD PATH [curl args]  -> HTTP status
code()  { local base=$1 cookie=$2 method=$3 path=$4; shift 4; curl -s -o /dev/null -w "%{http_code}" --max-time 8 -X "$method" -H "Cookie: portal_session=$cookie" -H "$H" "$@" "$base$path"; }
body()  { local base=$1 cookie=$2 path=$3; shift 3; curl -s --max-time 8 -H "Cookie: portal_session=$cookie" -H "$H" "$@" "$base$path"; }
put()   { curl -s --max-time 10 -w "\n%{http_code}" -X PUT -H "Cookie: portal_session=$1" -H "$H" -H "$J" -d "$2" "$F/api/admin/settings"; }
