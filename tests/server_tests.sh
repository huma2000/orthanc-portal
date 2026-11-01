#!/bin/bash
# End-to-end tests of the portal's server side, against a fake Orthanc (see mock_orthanc.py).
#   tests/server_tests.sh            (uses target/debug/orthanc-portal; PORTAL_BIN=... to test another binary)
source "$(dirname "$0")/lib.sh"
stack_fresh
cd "$WORK"

# ---- fixtures ---------------------------------------------------------------------------
python3 - <<'PY'
import zlib, struct, os
def png(w, h, rgb):
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open("logo.png", "wb").write(png(120, 40, (200, 30, 30)))
open("icon.png", "wb").write(png(64, 64, (20, 120, 60)))
open("evil_script.svg", "w").write("<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>")
open("evil_onload.svg", "w").write("<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'><circle r='3'/></svg>")
open("notes.txt", "w").write("not an image")
open("big.dcm", "wb").write(os.urandom(2_000_000)); open("small.dcm", "wb").write(os.urandom(5000))
PY

# ---- users ------------------------------------------------------------------------------
A=$(login $F admin SuperSecret123)
mkuser() { body $F $A /api/admin/users -X POST -H "$J" -d "{\"username\":\"$1\",\"password\":\"$2\",\"permissions\":[$3]}" > /dev/null; }
mkuser viewer1 ViewerPass123 '"read","visualize"'
mkuser reader  ReaderPass123 '"read"'
mkuser sender  SenderPass123 '"read","send"'
mkuser capuser CapUserPass123 '"read"'
V=$(login $F viewer1 ViewerPass123); RD=$(login $F reader ReaderPass123); S=$(login $F sender SenderPass123)
AR=$(login $R admin SuperSecret123); VR=$(login $R viewer1 ViewerPass123)
tok() { body $F $1 "/api/weasis/link?study_uid=1.2.3" | python3 -c "import sys,json,urllib.parse,re;print(re.search(r'Bearer (\w+)',urllib.parse.unquote(json.load(sys.stdin)['uri'])).group(1))"; }

echo "== two doors: intranet (full) and internet (view only)"
ME=$(body $F $A /api/me)
ok "intranet admin: scope full, all six permissions" "$(echo $ME | py "d['scope']+' '+','.join(d['permissions'])")" "full read,visualize,upload,download,send,admin"
ME=$(body $R $AR /api/me)
ok "internet admin: scope remote, only read + visualize" "$(echo $ME | py "d['scope']+' '+','.join(d['permissions'])")" "remote read,visualize"
ME=$(body $F $A /api/me -H 'CF-Connecting-IP: 1.2.3.4' -H 'Cf-Ray: abc')
ok "Cloudflare headers on the intranet port downgrade to remote" "$(echo $ME | py "d['scope']+' '+','.join(d['permissions'])")" "remote read,visualize"
for r in "POST /api/upload" "GET /api/studies/$SID/download" "GET /api/modalities" "POST /api/studies/$SID/send" "GET /api/transfers" "GET /api/admin/users" "GET /api/admin/audit" "GET /api/admin/settings" "GET /api/admin/nodes" "PUT /api/admin/branding/logo"; do set -- $r
  ok "internet listener: $1 $2 does not exist" "$(code $R $AR $1 $2 -H "$J" -d '{}' | sed 's/405/404/')" "404"; done
ok "Cloudflare headers: upload refused (403)" "$(code $F $A POST /api/upload -H 'CF-Connecting-IP: 1.2.3.4' --data-binary x)" "403"
ok "Cloudflare headers: admin refused (403)" "$(code $F $A GET /api/admin/settings -H 'Cf-Ray: abc')" "403"
ok "Cloudflare headers: ZIP download refused (403)" "$(code $F $A GET /api/studies/$SID/download -H 'Cdn-Loop: cloudflare')" "403"

echo "== from the internet images are viewed ONLY in the browser (OHIF), never through Weasis"
ok "features: Weasis allowed on the intranet" "$(body $F $V /api/me | py "d['features']['weasis']")" "True"
ok "features: Weasis NOT offered over the internet" "$(body $R $VR /api/me | py "d['features']['weasis']")" "False"
ok "features: the browser viewer is offered over the internet" "$(body $R $VR /api/me | py "d['features']['ohif']")" "True"
ok "features: Weasis not offered with Cloudflare headers either" "$(body $F $V /api/me -H 'CF-Connecting-IP: 1.2.3.4' | py "d['features']['weasis']")" "False"
ok "Weasis link on the intranet works" "$(code $F $V GET '/api/weasis/link?study_uid=1.2.3')" "200"
ok "Weasis link route does not exist on the internet listener" "$(code $R $VR GET '/api/weasis/link?study_uid=1.2.3')" "404"
OUT=$(body $F $V '/api/weasis/link?study_uid=1.2.3' -H 'CF-Connecting-IP: 1.2.3.4' -w ' [%{http_code}]')
has "Weasis link with Cloudflare headers on the intranet port: refused" "$OUT" "\[403\]"
has "...and the message points to the browser viewer" "$OUT" "browser viewer"
T=$(tok $V)
ok "a Weasis token works on the intranet" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" $F/dicom-web/studies)" "200"
ok "...but is useless on the internet listener (401)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" $R/dicom-web/studies)" "401"
ok "...and with Cloudflare headers on the intranet port (401)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" -H 'CF-Connecting-IP: 1.2.3.4' $F/dicom-web/studies)" "401"
ok "a bearer token does not help when the session cookie is wrong either" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" -H 'Cookie: portal_session=bad' $R/dicom-web/studies)" "401"
ok "the browser session cookie works over the internet (OHIF)" "$(code $R $VR GET /dicom-web/studies)" "200"
ok "the browser viewer page is served over the internet" "$(code $R $VR GET '/ohif/viewer?StudyInstanceUIDs=1.2.3')" "200"
ok "not signed in: DICOMweb 401" "$(curl -s -o /dev/null -w '%{http_code}' $R/dicom-web/studies)" "401"
ok "not signed in: OHIF 401" "$(curl -s -o /dev/null -w '%{http_code}' $R/ohif/viewer)" "401"

echo "== over the internet whole DICOM files cannot be downloaded through the viewer address"
for path in "studies/1.2.3" "studies/1.2.3/series/4.5" "studies/1.2.3/series/4.5/instances/6.7" "series/4.5" "instances/6.7"; do
  ok "internet: /dicom-web/$path -> 403" "$(code $R $VR GET /dicom-web/$path)" "403"; done
ok "...also via Cloudflare headers on the intranet port" "$(code $F $V GET /dicom-web/studies/1.2.3/series/4.5/instances/6.7 -H 'CF-Connecting-IP: 1.2.3.4')" "403"
OUT=$(body $R $VR /dicom-web/studies/1.2.3)
has "the refusal explains itself" "$OUT" "whole DICOM files"
for path in "studies?limit=1" "studies/1.2.3/series" "studies/1.2.3/instances" "studies/1.2.3/series/4.5/instances" "studies/1.2.3/metadata" "studies/1.2.3/series/4.5/metadata" "studies/1.2.3/series/4.5/instances/6.7/metadata" "studies/1.2.3/series/4.5/instances/6.7/frames/1" "studies/1.2.3/series/4.5/instances/6.7/rendered" "studies/1.2.3/series/4.5/instances/6.7/thumbnail" "series?limit=5" "instances?limit=5"; do
  ok "internet: /dicom-web/$path (what a viewer needs) -> 200" "$(code $R $VR GET "/dicom-web/$path")" "200"; done
W='/wado?requestType=WADO&studyUID=1&seriesUID=2&objectUID=3'
ok "internet: WADO-URI without a picture type (= DICOM file) -> 403" "$(code $R $VR GET "$W")" "403"
ok "internet: WADO-URI asking for application/dicom -> 403" "$(code $R $VR GET "$W&contentType=application/dicom")" "403"
ok "internet: WADO-URI asking for a JPEG picture -> 200" "$(code $R $VR GET "$W&contentType=image/jpeg")" "200"
ok "internet: ...URL-encoded type -> 200" "$(code $R $VR GET "$W&contentType=image%2Fjpeg")" "200"
ok "internet: a list mixing picture and DICOM -> 403" "$(code $R $VR GET "$W&contentType=image/jpeg,application/dicom")" "403"
ok "internet: a second contentType smuggling DICOM -> 403" "$(code $R $VR GET "$W&contentType=image/jpeg&contentType=application/dicom")" "403"
ok "internet: the DICOMweb plugin's other resources stay blocked (404)" "$(code $R $VR GET /dicom-web/servers)" "404"
ok "intranet is NOT restricted: whole study (Weasis needs it) -> 200" "$(code $F $V GET /dicom-web/studies/1.2.3/series/4.5/instances/6.7)" "200"
ok "intranet: WADO-URI DICOM file -> 200" "$(code $F $V GET "$W")" "200"
ok "internet: thumbnails (small previews) still work" "$(code $R $VR GET /api/studies/$SID/thumbnail)" "200"
ok "the setting is on by default" "$(body $F $A /api/admin/settings | py "d['settings']['remote_block_dicom_files']")" "True"
ok "the setting must be a true/false value (400)" "$(put $A '{"remote_block_dicom_files":"maybe"}' | tail -1)" "400"
put $A '{"remote_block_dicom_files":false}' > /dev/null
ok "switched off: internet whole-file retrieval allowed again" "$(code $R $VR GET /dicom-web/studies/1.2.3/series/4.5/instances/6.7)" "200"
ok "switched off: Weasis STILL refused over the internet (not a setting)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" $R/dicom-web/studies)" "401"
put $A '{"remote_block_dicom_files":true}' > /dev/null
ok "switched on again: blocked" "$(code $R $VR GET /dicom-web/studies/1.2.3)" "403"

echo "== search, modality filter, series"
body $F $V "/api/studies?modality=ct,rx&patient_name=DOE" > /dev/null; Q=$(tail -1 find.log)
has "modalities become a DICOM 'any of' list CT\\CR\\DX" "$Q" '"ModalitiesInStudy": "CT\\\\CR\\\\DX"'
has "newest first (OrderBy) and computed tags" "$Q" '"OrderBy"'
ok "invalid modality -> 400" "$(code $F $V GET '/api/studies?modality=C%3BT')" "400"
body $F $V "/api/series?modality=US,mr" > /dev/null
has "series search uses Modality list" "$(tail -1 find.log)" '"Modality": "US\\\\MR"'
ok "study result carries its modalities" "$(body $F $V '/api/studies' | py "','.join(d['items'][0]['modalities'])")" "CT,SR"
ok "series of a study sorted, 2 series" "$(body $F $V /api/studies/$SID/series | py "len(d)")" "2"
ok "read-only user can search" "$(code $F $RD GET /api/studies)" "200"

echo "== thumbnails"
curl -s -D th.h -o th.b -H "Cookie: portal_session=$V" "$F/api/studies/$SID/thumbnail"
has "study thumbnail is a JPEG, never stored by browsers" "$(cat th.h)" "image/jpeg"
has "...no-store" "$(cat th.h)" "private, no-store"
ok "picks the CT series (not the bigger report), middle image fails -> falls back (i2 then i1)" "$(python3 -c "import json;print(','.join(json.loads(l)['id'] for l in open('rendered.log')))")" "i2,i1"
N=$(wc -l < rendered.log); body $F $V /api/studies/$SID/thumbnail -o /dev/null
ok "second request is served from the cache" "$(wc -l < rendered.log)" "$N"
ok "report series has no preview -> 404" "$(code $F $V GET /api/series/$SR/thumbnail)" "404"
ok "needs visualize: read-only user -> 403" "$(code $F $RD GET /api/studies/$SID/thumbnail)" "403"
ok "invalid id -> 400" "$(code $F $V GET '/api/studies/..%2Fsystem/thumbnail')" "400"
put $A '{"thumbnails_enabled":false}' > /dev/null
ok "switched off: 404" "$(code $F $V GET /api/studies/$SID/thumbnail)" "404"
put $A '{"thumbnails_enabled":true}' > /dev/null

echo "== send a study to a DICOM node, follow the transfer"
ok "list nodes (AllowStore=false hidden)" "$(body $F $S /api/modalities | py "','.join(n['id'] for n in d)")" "down,ris"
ok "viewer cannot list nodes (403)" "$(code $F $V GET /api/modalities)" "403"
R1=$(body $F $S /api/studies/$SID/send -X POST -H "$J" -d '{"target":"ris"}')
has "Orthanc job id returned" "$R1" "27d7b341"
has "asynchronous store requested" "$(tail -1 store.log)" '"Synchronous": false'
ok "unknown destination -> 400" "$(code $F $S POST /api/studies/$SID/send -H "$J" -d '{"target":"../tools"}')" "400"
T1=$(body $F $S /api/transfers | py "d[0]['state']"); T2=$(body $F $S /api/transfers | py "d[0]['state']"); T3=$(body $F $S /api/transfers | py "d[0]['state']")
ok "job state Running, Running, Success" "$T1,$T2,$T3" "Running,Running,Success"
N=$(wc -l < jobs.log); for i in 1 2 3; do body $F $S /api/transfers > /dev/null; done
ok "finished transfers are remembered: Orthanc is not asked again" "$(wc -l < jobs.log)" "$N"

echo "== upload and download"
ok "upload a file" "$(code $F $A POST /api/upload -H 'Content-Type: application/dicom' --data-binary @small.dcm)" "200"
ok "upload needs the upload permission" "$(code $F $V POST /api/upload --data-binary @small.dcm)" "403"
ok "ZIP download streams" "$(body $F $A /api/studies/$SID/download | wc -c | awk '{print ($1>1000)?"ok":"short"}')" "ok"
ok "download needs the download permission" "$(code $F $V GET /api/studies/$SID/download)" "403"

echo "== branding (title, icon, logo)"
P=$(curl -s $F/api/public)
ok "default: no custom logo, built-in icon" "$(echo $P | py "str(d['logo'])+' '+d['favicon']['url']")" "None /favicon.svg"
put $A '{"browser_title":"A&B <b>x</b>"}' > /dev/null
has "window title is written into the HTML, escaped" "$(curl -s $F/)" '<title>A&amp;B &lt;b&gt;x&lt;/b&gt;</title>'
put $A '{"browser_title":"","portal_name":"Sunrise Radiology"}' > /dev/null
has "empty window title falls back to the portal name" "$(curl -s $F/)" "<title>Sunrise Radiology</title>"
up() { curl -s -o up.out -w "%{http_code}" -X PUT -H "Cookie: portal_session=$1" -H "$H" -H "Content-Type: ${4:-application/octet-stream}" --data-binary @$3 $F/api/admin/branding/$2; }
ok "upload a PNG logo" "$(up $A logo logo.png image/png)" "200"
ok "the logo is public and served as PNG" "$(curl -s -o /dev/null -w '%{content_type}' $F/branding/logo)" "image/png"
has "uploaded images are sandboxed" "$(curl -sI $F/branding/logo)" "sandbox"
ok "upload a website icon" "$(up $A favicon icon.png image/png)" "200"
has "the HTML links the custom icon" "$(curl -s $F/)" 'href="/branding/favicon?v='
ok "SVG with <script> refused" "$(up $A logo evil_script.svg image/svg+xml)" "400"
ok "SVG with onload= refused" "$(up $A logo evil_onload.svg image/svg+xml)" "400"
ok "text file refused" "$(up $A logo notes.txt image/png)" "400"
ok "viewers cannot change the branding" "$(up $V logo logo.png)" "403"
ok "the internet listener shows the same logo" "$(curl -s $R/api/public | py "d['logo']['custom']")" "True"

echo "== settings: live application, validation, secrets"
DOC=$(body $F $A /api/admin/settings)
ok "Orthanc password is set but never sent" "$(echo $DOC | py "d['settings']['orthanc_password_set']")" "True"
hasnt "the password value is nowhere in the answer" "$DOC" "s3cr3tpw"
ok "session_hours 0 -> 400" "$(put $A '{"session_hours":0}' | tail -1)" "400"
ok "unknown setting -> 400" "$(put $A '{"nope":1}' | tail -1)" "400"
ok "non-admin cannot read settings (403)" "$(code $F $V GET /api/admin/settings)" "403"
put $A '{"session_hours":1}' > /dev/null
has "new session length used by the next login" "$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d '{"username":"admin","password":"SuperSecret123"}' $F/api/login)" "Max-Age=3600"
put $A '{"session_hours":12,"max_upload_mb":1}' > /dev/null
ok "upload limit is live: 2 MB refused" "$(code $F $A POST /api/upload -H 'Content-Type: application/dicom' --data-binary @big.dcm)" "413"
put $A '{"max_upload_mb":2048}' > /dev/null
put $A '{"orthanc_url":"http://127.0.0.1:1"}' > /dev/null
ok "a wrong Orthanc address takes effect at once (502)" "$(code $F $V GET /api/studies)" "502"
put $A '{"orthanc_url":"http://127.0.0.1:18042"}' > /dev/null
ok "...and the right one restores service" "$(code $F $V GET /api/studies)" "200"
T=$(body $F $A /api/admin/orthanc/test -X POST -H "$J" -d '{}')
ok "Test connection reports version and plugins" "$(echo $T | py "str(d['ok'])+' '+d['version']+' ohif='+str(d['ohif'])")" "True 1.12.10 ohif=True"
OUT=$(put $A '{"bind_addr":"127.0.0.1:18090"}')
has "the intranet listener moves at runtime" "$OUT" "now listens on 127.0.0.1:18090"
ok "new port answers, old one is closed" "$(curl -s -o /dev/null -w '%{http_code}' $F/healthz --max-time 2 | sed 's/200/old-open/')/$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18090/healthz)" "000/200"
F=http://127.0.0.1:18090
put $A '{"bind_addr":"127.0.0.1:18080"}' > /dev/null; F=http://127.0.0.1:18080

echo "== sign-in hardening"
lc() { curl -s -o /dev/null -w "%{http_code}" -H "$H" -H "$J" -d "{\"username\":\"$1\",\"password\":\"$2\"}" "${@:3}" $F/api/login; }
ok "70-character user name -> 400" "$(lc "$(python3 -c 'print("x"*70)')" whatever12345)" "400"
ok "user name with a colon -> 400" "$(lc 'ip:1.2.3.4' whatever12345)" "400"
ok "password over 1024 characters -> 400" "$(lc admin "$(python3 -c 'print("p"*1100)')")" "400"
hasnt "none of that reached the audit log" "$(body $F $A '/api/admin/audit?limit=300')" "xxxxxxxxxx"
for i in $(seq 1 20); do lc "user$i" wrong$i -H 'CF-Connecting-IP: 7.7.7.7' > /dev/null; done
ok "an address that fails 20 times with 20 names is locked (429)" "$(lc admin SuperSecret123 -H 'CF-Connecting-IP: 7.7.7.7')" "429"
ok "another address is not affected" "$(lc admin SuperSecret123 -H 'CF-Connecting-IP: 8.8.8.8')" "200"
for i in 1 2 3 4 5; do lc victim wrong$i -H "CF-Connecting-IP: 6.6.6.$i" > /dev/null; done
ok "an account is locked after 5 failures from different addresses" "$(lc victim wrong6 -H 'CF-Connecting-IP: 6.6.6.9')" "429"
FIRST=""; THIRD=""; LAST=""
for i in $(seq 1 22); do C=$(login $F capuser CapUserPass123); [ $i -eq 1 ] && FIRST=$C; [ $i -eq 3 ] && THIRD=$C; LAST=$C; done
ok "sessions per user are capped: the oldest is dropped" "$(code $F $FIRST GET /api/me)/$(code $F $THIRD GET /api/me)/$(code $F $LAST GET /api/me)" "401/200/200"
ok "Weasis links reuse the user's token" "$(tok $V)" "$(tok $V)"
for path in servers peers info; do ok "DICOMweb proxy blocks /dicom-web/$path" "$(code $F $V GET /dicom-web/$path)" "404"; done
curl -s -D js.h -o /dev/null $F/static/app.js; E=$(grep -i '^etag' js.h | sed 's/[^"]*\("[^"]*"\).*/\1/')
ok "static files: matching ETag -> 304" "$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $E" $F/static/app.js)" "304"
has "Permissions-Policy header" "$(curl -sI $F/)" "permissions-policy"
has "API answers are no-store" "$(curl -sI -H "Cookie: portal_session=$A" $F/api/me)" "cache-control: no-store"
has "HSTS only over HTTPS" "$(curl -sI -H 'X-Forwarded-Proto: https' $F/)" "strict-transport-security"
ok "database file is private (600)" "$(stat -c %a $WORK/portal.db)" "600"

echo "== administrators"
mkuser admin2 AdminTwoPass123 '"read","visualize","upload","download","send","admin"'
ID1=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='admin'][0]")
ID2=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='admin2'][0]")
B=$(login $F admin2 AdminTwoPass123)
( curl -s -o /dev/null -w "%{http_code}" -X PUT -H "Cookie: portal_session=$A" -H "$H" -H "$J" -d '{"permissions":["read"]}' $F/api/admin/users/$ID2 > c1.out &
  curl -s -o /dev/null -w "%{http_code}" -X PUT -H "Cookie: portal_session=$B" -H "$H" -H "$J" -d '{"permissions":["read"]}' $F/api/admin/users/$ID1 > c2.out & wait )
ok "two administrators demoting each other: exactly one succeeds" "$(printf '%s\n%s\n' "$(cat c1.out)" "$(cat c2.out)" | grep -c '^200$')" "1"
ok "...and one active administrator always remains" "$(python3 -c "import sqlite3;print(sqlite3.connect('$WORK/portal.db').execute(\"select count(*) from users where active=1 and permissions like '%admin%'\").fetchone()[0])")" "1"

echo; echo "RESULT: $PASS passed, $FAIL failed"
stack_stop
[ "$FAIL" -eq 0 ]
