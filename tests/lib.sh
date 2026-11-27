#!/bin/bash
# Shared helpers of the test scripts: start/stop the fake Orthanc and the portal, tiny assertions.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TESTS="$ROOT/tests"
PORTAL_BIN="${PORTAL_BIN:-$ROOT/target/debug/orthanc-portal}"
export MOCK_LOG_DIR="${MOCK_LOG_DIR:-/tmp/portal-tests}"
WORK="$MOCK_LOG_DIR"
F=http://127.0.0.1:18080     # intranet listener: the staff portal (accounts + permissions)
G=http://127.0.0.1:18081     # internet listener: ONLY invitation (share) links, for guests
H='X-Requested-With: portal'; J='Content-Type: application/json'
SID=aaaaaaaa-bbbbbbbb-cccccccc-dddddddd-eeeeeeee
SER=11111111-22222222-33333333-44444444-55555555
SR=99999999-88888888-77777777-66666666-55555555
UID_=1.2.826.0.1.3680043.8.498.1   # StudyInstanceUID of the study the fake Orthanc holds
PASS=0; FAIL=0; SKIP=0

# ---- Windows (Git Bash) ----------------------------------------------------------------------------
# * "python3" may be missing or a Microsoft Store placeholder: use "python" then.
# * Paths: Git Bash converts /c/... paths for Windows programs only when they are separate arguments,
#   never inside a piece of code. So Python snippets below always receive paths as arguments (sys.argv).
# * There is no pkill: the processes started here are stopped by their recorded process numbers (that
#   is also safer everywhere: it can never stop a portal or an Orthanc that these scripts did not start).
on_windows() { case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) return 0 ;; *) return 1 ;; esac; }
# PYTHON3 = the real program (for processes started in the background: their number must be Python's
# own, so they can be stopped); python3 = the same, for everything else.
export PYTHON3=python3
if ! command python3 -c "" 2>/dev/null; then PYTHON3=python; python3() { python "$@"; }; fi
[ -f "$PORTAL_BIN" ] || { [ -f "$PORTAL_BIN.exe" ] && PORTAL_BIN="$PORTAL_BIN.exe"; }

# Every process started here writes its number to $WORK/<name>.pid, and is stopped by that number.
remember() { echo "$2" > "$WORK/$1.pid"; }
# stop_pid NAME -> stops that process and waits (up to 5 s) until it is gone, so its port is free again
stop_pid() {
  local file="$WORK/$1.pid" pid i
  [ -f "$file" ] || return 0
  pid=$(cat "$file"); rm -f "$file"
  kill "$pid" 2>/dev/null || return 0
  for i in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.1; done
  kill -9 "$pid" 2>/dev/null
}
portal_stop() { stop_pid portal; sleep 0.3; }
stack_stop() { stop_pid portal; stop_pid mock; stop_pid smtp; sleep 0.3; }
mock_start() {
  "$PYTHON3" "$TESTS/mock_orthanc.py" > "$WORK/mock.out" 2>&1 & remember mock $!
  "$PYTHON3" "$TESTS/mock_smtp.py" 18025 "$WORK/mail" > "$WORK/smtp.out" 2>&1 & remember smtp $!
  sleep 0.6
}
# mock_auth MODE  -> changes how the fake Orthanc protects itself while it runs:
#   basic = needs the account, none = no users and we are an allowed client, deny = no users and we are NOT allowed
mock_auth() { curl -s -X POST -d "{\"mode\":\"$1\"}" http://127.0.0.1:18042/__mock/auth > /dev/null; }
# portal_start [ENV=value ...]  (first start: environment values seed the settings)
portal_start() {
  # UPDATE_CHECK_URL: the update check asks the fake Orthanc's stand-in, never GitHub.
  # CD_VIEWER_*: the Weasis package of patient CDs is a small fake served by the fake Orthanc.
  local cdv_sha; cdv_sha=$("$PYTHON3" "$TESTS/fake_cd_viewer.py" "$WORK/fake-cd-viewer.zip")
  env ORTHANC_URL=http://127.0.0.1:18042 ORTHANC_USERNAME=orthanc ORTHANC_PASSWORD=s3cr3tpw DATABASE_PATH="$WORK/portal.db" \
      UPDATE_CHECK_URL=http://127.0.0.1:18042/__mock/github/latest \
      CD_VIEWER_URL=http://127.0.0.1:18042/__mock/cd-viewer.zip CD_VIEWER_SHA256="$cdv_sha" \
      BIND_ADDR=127.0.0.1:18080 REMOTE_BIND_ADDR=127.0.0.1:18081 ADMIN_PASSWORD=SuperSecret123 "$@" "$PORTAL_BIN" >> "$WORK/portal.log" 2>&1 &
  remember portal $!
  sleep 2.5
}
stack_fresh() { stack_stop; rm -rf "$WORK"; mkdir -p "$WORK"; mock_start; portal_start "$@"; }

ok()    { if [ "$2" == "$3" ]; then PASS=$((PASS+1)); echo "PASS  $1"; else FAIL=$((FAIL+1)); echo "FAIL  $1  (got: $2 | expected: $3)"; fi; }
has()   { if echo "$2" | grep -qi -- "$3"; then PASS=$((PASS+1)); echo "PASS  $1"; else FAIL=$((FAIL+1)); echo "FAIL  $1  (missing: $3 in: ${2:0:240})"; fi; }
# skip NAME REASON -> a check that cannot run on this system (counted apart, never as a failure)
skip()  { SKIP=$((SKIP+1)); echo "SKIP  $1  ($2)"; }
hasnt() { if echo "$2" | grep -qi -- "$3"; then FAIL=$((FAIL+1)); echo "FAIL  $1  (found: $3)"; else PASS=$((PASS+1)); echo "PASS  $1"; fi; }
py()    { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
# login BASE USER PASS [extra curl args]  -> the session cookie value
login() { local base=$1 u=$2 p=$3; shift 3; curl -s -D - -o /dev/null "$@" -H "$H" -H "$J" -d "{\"username\":\"$u\",\"password\":\"$p\"}" "$base/api/login" | grep -i set-cookie | sed 's/.*portal_session=\([0-9a-f]*\);.*/\1/'; }
# code BASE COOKIE METHOD PATH [curl args]  -> HTTP status
code()  { local base=$1 cookie=$2 method=$3 path=$4; shift 4; curl -s -o /dev/null -w "%{http_code}" --max-time 8 -X "$method" -H "Cookie: portal_session=$cookie" -H "$H" "$@" "$base$path"; }
body()  { local base=$1 cookie=$2 path=$3; shift 3; curl -s --max-time 8 -H "Cookie: portal_session=$cookie" -H "$H" "$@" "$base$path"; }
put()   { curl -s --max-time 10 -w "\n%{http_code}" -X PUT -H "Cookie: portal_session=$1" -H "$H" -H "$J" -d "$2" "$F/api/admin/settings"; }

# ---- the guest side (internet listener, invitation links) -----------------------------------------
# Guests have no account: they hold the cookie `share_session`, issued when the password is right.
# mkshare COOKIE JSON  -> JSON of a new invitation for the test study (made by a staff member)
mkshare() { body $F $1 /api/studies/$SID/share -X POST -H "$J" -d "$2"; }
# unlock_code TOKEN PASSWORD [curl args] -> HTTP status of the password check
unlock_code() { local t=$1 p=$2; shift 2; curl -s -o /dev/null -w "%{http_code}" --max-time 15 "$@" -H "$H" -H "$J" -d "{\"password\":\"$p\"}" "$G/api/share/$t/unlock"; }
# unlock TOKEN PASSWORD [curl args] -> the guest session cookie value (empty when refused)
unlock() { local t=$1 p=$2; shift 2; curl -s -D - -o /dev/null --max-time 15 "$@" -H "$H" -H "$J" -d "{\"password\":\"$p\"}" "$G/api/share/$t/unlock" | grep -i set-cookie | sed 's/.*share_session=\([0-9a-f]*\);.*/\1/'; }
# gcode COOKIE METHOD PATH [curl args] -> HTTP status on the internet listener
gcode() { local cookie=$1 method=$2 path=$3; shift 3; curl -s -o /dev/null -w "%{http_code}" --max-time 8 -X "$method" -H "Cookie: share_session=$cookie" -H "$H" "$@" "$G$path"; }
gbody() { local cookie=$1 path=$2; shift 2; curl -s --max-time 8 -H "Cookie: share_session=$cookie" -H "$H" "$@" "$G$path"; }
# sql "statement"  -> runs it on the portal's database (used to move a link's expiry into the past)
sql() { python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[2], timeout=10);c.execute(sys.argv[1]);c.commit()" "$1" "$WORK/portal.db"; }
# sqlval "select ..." -> the first value of the first row
sqlval() { python3 -c "import sqlite3,sys;print(sqlite3.connect(sys.argv[2], timeout=10).execute(sys.argv[1]).fetchone()[0])" "$1" "$WORK/portal.db"; }
