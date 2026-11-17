#!/bin/bash
# A local environment to try the portal by hand: the fake Orthanc in demo mode (a dozen studies with
# preview images, uploads create new studies) plus the portal built from this folder.
#
#   tests/dev_env.sh start     build, start both, and create the demo accounts the first time
#   tests/dev_env.sh stop      stop both
#   tests/dev_env.sh restart   stop, rebuild and start again (keeps users and settings)
#   tests/dev_env.sh reset     stop and forget everything (users, settings, audit log)
#   tests/dev_env.sh status
#
# It uses its own ports (19042 Orthanc, 19080 staff portal, 19081 invitation links) and its own folder
# (tests/.dev-env, not committed), so it never collides with the automated tests or a real installation.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/tests/.dev-env"
ORTHANC_PORT="${ORTHANC_PORT:-19042}"; STAFF_PORT="${STAFF_PORT:-19080}"; GUEST_PORT="${GUEST_PORT:-19081}"
F="http://127.0.0.1:$STAFF_PORT"
H='X-Requested-With: portal'; J='Content-Type: application/json'
BIN="$ROOT/target/debug/orthanc-portal"; [ -f "$BIN.exe" ] && BIN="$BIN.exe"

# python3 on Linux/macOS, python on Windows (where "python3" may be a Store placeholder).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python

running() { [ -f "$DIR/$1.pid" ] && kill -0 "$(cat "$DIR/$1.pid")" 2>/dev/null; }

stop() {
  for name in portal mock; do
    if running $name; then kill "$(cat "$DIR/$name.pid")" 2>/dev/null; echo "stopped $name"; fi
    rm -f "$DIR/$name.pid"
  done
  sleep 0.5
}

wait_for() { for _ in $(seq 1 40); do curl -s -o /dev/null "$1" && return 0; sleep 0.25; done; return 1; }

api() { curl -s --max-time 10 -H "Cookie: portal_session=$COOKIE" -H "$H" -H "$J" "$@"; }

seed() {
  COOKIE=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d '{"username":"admin","password":"AdminPass123"}' "$F/api/login" \
    | grep -i set-cookie | sed 's/.*portal_session=\([0-9a-f]*\);.*/\1/')
  [ -n "$COOKIE" ] || { echo "could not sign in as admin to create the demo accounts"; return 1; }
  # The viewer and contributor roles use the simplified view; reception and doctor may switch views
  # (switch_view permission), viewer may not.
  local roles
  roles=$(api "$F/api/admin/settings" | "$PY" -c "import sys,json;d=json.load(sys.stdin);print(json.dumps([dict(r, interface='simplified' if r['key'] in ('viewer','contributor') else 'normal') for r in d['settings']['roles']]))")
  api -X PUT "$F/api/admin/settings" -o /dev/null -d "{\"portal_name\":\"Demo Imaging Centre\",\"roles\":$roles,\"simplified_enabled\":true,\"share_public_url\":\"http://127.0.0.1:$GUEST_PORT\"}"
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"reception","password":"ReceptionPass123","permissions":["read","visualize","upload","switch_view"],"role":"contributor"}'
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"viewer","password":"ViewerPass123","permissions":["read","visualize"],"role":"viewer"}'
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"doctor","password":"DoctorPass123","permissions":["read","visualize","download","send","share","switch_view"],"role":"exporter"}'
  api -X POST "$F/api/logout" -o /dev/null
}

start() {
  mkdir -p "$DIR"
  (cd "$ROOT" && cargo build) || exit 1
  local first=0; [ -f "$DIR/portal.db" ] || first=1

  if ! running mock; then
    MOCK_DEMO=1 MOCK_LOG_DIR="$DIR" "$PY" "$ROOT/tests/mock_orthanc.py" "$ORTHANC_PORT" > "$DIR/mock.out" 2>&1 &
    echo $! > "$DIR/mock.pid"
  fi
  wait_for "http://127.0.0.1:$ORTHANC_PORT/system" || { echo "the fake Orthanc did not start (see $DIR/mock.out)"; exit 1; }

  if ! running portal; then
    env ORTHANC_URL="http://127.0.0.1:$ORTHANC_PORT" ORTHANC_USERNAME=orthanc ORTHANC_PASSWORD=s3cr3tpw \
        DATABASE_PATH="$DIR/portal.db" BIND_ADDR="127.0.0.1:$STAFF_PORT" REMOTE_BIND_ADDR="127.0.0.1:$GUEST_PORT" \
        LISTENERS_FROM_ENV=true ADMIN_USERNAME=admin ADMIN_PASSWORD=AdminPass123 \
        "$BIN" >> "$DIR/portal.log" 2>&1 &
    echo $! > "$DIR/portal.pid"
  fi
  wait_for "$F/healthz" || { echo "the portal did not start (see $DIR/portal.log)"; exit 1; }
  [ $first = 1 ] && seed

  cat <<EOF

Test environment running.
  Staff portal:      $F
  Invitation links:  http://127.0.0.1:$GUEST_PORT
  Fake Orthanc:      http://127.0.0.1:$ORTHANC_PORT  (demo data; uploads create new studies)

  Account / password             view        permissions
  admin / AdminPass123           normal      everything (Settings is here)
  reception / ReceptionPass123   simplified  read, visualize, upload, switch_view
  viewer / ViewerPass123         simplified  read, visualize  (cannot switch views)
  doctor / DoctorPass123         normal      read, visualize, download, send, share, switch_view

Logs and database: $DIR
Stop with: tests/dev_env.sh stop
EOF
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  restart) stop; start ;;
  reset) stop; rm -rf "$DIR"; echo "everything forgotten; run tests/dev_env.sh start" ;;
  status) for n in mock portal; do running $n && echo "$n: running" || echo "$n: stopped"; done ;;
  *) echo "usage: $0 start|stop|restart|reset|status"; exit 1 ;;
esac
