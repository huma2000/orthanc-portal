#!/bin/bash
# A local environment to try the portal by hand: the fake Orthanc in demo mode (a dozen studies with
# preview images, uploads create new studies) plus the portal built from this folder.
#
#   tests/dev_env.sh start     build, start both, and create the demo accounts the first time
#   tests/dev_env.sh stop      stop both
#   tests/dev_env.sh restart   stop, rebuild and start again (keeps users and settings)
#   tests/dev_env.sh reset     stop and forget everything (users, settings, audit log)
#   tests/dev_env.sh status
#   tests/dev_env.sh shares    replace every invitation link with fresh demo links in every state
#                              (the first start makes them too); users and settings are kept
#   tests/dev_env.sh mail [N]  the last email the portal sent (or the last N, as summaries): invitations
#                              go to a fake mail server that keeps them in tests/.dev-env/mail/*.eml
#   tests/dev_env.sh guest [N] see what a person outside sees: a new invitation link for demo study N
#                              (1-14, default 1), its password, and the link opened in your browser
#                              (NO_BROWSER=1 to only print them)
#
# Invitation passwords are shown only once, so every link this script makes is written, with its
# password, to tests/.dev-env/guest-links.txt. Links made in the portal (Share... on a study) also
# work locally: the public address of the share links is this environment's invitation port.
#
# It uses its own ports (19042 Orthanc, 19080 staff portal, 19081 invitation links) and its own folder
# (tests/.dev-env, not committed), so it never collides with the automated tests or a real installation.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/tests/.dev-env"
ORTHANC_PORT="${ORTHANC_PORT:-19042}"; STAFF_PORT="${STAFF_PORT:-19080}"; GUEST_PORT="${GUEST_PORT:-19081}"
SMTP_PORT="${SMTP_PORT:-19025}"
MAIL="$DIR/mail"
F="http://127.0.0.1:$STAFF_PORT"
# Invitation links use "localhost", the staff portal "127.0.0.1": browsers keep cookies per host name,
# so the invitation page never sees your staff sign-in, exactly like a person outside.
GUEST_URL="http://localhost:$GUEST_PORT"
LINKS="$DIR/guest-links.txt"
H='X-Requested-With: portal'; J='Content-Type: application/json'
BIN="$ROOT/target/debug/orthanc-portal"; [ -f "$BIN.exe" ] && BIN="$BIN.exe"

# python3 on Linux/macOS, python on Windows (where "python3" may be a Store placeholder).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python

running() { [ -f "$DIR/$1.pid" ] && kill -0 "$(cat "$DIR/$1.pid")" 2>/dev/null; }

stop() {
  for name in portal mock smtp; do
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
  api -X PUT "$F/api/admin/settings" -o /dev/null -d "{\"portal_name\":\"Demo Imaging Centre\",\"roles\":$roles,\"simplified_enabled\":true,\"share_public_url\":\"$GUEST_URL\"}"
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"reception","password":"ReceptionPass123","permissions":["read","visualize","upload","switch_view"],"role":"contributor"}'
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"viewer","password":"ViewerPass123","permissions":["read","visualize"],"role":"viewer"}'
  api -X POST "$F/api/admin/users" -o /dev/null -d '{"username":"doctor","password":"DoctorPass123","permissions":["read","visualize","download","send","share","switch_view"],"role":"exporter"}'
  api -X POST "$F/api/logout" -o /dev/null
}

login_as() { curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"username\":\"$1\",\"password\":\"$2\"}" "$F/api/login" \
  | grep -i set-cookie | sed 's/.*portal_session=\([0-9a-f]*\);.*/\1/'; }

# demo_link COOKIE STUDY_NUMBER HOURS DOWNLOADS NOTE  -> "id token password url" of a new invitation,
# also written to $LINKS (the password cannot be read again later)
demo_link() {
  local study out note="$5" number="$2"; study=$(printf '%08x-0000-0000-0000-00000000000a' "$2")
  out=$(curl -s --max-time 10 -H "Cookie: portal_session=$1" -H "$H" -H "$J" -X POST \
    -d "{\"hours\":$3,\"downloads\":$4,\"note\":\"$5\"}" "$F/api/studies/$study/share" \
    | "$PY" -c "import sys,json;d=json.load(sys.stdin);print(d['id'], d['url'].rsplit('/s/',1)[1], d['password'], d['url'])" | tr -d '\r')
  [ -n "$out" ] || { echo "could not create the link (is the public address of the share links set?)" >&2; return 1; }
  set -- $out
  printf '%s  %-60s  password %s  (%s)\n' "$(date '+%Y-%m-%d %H:%M')" "$4" "$3" "study $number: $note" >> "$LINKS"
  echo "$out"
}

# Makes sure links point at this environment's invitation port (older environments used 127.0.0.1).
use_local_links() {
  curl -s -o /dev/null --max-time 10 -H "Cookie: portal_session=$1" -H "$H" -H "$J" -X PUT \
    -d "{\"share_public_url\":\"$GUEST_URL\"}" "$F/api/admin/settings"
}

# guest [STUDY_NUMBER]: what a person outside sees. A new link (48 hours, 3 downloads), its password,
# and the link opened in the default browser.
guest() {
  local study="${1:-1}" admin doctor
  [[ "$study" =~ ^[0-9]+$ ]] && [ "$study" -ge 1 ] && [ "$study" -le 14 ] || { echo "the demo studies are numbered 1 to 14"; return 1; }
  running portal || { echo "the environment is not running: tests/dev_env.sh start"; return 1; }
  admin=$(login_as admin AdminPass123); doctor=$(login_as doctor DoctorPass123)
  [ -n "$admin" ] && [ -n "$doctor" ] || { echo "could not sign in as admin / doctor"; return 1; }
  use_local_links "$admin"
  set -- $(demo_link "$doctor" "$study" 48 3 "Preview from dev_env.sh") || return 1
  cat <<EOF

What a person outside sees (as if they had received the invitation):
  Link:      $4
  Password:  $3          (typed with or without the dashes, in any case)

It allows 3 downloads and works for 48 hours. Watch it from the staff side in Shares (as doctor or admin).
All links made by this script, with their passwords: $LINKS
EOF
  [ "${NO_BROWSER:-0}" = 1 ] && return 0
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) cmd.exe /c start "" "$4" ;;
    Darwin) open "$4" ;;
    *) xdg-open "$4" >/dev/null 2>&1 || echo "(open the link in your browser)" ;;
  esac
}

# guest_visit TOKEN PASSWORD DOWNLOADS  -> a guest opens the link and downloads the study N times
guest_visit() {
  local g="http://127.0.0.1:$GUEST_PORT" cookie i
  cookie=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"password\":\"$2\"}" "$g/api/share/$1/unlock" \
    | grep -i set-cookie | sed 's/.*share_session=\([0-9a-f]*\);.*/\1/')
  curl -s -o /dev/null -H "Cookie: share_session=$cookie" "$g/api/share/$1/study"
  for ((i = 0; i < $3; i++)); do curl -s -o /dev/null -H "Cookie: share_session=$cookie" "$g/api/share/$1/download"; done
}

# db_set_expiry SHARE_ID SECONDS_FROM_NOW  (negative = already expired; the API never allows that)
# Removes every invitation link of this environment (a fresh set of demo links follows).
db_clear_shares() {
  (cd "$DIR" && "$PY" -c "import sqlite3;c=sqlite3.connect('portal.db',timeout=10);n=c.execute('delete from shares').rowcount;c.commit();print(f'removed {n} invitation links')")
  : > "$LINKS"
}

db_set_expiry() {
  (cd "$DIR" && "$PY" -c "import sqlite3,sys,time;c=sqlite3.connect('portal.db',timeout=10);c.execute('update shares set expires_at=? where id=?',(int(time.time())+int(sys.argv[2]),int(sys.argv[1])));c.commit()" "$1" "$2")
}

# Invitation links in every state, to see how the Shares page shows time and downloads left.
demo_shares() {
  local doctor admin
  doctor=$(login_as doctor DoctorPass123); admin=$(login_as admin AdminPass123)
  [ -n "$doctor" ] && [ -n "$admin" ] || { echo "could not sign in as doctor / admin"; return 1; }
  use_local_links "$admin"
  set -- $(demo_link "$doctor" 1 48 3 "Dr. Smith, second opinion"); guest_visit "$2" "$3" 1      # active, 1 of 3 used
  set -- $(demo_link "$doctor" 2 6 1 "Patient copy");             guest_visit "$2" "$3" 1      # active, no download left
  set -- $(demo_link "$doctor" 3 720 0 "Online viewing only")                                 # view only, never opened
  set -- $(demo_link "$doctor" 4 24 2 "Referring vet, urgent");   guest_visit "$2" "$3" 0; db_set_expiry "$1" 1500   # 25 min left
  set -- $(demo_link "$doctor" 5 24 2 "Insurance");               guest_visit "$2" "$3" 2; db_set_expiry "$1" -7200  # expired
  set -- $(demo_link "$doctor" 6 48 1 "Sent to the wrong address")
  curl -s -o /dev/null -H "Cookie: portal_session=$doctor" -H "$H" -X POST "$F/api/shares/$1/revoke"           # revoked
  set -- $(demo_link "$admin" 7 168 5 "Teaching file");           guest_visit "$2" "$3" 2      # made by another user
  echo "created 7 demo invitation links (active, no downloads left, view only, about to expire, expired, revoked, by admin)"
  echo "their links and passwords: $LINKS"
}

# Points Settings > Email at the fake mail server, unless a mail server was already set by hand.
use_local_mail() {
  local admin; admin=$(login_as admin AdminPass123)
  [ -n "$admin" ] || return 0
  local host; host=$(curl -s --max-time 10 -H "Cookie: portal_session=$admin" "$F/api/admin/settings" | "$PY" -c "import sys,json;print(json.load(sys.stdin)['settings']['smtp_host'])" | tr -d '\r')
  [ -z "$host" ] || return 0
  curl -s -o /dev/null --max-time 10 -H "Cookie: portal_session=$admin" -H "$H" -H "$J" -X PUT \
    -d "{\"smtp_host\":\"127.0.0.1\",\"smtp_port\":$SMTP_PORT,\"smtp_security\":\"none\",\"smtp_from\":\"Demo Imaging Centre <pacs@demo.example>\",\"email_footer\":\"Demo Imaging Centre - 1 High Street - 01234 567890\"}" "$F/api/admin/settings"
}

# mail [N]: the last email the portal sent, as the recipient reads it; with N, a list of the last N.
show_mail() {
  [ -d "$MAIL" ] && ls "$MAIL"/*.eml > /dev/null 2>&1 || { echo "no email sent yet (folder $MAIL)"; return 0; }
  "$PY" - "$MAIL" "${1:-}" <<'PY'
import sys, glob, os
from email import message_from_bytes, policy
files = sorted(glob.glob(os.path.join(sys.argv[1], "*.eml")))
count = sys.argv[2]
if count:
    for path in files[-int(count):]:
        m = message_from_bytes(open(path, "rb").read(), policy=policy.default)
        print(f"{os.path.basename(path)}  {m['Date']}  to {m['To']}  {m['Subject']}")
else:
    m = message_from_bytes(open(files[-1], "rb").read(), policy=policy.default)
    for header in ("From", "To", "Date", "Subject"):
        print(f"{header}: {m[header]}")
    print("\n" + m.get_body(("plain",)).get_content())
    print(f"(file: {files[-1]}; it opens in any mail program)")
PY
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
  if ! running smtp; then
    "$PY" "$ROOT/tests/mock_smtp.py" "$SMTP_PORT" "$MAIL" > "$DIR/smtp.out" 2>&1 &
    echo $! > "$DIR/smtp.pid"
  fi

  if ! running portal; then
    env ORTHANC_URL="http://127.0.0.1:$ORTHANC_PORT" ORTHANC_USERNAME=orthanc ORTHANC_PASSWORD=s3cr3tpw \
        DATABASE_PATH="$DIR/portal.db" BIND_ADDR="127.0.0.1:$STAFF_PORT" REMOTE_BIND_ADDR="127.0.0.1:$GUEST_PORT" \
        LISTENERS_FROM_ENV=true ADMIN_USERNAME=admin ADMIN_PASSWORD=AdminPass123 \
        "$BIN" >> "$DIR/portal.log" 2>&1 &
    echo $! > "$DIR/portal.pid"
  fi
  wait_for "$F/healthz" || { echo "the portal did not start (see $DIR/portal.log)"; exit 1; }
  [ $first = 1 ] && seed && demo_shares
  use_local_mail

  cat <<EOF

Test environment running.
  Staff portal:      $F
  Invitation links:  $GUEST_URL  (what people outside see: tests/dev_env.sh guest)
  Fake Orthanc:      http://127.0.0.1:$ORTHANC_PORT  (demo data; uploads create new studies)
  Fake mail server:  127.0.0.1:$SMTP_PORT  (emails kept in $MAIL; read the last one: tests/dev_env.sh mail)

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
  shares) running portal || { echo "the environment is not running: tests/dev_env.sh start"; exit 1; }; db_clear_shares; demo_shares ;;
  guest) guest "$2" ;;
  mail) show_mail "$2" ;;
  status) for n in mock smtp portal; do running $n && echo "$n: running" || echo "$n: stopped"; done ;;
  *) echo "usage: $0 start|stop|restart|reset|status|shares|guest [study]|mail [count]"; exit 1 ;;
esac
