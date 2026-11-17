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
mkuser sharer  SharerPass123 '"read","visualize","share"'      # may create invitation links
mkuser sharer2 SharerTwoPass123 '"read","share"'                # another person who may share
V=$(login $F viewer1 ViewerPass123); RD=$(login $F reader ReaderPass123); S=$(login $F sender SenderPass123)
SH=$(login $F sharer SharerPass123); SH2=$(login $F sharer2 SharerTwoPass123)
tok() { body $F $1 "/api/weasis/link?study_uid=1.2.3" | python3 -c "import sys,json,urllib.parse,re;print(re.search(r'Bearer (\w+)',urllib.parse.unquote(json.load(sys.stdin)['uri'])).group(1))"; }
# newlink NAME COOKIE JSON: creates an invitation as a staff member and keeps its parts in NAME_JSON,
# NAME_TOKEN, NAME_PW and NAME_ID (the password can only be read in this one answer).
newlink() {
  local out; out=$(mkshare "$2" "$3")
  printf -v "$1_JSON" '%s' "$out"
  printf -v "$1_TOKEN" '%s' "$(echo "$out" | py "d['url'].rsplit('/s/',1)[1]")"
  printf -v "$1_PW" '%s' "$(echo "$out" | py "d['password']")"
  printf -v "$1_ID" '%s' "$(echo "$out" | py "d['id']")"
}

echo "== two doors: the staff portal (intranet) and the internet listener (invitation links only)"
ME=$(body $F $A /api/me)
ok "intranet admin has all eight permissions" "$(echo $ME | py "','.join(d['permissions'])")" "read,visualize,upload,download,send,share,admin,switch_view"
hasnt "accounts no longer have a remote/full 'scope'" "$ME" '"scope"'
ok "share is a permission that needs 'read' (400 without it)" "$(code $F $A POST /api/admin/users -H "$J" -d '{"username":"badshare","password":"BadSharePass123","permissions":["share"]}')" "400"
ok "the Exporter and Clinical roles include share" "$(body $F $A /api/admin/settings | py "[ 'share' in r['permissions'] for r in d['settings']['roles'] if r['key'] in ('exporter','clinical')]")" "[True, True]"
OUT=$(body $F $A /api/me -H 'CF-Connecting-IP: 1.2.3.4' -H 'Cf-Ray: abc' -w ' [%{http_code}]')
has "Cloudflare headers on the intranet port: refused (403)" "$OUT" "\[403\]"
has "...and the message sends the visitor to their invitation link" "$OUT" "invitation link"
ok "Cloudflare headers: the page itself is refused" "$(curl -s -o /dev/null -w '%{http_code}' -H 'Cf-Ray: abc' $F/)" "403"
ok "Cloudflare headers: DICOMweb is refused" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: portal_session=$V" -H 'CF-Connecting-IP: 1.2.3.4' $F/dicom-web/studies)" "403"
ok "Cloudflare headers: login is refused" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -H 'Cdn-Loop: cloudflare' -d '{"username":"admin","password":"SuperSecret123"}' $F/api/login)" "403"
ok "the health check still answers with Cloudflare headers" "$(curl -s -o /dev/null -w '%{http_code}' -H 'Cf-Ray: abc' $F/healthz)" "200"
for r in "POST /api/login" "GET /api/me" "GET /api/studies" "GET /api/series" "GET /api/studies/$SID/series" "POST /api/upload" "GET /api/studies/$SID/download" "GET /api/series/$SER/download" "GET /api/modalities" "POST /api/studies/$SID/send" "GET /api/transfers" "POST /api/studies/$SID/share" "GET /api/shares" "POST /api/shares/1/revoke" "GET /api/admin/users" "GET /api/admin/audit" "GET /api/admin/settings" "GET /api/admin/nodes" "PUT /api/admin/branding/logo" "GET /api/weasis/link" "GET /api/studies/$SID/thumbnail" "GET /static/app.js"; do set -- $r
  ok "internet listener: $1 $2 does not exist" "$(code $G $A $1 $2 -H "$J" -d '{}' | sed 's/405/404/')" "404"; done
ok "internet listener: the staff cookie opens nothing there (DICOMweb 401)" "$(code $G $V GET /dicom-web/studies)" "401"
ok "internet listener: not signed in, DICOMweb 401" "$(curl -s -o /dev/null -w '%{http_code}' $G/dicom-web/studies)" "401"
ok "internet listener: not signed in, OHIF 401" "$(curl -s -o /dev/null -w '%{http_code}' $G/ohif/viewer)" "401"
ok "internet listener: not signed in, WADO 401" "$(curl -s -o /dev/null -w '%{http_code}' "$G/wado?requestType=WADO&studyUID=$UID_&contentType=image/jpeg")" "401"
ok "internet listener: the invitation page exists" "$(curl -s -o /dev/null -w '%{http_code}' $G/)" "200"
ok "internet listener: the health check is public" "$(curl -s -o /dev/null -w '%{http_code}' $G/healthz)" "200"
ok "internet listener: the portal name and logo are public" "$(curl -s -o /dev/null -w '%{http_code}' $G/api/public)" "200"

echo "== the intranet keeps Weasis and the unrestricted viewer proxy (staff, with accounts)"
ok "features: the browser viewer is offered" "$(body $F $V /api/me | py "d['features']['ohif']")" "True"
ok "Weasis link works for a user with visualize" "$(code $F $V GET '/api/weasis/link?study_uid=1.2.3')" "200"
ok "Weasis link needs visualize (read-only user: 403)" "$(code $F $RD GET '/api/weasis/link?study_uid=1.2.3')" "403"
ok "Weasis is offered by default (features.weasis)" "$(body $F $V /api/me | py "d['features']['weasis']")" "True"
put $A '{"weasis_enabled":false}' > /dev/null
ok "Weasis switched off: no link is made (403)" "$(code $F $V GET '/api/weasis/link?study_uid=1.2.3')" "403"
ok "...and the UI is told (features.weasis)" "$(body $F $V /api/me | py "d['features']['weasis']")" "False"
put $A '{"weasis_enabled":true}' > /dev/null
ok "every user gets the visible modality buttons, in order" "$(body $F $V /api/me | py "','.join(b['label'] for b in d['features']['modality_buttons'])")" "CT,MR,US,RX,MG,XA,RF,NM,PT"
ok "a modality button with a wildcard code is refused (400)" "$(put $A '{"modality_buttons":[{"label":"CT","codes":["C*"]}]}' | tail -1)" "400"
put $A '{"modality_buttons":[{"label":"US","codes":["us"],"title":"Ultrasound"},{"label":"CT","codes":["CT"],"visible":false}]}' > /dev/null
ok "order and hiding reach every user (codes upper-cased)" "$(body $F $V /api/me | py "','.join(b['label']+':'+'+'.join(b['codes']) for b in d['features']['modality_buttons'])")" "US:US"
put $A "{\"modality_buttons\":$(body $F $A /api/admin/settings | py "json.dumps(d['defaults']['modality_buttons'])")}" > /dev/null
ok "...and the defaults can be restored" "$(body $F $V /api/me | py "len(d['features']['modality_buttons'])")" "9"

echo "== appearance: colours, the simplified view and the view of each role"
ok "the colours are public (login and invitation pages)" "$(curl -s $G/api/public | py "sorted(d['theme'])")" "['accent', 'background', 'header', 'mode', 'sidebar']"
ok "a colour that is not #rrggbb is refused (400)" "$(put $A '{"color_accent":"red"}' | tail -1)" "400"
ok "the number of recent studies is limited (400)" "$(put $A '{"simplified_recent_count":500}' | tail -1)" "400"
put $A '{"color_accent":"#AA3311","theme_mode":"dark"}' > /dev/null
ok "saved colours reach the public page (lower-cased)" "$(curl -s $F/api/public | py "d['theme']['accent']+' '+d['theme']['mode']")" "#aa3311 dark"
ok "simplified view switched off: everybody gets the normal view" "$(body $F $V /api/me | py "d['features']['interface']")" "normal"
ROLES=$(body $F $A /api/admin/settings | py "json.dumps([dict(r, interface='simplified' if r['key']=='viewer' else 'normal') for r in d['settings']['roles']])")
put $A "{\"roles\":$ROLES,\"simplified_enabled\":true,\"simplified_recent_count\":8,\"simplified_upload_text\":\"Drop here\"}" > /dev/null
ok "switched on: the viewer role (found by its permissions) gets the simplified view" "$(body $F $V /api/me | py "d['features']['interface']")" "simplified"
ok "...with the home page options" "$(body $F $V /api/me | py "str(d['features']['simplified']['recent_count'])+' '+d['features']['simplified']['upload_text']")" "8 Drop here"
ok "the administrator role keeps the normal view" "$(body $F $A /api/me | py "d['features']['interface']")" "normal"
ok "custom permissions (no role): the default view" "$(body $F $RD /api/me | py "d['features']['interface']")" "normal"
put $A '{"default_interface":"simplified"}' > /dev/null
ok "...which administrators can change" "$(body $F $RD /api/me | py "d['features']['interface']")" "simplified"
VID=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='viewer1'][0]")
ok "the users list shows each user's role and view" "$(body $F $A /api/admin/users | py "[(u['effective_role'], u['interface']) for u in d['users'] if u['username']=='viewer1'][0]")" "('viewer', 'simplified')"
ok "an unknown role is refused (400)" "$(code $F $A PUT /api/admin/users/$VID -H "$J" -d '{"role":"nope"}')" "400"
code $F $A PUT /api/admin/users/$VID -H "$J" -d '{"role":"contributor"}' > /dev/null
ok "the role saved with the account wins over the permissions" "$(body $F $V /api/me | py "d['features']['interface']")" "normal"
code $F $A PUT /api/admin/users/$VID -H "$J" -d '{"role":""}' > /dev/null
ok "switching views needs the switch_view permission" "$(body $F $V /api/me | py "d['features']['interface_switch']")" "False"
code $F $A PUT /api/admin/users/$VID -H "$J" -d '{"permissions":["read","visualize","switch_view"]}' > /dev/null
ok "...with it, the user may switch" "$(body $F $V /api/me | py "d['features']['interface_switch']")" "True"
put $A '{"simplified_enabled":false}' > /dev/null
ok "...but not while the simplified view is switched off" "$(body $F $V /api/me | py "d['features']['interface_switch']")" "False"
put $A '{"simplified_enabled":true}' > /dev/null
code $F $A PUT /api/admin/users/$VID -H "$J" -d '{"permissions":["read","visualize"]}' > /dev/null
ok "the old switch-for-everyone setting is gone (400)" "$(put $A '{"interface_switch":true}' | tail -1)" "400"
mkuser uponly UploadOnlyPass123 '"upload"'
UP=$(login $F uponly UploadOnlyPass123)
ok "total number of studies (from Orthanc's statistics)" "$(body $F $V /api/stats | py "d['studies']")" "1"
ok "...needs the read permission (403)" "$(code $F $UP GET /api/stats)" "403"
body $F $V "/api/studies?limit=8&order=received" > /dev/null
has "recent studies: the last ones received first (Orthanc's LastUpdate)" "$(tail -1 $WORK/find.log)" '"Key": "LastUpdate"'
body $F $V "/api/studies?limit=8" > /dev/null
has "...the search keeps the newest study dates first" "$(tail -1 $WORK/find.log)" '"Key": "StudyDate"'
put $A "{\"roles\":$(body $F $A /api/admin/settings | py "json.dumps(d['defaults']['roles'])"),\"simplified_enabled\":false,\"default_interface\":\"normal\",\"color_accent\":\"\",\"theme_mode\":\"auto\"}" > /dev/null
ok "...and everything goes back to the normal view" "$(body $F $V /api/me | py "d['features']['interface']")" "normal"
T=$(tok $V)
ok "a Weasis token works on the intranet" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" $F/dicom-web/studies)" "200"
ok "...and is useless on the internet listener (401)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $T" $G/dicom-web/studies)" "401"
ok "intranet: whole DICOM files are available to staff (Weasis needs them)" "$(code $F $V GET /dicom-web/studies/1.2.3/series/4.5/instances/6.7)" "200"
W0='/wado?requestType=WADO&studyUID=1&seriesUID=2&objectUID=3'
ok "intranet: WADO-URI DICOM file for staff" "$(code $F $V GET "$W0")" "200"
for path in servers peers info; do ok "DICOMweb proxy blocks /dicom-web/$path" "$(code $F $V GET /dicom-web/$path)" "404"; done
ok "not signed in: intranet DICOMweb 401" "$(curl -s -o /dev/null -w '%{http_code}' $F/dicom-web/studies)" "401"
ok "intranet: the browser viewer page is served to staff" "$(code $F $V GET '/ohif/viewer?StudyInstanceUIDs=1.2.3')" "200"

echo "== sharing needs the public address first (Settings > Share links)"
ok "the UI is told sharing is not set up" "$(body $F $SH /api/me | py "d['features']['share']['configured']")" "False"
OUT=$(mkshare $SH '{}')
has "creating a link before that is refused with an explanation" "$OUT" "Sharing is not set up"
ok "...as a 400" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{}')" "400"
ok "validation: the address must be http(s)" "$(put $A '{"share_public_url":"ftp://pacs.example.com"}' | tail -1)" "400"
ok "validation: no spaces or odd characters in the address" "$(put $A '{"share_public_url":"https://pacs example.com/<x>"}' | tail -1)" "400"
ok "validation: default validity may not exceed the maximum" "$(put $A '{"share_default_hours":100,"share_max_hours":50}' | tail -1)" "400"
ok "validation: validity range is 1-8760 hours" "$(put $A '{"share_max_hours":9000}' | tail -1)" "400"
ok "validation: default downloads may not exceed the maximum" "$(put $A '{"share_default_downloads":5,"share_max_downloads":3}' | tail -1)" "400"
ok "validation: download counts are limited to 1000" "$(put $A '{"share_max_downloads":5000}' | tail -1)" "400"
ok "validation: non-admins cannot change it" "$(code $F $SH PUT /api/admin/settings -H "$J" -d '{"share_public_url":"https://evil.example.com"}')" "403"
ok "the shipped defaults: 48 hours, 1 download" "$(body $F $A /api/admin/settings | py "str(d['settings']['share_default_hours'])+' '+str(d['settings']['share_default_downloads'])")" "48 1"
ok "an administrator enters the public address (trailing slash is trimmed)" "$(put $A '{"share_public_url":"https://pacs.example.com/"}' | tail -1)" "200"
ok "...saved without the slash" "$(body $F $A /api/admin/settings | py "d['settings']['share_public_url']")" "https://pacs.example.com"
ok "the UI is told sharing is ready" "$(body $F $SH /api/me | py "d['features']['share']['configured']")" "True"

echo "== creating invitation links (intranet)"
put $A '{"share_keep_passwords":false}' > /dev/null   # passwords shown only once: only a hash is stored
newlink L1 $SH '{}'
ok "default validity: 48 hours" "$(echo "$L1_JSON" | py "d['hours']")" "48"
ok "default downloads: 1" "$(echo "$L1_JSON" | py "d['max_downloads']")" "1"
ok "it expires in 48 hours from now" "$(echo "$L1_JSON" | py "172700 < d['expires_at']-int(__import__('time').time()) <= 172800")" "True"
ok "the link is <public address>/s/<48 hex characters>" "$(echo "$L1_JSON" | py "bool(__import__('re').fullmatch(r'https://pacs\.example\.com/s/[0-9a-f]{48}', d['url']))")" "True"
ok "the password is 3 groups of 4 readable characters" "$(echo "$L1_PW" | grep -Ec '^[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}$')" "1"
ok "no two links share a token" "$([ "$L1_TOKEN" != "$(mkshare $SH '{}' | py "d['url'].rsplit('/s/',1)[1]")" ] && echo different)" "different"
HASH=$(sqlval "select password_hash from shares where id=$L1_ID")
ok "the password is stored only as an Argon2 hash" "${HASH:0:9}" '$argon2id'
ok "the plain password is nowhere in the database files" "$(python3 - "$WORK" "$L1_PW" <<'PY'
import sys, glob
work, pw = sys.argv[1], sys.argv[2]
data = b"".join(open(f, "rb").read() for f in glob.glob(work + "/portal.db*"))
print("FOUND" if pw.encode() in data or pw.replace("-", "").encode() in data else "absent")
PY
)" "absent"
ok "...and it cannot be shown again (409)" "$(code $F $SH POST /api/shares/$L1_ID/password)" "409"
ok "...nor offered in the list" "$(body $F $SH /api/shares | py "[s['password_available'] for s in d if s['id']==$L1_ID][0]")" "False"

echo "== showing a link's password again (Settings > Share links, on by default)"
put $A '{"share_keep_passwords":true}' > /dev/null
newlink K1 $SH '{}'
ok "the creation says the password is kept" "$(echo "$K1_JSON" | py "d['password_kept']")" "True"
ok "the list offers it, without the password itself" "$(body $F $SH /api/shares | py "[s['password_available'] for s in d if s['id']==$K1_ID][0]")" "True"
hasnt "...the list never contains a password" "$(body $F $A /api/shares)" "$K1_PW"
ok "its creator sees it again" "$(body $F $SH /api/shares/$K1_ID/password -X POST | py "d['password']")" "$K1_PW"
ok "an administrator too" "$(body $F $A /api/shares/$K1_ID/password -X POST | py "d['password']")" "$K1_PW"
ok "another person with the share permission cannot (403)" "$(code $F $SH2 POST /api/shares/$K1_ID/password)" "403"
ok "a user without the share permission cannot (403)" "$(code $F $V POST /api/shares/$K1_ID/password)" "403"
ok "it needs the CSRF header (403)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: portal_session=$SH" $F/api/shares/$K1_ID/password)" "403"
ok "it is not on the internet listener (404)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" -H "Cookie: portal_session=$SH" $G/api/shares/$K1_ID/password | sed 's/405/404/')" "404"
has "every time is in the audit log" "$(body $F $A '/api/admin/audit?limit=20')" "share_password_shown"
ok "the kept password still opens the link" "$(unlock_code $K1_TOKEN "$K1_PW")" "200"
code $F $SH POST /api/shares/$K1_ID/revoke > /dev/null
ok "revoked: it cannot be shown any more (409)" "$(code $F $SH POST /api/shares/$K1_ID/password)" "409"
ok "...and the copy is erased from the database" "$(sqlval "select password_copy is null from shares where id=$K1_ID")" "1"
newlink K2 $SH '{}'
OUT=$(put $A '{"share_keep_passwords":false}')
has "switching it off erases every kept copy" "$OUT" "kept share link passwords were erased"
ok "...and nothing can be shown (409)" "$(code $F $SH POST /api/shares/$K2_ID/password)" "409"
put $A '{"share_keep_passwords":true}' > /dev/null
ok "...not even when switched back on: the copy is gone (409)" "$(code $F $SH POST /api/shares/$K2_ID/password)" "409"
NEWPW=$(body $F $SH /api/shares/$K2_ID -X PUT -H "$J" -d '{"new_password":true}' | py "d['password']")
ok "...but the link can be given a new password, kept from then on" "$(body $F $SH /api/shares/$K2_ID/password -X POST | py "d['password']")" "$NEWPW"
ok "the old password no longer opens it" "$(unlock_code $K2_TOKEN "$K2_PW")" "400"
ok "the new one does" "$(unlock_code $K2_TOKEN "$NEWPW")" "200"

echo "== changing a link: more time, more downloads, the note"
newlink E1 $SH '{"hours":2,"downloads":1,"note":"first"}'
ok "more time: 100 hours from now" "$(code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"hours":100}')" "200"
ok "...is saved" "$(body $F $SH /api/shares | py "[round((s['expires_at']-__import__('time').time())/3600) for s in d if s['id']==$E1_ID][0]")" "100"
ok "more downloads: 5 left from now on" "$(code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"downloads_left":5}')" "200"
ok "...is saved (used + left)" "$(body $F $SH /api/shares | py "[s['max_downloads']-s['downloads_used'] for s in d if s['id']==$E1_ID][0]")" "5"
code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"note":"second opinion"}' > /dev/null
ok "the note" "$(body $F $SH /api/shares | py "[s['note'] for s in d if s['id']==$E1_ID][0]")" "second opinion"
ok "the password is unchanged by that" "$(unlock_code $E1_TOKEN "$E1_PW")" "200"
ok "beyond the maximum validity (400)" "$(code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"hours":721}')" "400"
ok "beyond the maximum downloads (400)" "$(code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"downloads_left":21}')" "400"
ok "another person with the share permission cannot (403)" "$(code $F $SH2 PUT /api/shares/$E1_ID -H "$J" -d '{"hours":5}')" "403"
ok "an administrator can" "$(code $F $A PUT /api/shares/$E1_ID -H "$J" -d '{"hours":5}')" "200"
has "it is in the audit log" "$(body $F $A '/api/admin/audit?limit=10')" "share_updated"
code $F $SH POST /api/shares/$E1_ID/revoke > /dev/null
ok "a revoked link cannot be changed (409)" "$(code $F $SH PUT /api/shares/$E1_ID -H "$J" -d '{"hours":5}')" "409"
ok "the time can be given in minutes (2 days 3 hours 20 minutes)" "$(mkshare $SH '{"minutes":3080}' | py "d['minutes']")" "3080"
ok "...at least 1 minute (400)" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"minutes":0}')" "400"
ok "...at most the maximum, 30 days (400)" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"minutes":43201}')" "400"

echo "== sending share invitations by email (Settings > Email; tests/mock_smtp.py keeps what it receives)"
# lastmail -> the text of the newest email the fake mail server received
lastmail() { python3 - "$WORK/mail" <<'PY'
import sys, glob
from email import message_from_bytes, policy
newest = sorted(glob.glob(sys.argv[1] + "/*.eml"))[-1]
print(message_from_bytes(open(newest, "rb").read(), policy=policy.default).get_body(("plain",)).get_content())
PY
}
ok "no mail server: the Share window is told" "$(body $F $SH /api/me | py "d['features']['share']['email']")" "False"
ok "...and asking for an email is refused (400)" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"email":{"to":"guest@example.org"}}')" "400"
ok "a mail server needs a sender (400)" "$(put $A '{"smtp_host":"127.0.0.1"}' | tail -1)" "400"
put $A '{"smtp_host":"127.0.0.1","smtp_port":18025,"smtp_security":"none","smtp_from":"Demo PACS <pacs@example.com>","smtp_username":"mailer","smtp_password":"MailPass123","email_footer":"1 High Street"}' > /dev/null
ok "the mail server password is never sent back" "$(body $F $A /api/admin/settings | py "('smtp_password' in d['settings'], d['settings']['smtp_password_set'])")" "(False, True)"
ok "now the Share window offers email" "$(body $F $SH /api/me | py "d['features']['share']['email']")" "True"
ok "a test email (administrators)" "$(body $F $A /api/admin/email/test -X POST -H "$J" -d '{"to":"admin@example.org"}' | py "d['ok']")" "True"
has "...arrives" "$(tail -1 $WORK/mail/mail.log)" "test email"
ok "...not for others (403)" "$(code $F $SH POST /api/admin/email/test -H "$J" -d '{"to":"a@example.org"}')" "403"
BEFORE=$(body $F $SH /api/shares | py "len(d)")
ok "a bad address is refused (400)" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"email":{"to":"not an address"}}')" "400"
ok "...before anything is created" "$(body $F $SH /api/shares | py "len(d)")" "$BEFORE"
M1=$(mkshare $SH '{"minutes":3080,"downloads":2,"email":{"to":"Dr Who <who@example.org>","message":"Second opinion please"}}')
ok "created and emailed" "$(echo "$M1" | py "(d['email']['sent'], d['email']['to'])")" "(True, 'who@example.org')"
MAIL=$(lastmail)
has "the email has the link" "$MAIL" "$(echo "$M1" | py "d['url']")"
has "...how long it works" "$MAIL" "2 days 3 hours 20 minutes"
has "...the downloads" "$MAIL" "download it 2 times"
has "...the message" "$MAIL" "Second opinion please"
has "...the text of Settings > Email" "$MAIL" "1 High Street"
hasnt "...and not the password" "$MAIL" "$(echo "$M1" | py "d['password']")"
has "...which it says comes separately" "$MAIL" "given to you separately"
has "the recipient is right" "$(tail -1 $WORK/mail/mail.log)" "who@example.org"
M2=$(mkshare $SH '{"email":{"to":"a@example.org","include_password":true}}')
M2PW=$(echo "$M2" | py "d['password']"); M2ID=$(echo "$M2" | py "d['id']")
has "the password only when asked for" "$(lastmail)" "Password: $M2PW"
ok "editing a link and emailing the update" "$(body $F $SH /api/shares/$M2ID -X PUT -H "$J" -d '{"minutes":90,"email":{"to":"a@example.org","include_password":true}}' | py "d['email']['sent']")" "True"
MAIL=$(lastmail)
has "...it says it was updated" "$MAIL" "has updated your link"
has "...with the new time" "$MAIL" "1 hour 30 minutes"
has "...and the kept password" "$MAIL" "Password: $M2PW"
has "every email is in the audit log" "$(body $F $A '/api/admin/audit?limit=20')" "share_emailed"
put $A '{"smtp_port":18026}' > /dev/null   # nothing listens there
M3=$(mkshare $SH '{"email":{"to":"b@example.org"}}')
ok "mail server down: the link is still created, the answer says the email was not sent" "$(echo "$M3" | py "(bool(d['url']), d['email']['sent'], bool(d['email']['error']))")" "(True, False, True)"
put $A '{"smtp_host":""}' > /dev/null
ok "a user without the share permission cannot create links (403)" "$(code $F $V POST /api/studies/$SID/share -H "$J" -d '{}')" "403"
ok "a read-only user cannot either" "$(code $F $RD POST /api/studies/$SID/share -H "$J" -d '{}')" "403"
ok "not signed in: 401" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" -H "$J" -d '{}' $F/api/studies/$SID/share)" "401"
ok "validity: 0 hours -> 400" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"hours":0}')" "400"
ok "validity above the maximum (720 h) -> 400" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"hours":721}')" "400"
ok "downloads above the maximum (20) -> 400" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"downloads":21}')" "400"
ok "an unknown study -> 404" "$(code $F $SH POST /api/studies/ffffffff-ffffffff-ffffffff-ffffffff-ffffffff/share -H "$J" -d '{}')" "404"
ok "an invalid study id -> 400" "$(code $F $SH POST '/api/studies/..%2Fsystem/share' -H "$J" -d '{}')" "400"
ok "creating a link needs the CSRF header (403)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: portal_session=$SH" -H "$J" -d '{}' $F/api/studies/$SID/share)" "403"
ok "creator sees their links" "$(body $F $SH /api/shares | py "len(d) >= 2")" "True"
ok "another sharer does not see them" "$(body $F $SH2 /api/shares | py "len(d)")" "0"
ok "an administrator sees everyone's" "$(body $F $A /api/shares | py "len(d) >= 2")" "True"
ok "the list gives the link but never the password" "$(body $F $SH /api/shares | py "any('password' in s for s in d) or any(v in json.dumps(d) for v in ('$L1_PW', '$L1_PW'.replace('-', '')))")" "False"
ok "a status is shown for each link" "$(body $F $SH /api/shares | py "d[-1]['status']")" "active"
has "the audit log records who shared what" "$(body $F $A '/api/admin/audit?limit=300')" "share_created"
hasnt "...but never the password" "$(body $F $A '/api/admin/audit?limit=300')" "$L1_PW"

echo "== a guest opens the link: the password comes first"
PAGE=$(curl -s $G/s/$L1_TOKEN)
has "the invitation page is served on the internet listener" "$PAGE" "<title>"
has "...and tells search engines to stay away" "$PAGE" "noindex"
hasnt "...with no patient data in it" "$PAGE" "DOE"
has "...and a strict content policy" "$(curl -sI $G/s/$L1_TOKEN)" "script-src 'self'"
ok "the same page for any address (the script decides what to show)" "$(curl -s -o /dev/null -w '%{http_code}' $G/s/not-a-token)" "200"
ok "status before the password: locked" "$(curl -s $G/api/share/$L1_TOKEN/status | py "d['state']")" "locked"
ok "status of an unknown link: unavailable" "$(curl -s $G/api/share/$(printf 'a%.0s' $(seq 1 48))/status | py "d['state']")" "unavailable"
ok "status of a malformed link: unavailable" "$(curl -s $G/api/share/zzz/status | py "d['state']")" "unavailable"
ok "study details without the password: 401" "$(gcode '' GET /api/share/$L1_TOKEN/study)" "401"
ok "download without the password: 401" "$(gcode '' GET /api/share/$L1_TOKEN/download)" "401"
ok "a wrong password: 400" "$(unlock_code $L1_TOKEN WRNG-WRNG-WRNG -H 'CF-Connecting-IP: 10.1.1.1')" "400"
ok "an empty password: 400" "$(unlock_code $L1_TOKEN '' -H 'CF-Connecting-IP: 10.1.1.2')" "400"
ok "unlocking needs the CSRF header (403)" "$(curl -s -o /dev/null -w '%{http_code}' -H "$J" -d "{\"password\":\"$L1_PW\"}" $G/api/share/$L1_TOKEN/unlock)" "403"
ok "an unknown link cannot be unlocked (400, same answer as a wrong password)" "$(unlock_code $(printf 'b%.0s' $(seq 1 48)) "$L1_PW" -H 'CF-Connecting-IP: 10.1.1.3')" "400"
GC=$(unlock $L1_TOKEN "$(echo "$L1_PW" | tr 'A-Z' 'a-z' | tr -d '-')")
ok "the password is accepted typed in lower case, without dashes" "$([ -n "$GC" ] && echo cookie)" "cookie"
HDR=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"password\":\"$L1_PW\"}" $G/api/share/$L1_TOKEN/unlock)
has "the guest cookie is HttpOnly" "$HDR" "HttpOnly"
has "...SameSite" "$HDR" "SameSite=Lax"
has "...and lasts at most 12 hours" "$HDR" "Max-Age=43200"
ok "status after the password: open" "$(gbody $GC /api/share/$L1_TOKEN/status | py "d['state']")" "open"
ok "the cookie of this link does not open another one" "$(gcode $GC GET /api/share/$(printf 'c%.0s' $(seq 1 48))/study)" "401"
ok "a made-up cookie opens nothing" "$(gcode deadbeefdeadbeefdeadbeefdeadbeef GET /api/share/$L1_TOKEN/study)" "401"
ok "the guest cookie is no staff session on the intranet" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: share_session=$GC" $F/api/me)" "401"
ok "...nor for the intranet viewer proxy" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: share_session=$GC" $F/dicom-web/studies)" "401"

echo "== what the guest sees: study, series, countdown, downloads left"
ST=$(gbody $GC /api/share/$L1_TOKEN/study)
ok "patient name" "$(echo "$ST" | py "d['patient']['name']")" "DOE^JOHN"
ok "patient id" "$(echo "$ST" | py "d['patient']['id']")" "P-001"
ok "study date and description" "$(echo "$ST" | py "d['study']['date']+' '+d['study']['description'][:8]")" "20240517 CT CHEST"
ok "modalities of the study" "$(echo "$ST" | py "','.join(d['study']['modalities'])")" "CT,SR"
ok "two series, in series-number order, with their image counts" "$(echo "$ST" | py "';'.join(s['modality']+':'+str(s['images']) for s in d['series'])")" "CT:3;SR:5"
ok "series details: number, description, body part" "$(echo "$ST" | py "d['series'][0]['number']+' '+d['series'][0]['description']+' '+d['series'][0]['body_part']")" "2 Chest 1mm CHEST"
ok "the server's clock is given, so the countdown does not depend on the guest's clock" "$(echo "$ST" | py "abs(d['now']-int(__import__('time').time())) < 5")" "True"
ok "remaining time is about 48 hours" "$(echo "$ST" | py "172700 < d['expires_at']-d['now'] <= 172800")" "True"
ok "downloads: 1 allowed, 0 used, 1 left" "$(echo "$ST" | py "str(d['downloads']['max'])+'/'+str(d['downloads']['used'])+'/'+str(d['downloads']['left'])")" "1/0/1"
ok "the online viewer is available and points at this study" "$(echo "$ST" | py "str(d['viewer']['available'])+' '+d['viewer']['url']")" "True /ohif/viewer?StudyInstanceUIDs=$UID_"
hasnt "no Orthanc internals are exposed (ids, paths)" "$ST" "aaaaaaaa-bbbbbbbb"
has "guest API answers are never cached" "$(curl -sI -H "Cookie: share_session=$GC" $G/api/share/$L1_TOKEN/study)" "cache-control: no-store"
SOON=$((($(date +%s))+90)); sql "update shares set expires_at=$SOON where id=$L1_ID"
ok "the countdown follows the database: about 90 seconds left" "$(gbody $GC /api/share/$L1_TOKEN/study | py "80 <= d['expires_at']-d['now'] <= 90")" "True"
sql "update shares set expires_at=$(($(date +%s)+172800)) where id=$L1_ID"

echo "== the viewer: the guest sees ONLY the shared study, and never whole DICOM files"
D="/dicom-web"
ok "OHIF page is served with the guest cookie" "$(gcode $GC GET "/ohif/viewer?StudyInstanceUIDs=$UID_")" "200"
ok "OHIF assets too" "$(gcode $GC GET /ohif/app.js)" "200"
ok "a search narrowed to the shared study" "$(gcode $GC GET "$D/studies?StudyInstanceUID=$UID_")" "200"
ok "...also with the DICOM tag form" "$(gcode $GC GET "$D/studies?0020000D=$UID_")" "200"
ok "a series search narrowed to the shared study" "$(gcode $GC GET "$D/series?StudyInstanceUID=$UID_")" "200"
ok "an unfiltered study search would list the whole archive: 403" "$(gcode $GC GET "$D/studies?limit=1")" "403"
ok "an unfiltered series search: 403" "$(gcode $GC GET "$D/series?limit=5")" "403"
ok "an unfiltered instance search: 403" "$(gcode $GC GET "$D/instances?limit=5")" "403"
ok "a search for ANOTHER study: 403" "$(gcode $GC GET "$D/studies?StudyInstanceUID=9.9.9")" "403"
ok "a search naming the shared study AND another: 403" "$(gcode $GC GET "$D/studies?StudyInstanceUID=$UID_&StudyInstanceUID=9.9.9")" "403"
for path in series metadata instances series/4.5/instances series/4.5/metadata series/4.5/instances/6.7/metadata series/4.5/instances/6.7/frames/1 series/4.5/instances/6.7/rendered series/4.5/instances/6.7/thumbnail; do
  ok "guest: /dicom-web/studies/<shared>/$path (what a viewer needs) -> 200" "$(gcode $GC GET "$D/studies/$UID_/$path")" "200"; done
for path in "studies/$UID_" "studies/$UID_/series/4.5" "studies/$UID_/series/4.5/instances/6.7" "series/4.5" "instances/6.7"; do
  ok "guest: /dicom-web/$path (a whole DICOM file) -> 403" "$(gcode $GC GET "$D/$path")" "403"; done
for path in "studies/9.9.9/series" "studies/9.9.9/metadata" "studies/9.9.9/series/4.5/instances/6.7/frames/1" "studies/9.9.9/series/4.5/instances/6.7/rendered"; do
  ok "guest: another study ($path) -> 403" "$(gcode $GC GET "$D/$path")" "403"; done
ok "guest: the DICOMweb plugin's other resources stay blocked (404)" "$(gcode $GC GET "$D/servers")" "404"
has "the refusal explains itself" "$(gbody $GC "$D/studies/$UID_")" "only gives access to the shared study"
W="/wado?requestType=WADO&studyUID=$UID_&seriesUID=2&objectUID=3"
ok "guest: WADO-URI without a picture type (= a DICOM file) -> 403" "$(gcode $GC GET "$W")" "403"
ok "guest: WADO-URI asking for application/dicom -> 403" "$(gcode $GC GET "$W&contentType=application/dicom")" "403"
ok "guest: WADO-URI asking for a JPEG of the shared study -> 200" "$(gcode $GC GET "$W&contentType=image/jpeg")" "200"
ok "guest: ...URL-encoded type -> 200" "$(gcode $GC GET "$W&contentType=image%2Fjpeg")" "200"
ok "guest: a list mixing picture and DICOM -> 403" "$(gcode $GC GET "$W&contentType=image/jpeg,application/dicom")" "403"
ok "guest: a second contentType smuggling DICOM -> 403" "$(gcode $GC GET "$W&contentType=image/jpeg&contentType=application/dicom")" "403"
ok "guest: a picture of ANOTHER study -> 403" "$(gcode $GC GET "/wado?requestType=WADO&studyUID=9.9.9&seriesUID=2&objectUID=3&contentType=image/jpeg")" "403"
hasnt "the guest's cookie is never forwarded to Orthanc" "$(cat proxy.log)" "share_session"
hasnt "...nor any staff cookie" "$(cat proxy.log)" "portal_session"

echo "== downloading the study as a ZIP: counted, and the last one is the last one"
curl -s -D d1.h -o d1.zip -H "Cookie: share_session=$GC" -H "$H" $G/api/share/$L1_TOKEN/download
has "the first download is a ZIP" "$(cat d1.h)" "application/zip"
has "...named after the patient and the study date" "$(cat d1.h)" "P-001_20240517.zip"
ok "...and it has content" "$([ $(stat -c %s d1.zip) -gt 1000 ] && echo yes)" "yes"
ok "downloads now: 1 used, 0 left" "$(gbody $GC /api/share/$L1_TOKEN/study | py "str(d['downloads']['used'])+'/'+str(d['downloads']['left'])")" "1/0"
OUT=$(gbody $GC /api/share/$L1_TOKEN/download -w ' [%{http_code}]')
has "the second download is refused (403)" "$OUT" "\[403\]"
has "...and the guest is told they can still look at the study online" "$OUT" "still look at the study online"
ok "downloads stay at 1 used" "$(gbody $GC /api/share/$L1_TOKEN/study | py "d['downloads']['used']")" "1"
ok "running out of downloads does NOT stop the details" "$(gcode $GC GET /api/share/$L1_TOKEN/study)" "200"
ok "...nor the browser viewer" "$(gcode $GC GET "/ohif/viewer?StudyInstanceUIDs=$UID_")" "200"
ok "...nor the viewer's data" "$(gcode $GC GET "$D/studies/$UID_/metadata")" "200"
ok "...nor the viewer's pictures" "$(gcode $GC GET "$W&contentType=image/jpeg")" "200"
has "the audit log records the guest's download" "$(body $F $A '/api/admin/audit?limit=300')" "share_download"
ok "the staff list shows 1 of 1 downloads used and the visits" "$(body $F $SH /api/shares | py "[ str(s['downloads_used'])+'/'+str(s['max_downloads'])+'/'+str(s['access_count']>=2) for s in d if s['id']==$L1_ID][0]")" "1/1/True"

newlink L2 $SH '{"hours":24,"downloads":0,"note":"view only for Dr. Who"}'
ok "a link may be view-only (0 downloads), valid 24 hours" "$(echo "$L2_JSON" | py "str(d['hours'])+'/'+str(d['max_downloads'])")" "24/0"
G2=$(unlock $L2_TOKEN $L2_PW)
OUT=$(gbody $G2 /api/share/$L2_TOKEN/download -w ' [%{http_code}]')
has "download of a view-only link: 403" "$OUT" "\[403\]"
has "...explained" "$OUT" "not enabled"
ok "details show 0 downloads" "$(gbody $G2 /api/share/$L2_TOKEN/study | py "str(d['downloads']['max'])+'/'+str(d['downloads']['left'])")" "0/0"
ok "viewing works" "$(gcode $G2 GET "$D/studies/$UID_/metadata")" "200"
ok "the note is kept for the staff list" "$(body $F $SH /api/shares | py "[s['note'] for s in d if s['id']==$L2_ID][0]")" "view only for Dr. Who"

newlink L3 $SH '{"downloads":3}'
G3=$(unlock $L3_TOKEN $L3_PW); rm -f par*.out
# (a subshell: a bare `wait` would also wait for the fake Orthanc and the portal started by this script)
( for i in 1 2 3 4 5 6 7 8; do curl -s -o /dev/null -w "%{http_code}\n" -H "Cookie: share_session=$G3" -H "$H" $G/api/share/$L3_TOKEN/download > par$i.out & done; wait )
ok "8 simultaneous downloads of a link that allows 3: exactly 3 succeed" "$(cat par*.out | grep -c '^200$')" "3"
ok "...the other 5 are refused" "$(cat par*.out | grep -c '^403$')" "5"
ok "the counter is exactly 3" "$(gbody $G3 /api/share/$L3_TOKEN/study | py "str(d['downloads']['used'])+'/'+str(d['downloads']['left'])")" "3/0"

echo "== a guest can leave: Lock ends the session"
GX=$(unlock $L3_TOKEN $L3_PW)
ok "a fresh session works" "$(gcode $GX GET /api/share/$L3_TOKEN/study)" "200"
ok "locking" "$(gcode $GX POST /api/share/$L3_TOKEN/lock)" "200"
ok "...ends that session (401)" "$(gcode $GX GET /api/share/$L3_TOKEN/study)" "401"
ok "...and its viewer access" "$(gcode $GX GET "$D/studies/$UID_/metadata")" "401"
ok "the other session of the same link is not affected" "$(gcode $G3 GET /api/share/$L3_TOKEN/study)" "200"

echo "== guessing is throttled"
newlink L7 $SH '{}'
for i in 1 2 3 4 5; do unlock_code $L7_TOKEN "BAD$i-BAD$i-BAD$i" -H "CF-Connecting-IP: 10.2.0.$i" > /dev/null; done
ok "5 wrong passwords lock THAT link, even for the right password (429)" "$(unlock_code $L7_TOKEN $L7_PW -H 'CF-Connecting-IP: 10.2.0.99')" "429"
ok "other links are not affected" "$(unlock_code $L1_TOKEN $L1_PW -H 'CF-Connecting-IP: 10.2.0.99')" "200"
for i in $(seq 1 20); do unlock_code $(printf '%048x' $i) whatever -H 'CF-Connecting-IP: 7.7.7.7' > /dev/null; done
ok "an address that fails 20 times (guessing links) is locked out (429)" "$(unlock_code $L1_TOKEN $L1_PW -H 'CF-Connecting-IP: 7.7.7.7')" "429"
ok "another address is not affected" "$(unlock_code $L1_TOKEN $L1_PW -H 'CF-Connecting-IP: 8.8.8.8')" "200"
has "failed guesses are in the audit log" "$(body $F $A '/api/admin/audit?limit=300')" "share_unlock_failed"

echo "== when the time is up everything stops"
newlink L4 $SH '{"hours":1}'
ok "the shortest validity is 1 hour" "$(echo "$L4_JSON" | py "3500 < d['expires_at']-int(__import__('time').time()) <= 3600")" "True"
G4=$(unlock $L4_TOKEN $L4_PW)
ok "before the time is up it works" "$(gcode $G4 GET /api/share/$L4_TOKEN/study)" "200"
sql "update shares set expires_at=$(date +%s)-5 where id=$L4_ID"
ok "status: expired" "$(curl -s $G/api/share/$L4_TOKEN/status | py "d['state']")" "expired"
ok "details: 410 Gone" "$(gcode $G4 GET /api/share/$L4_TOKEN/study)" "410"
ok "download: 410" "$(gcode $G4 GET /api/share/$L4_TOKEN/download)" "410"
ok "the viewer's data: 410" "$(gcode $G4 GET "$D/studies/$UID_/metadata")" "410"
ok "the browser viewer: 410" "$(gcode $G4 GET "/ohif/viewer?StudyInstanceUIDs=$UID_")" "410"
ok "unlocking again: 410" "$(unlock_code $L4_TOKEN $L4_PW -H 'CF-Connecting-IP: 10.3.0.1')" "410"
ok "the staff list shows it as expired" "$(body $F $SH /api/shares | py "[s['status'] for s in d if s['id']==$L4_ID][0]")" "expired"

echo "== revoking a link"
newlink L5 $SH '{}'; newlink L6 $SH '{}'
G5=$(unlock $L5_TOKEN $L5_PW); G6=$(unlock $L6_TOKEN $L6_PW)
ok "another person with the share permission cannot revoke it (403)" "$(code $F $SH2 POST /api/shares/$L5_ID/revoke)" "403"
ok "a user without the share permission cannot (403)" "$(code $F $V POST /api/shares/$L5_ID/revoke)" "403"
ok "revoking needs the CSRF header (403)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: portal_session=$SH" $F/api/shares/$L5_ID/revoke)" "403"
ok "an unknown link -> 404" "$(code $F $SH POST /api/shares/999999/revoke)" "404"
ok "the guest still works until it is revoked" "$(gcode $G5 GET /api/share/$L5_TOKEN/study)" "200"
ok "the creator revokes their link" "$(code $F $SH POST /api/shares/$L5_ID/revoke)" "200"
ok "a guest who is looking at it is cut off at once (410)" "$(gcode $G5 GET /api/share/$L5_TOKEN/study)" "410"
ok "...also the viewer's data" "$(gcode $G5 GET "$D/studies/$UID_/metadata")" "410"
ok "status: unavailable" "$(curl -s $G/api/share/$L5_TOKEN/status | py "d['state']")" "unavailable"
ok "unlocking: 410" "$(unlock_code $L5_TOKEN $L5_PW -H 'CF-Connecting-IP: 10.4.0.1')" "410"
ok "an administrator can revoke someone else's link" "$(code $F $A POST /api/shares/$L6_ID/revoke)" "200"
ok "...and that guest is cut off" "$(gcode $G6 GET /api/share/$L6_TOKEN/study)" "410"
ok "the staff list shows both as revoked" "$(body $F $SH /api/shares | py "sorted(s['status'] for s in d if s['id'] in ($L5_ID,$L6_ID))")" "['revoked', 'revoked']"
has "the audit log records the revocations" "$(body $F $A '/api/admin/audit?limit=300')" "share_revoked"
ok "the other links were not touched" "$(gcode $GC GET /api/share/$L1_TOKEN/study)" "200"

echo "== a link can be limited by the administrator"
put $A '{"share_max_hours":24,"share_default_hours":12,"share_max_downloads":2,"share_default_downloads":2}' > /dev/null
ok "the new maximums are applied at once (48 h now refused)" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"hours":48}')" "400"
ok "...3 downloads refused" "$(code $F $SH POST /api/studies/$SID/share -H "$J" -d '{"downloads":3}')" "400"
ok "...and the new defaults are used" "$(mkshare $SH '{}' | py "str(d['hours'])+'/'+str(d['max_downloads'])")" "12/2"
ok "the UI is told the new limits" "$(body $F $SH /api/me | py "str(d['features']['share']['max_hours'])+'/'+str(d['features']['share']['default_downloads'])")" "24/2"
put $A '{"share_max_hours":720,"share_default_hours":48,"share_max_downloads":20,"share_default_downloads":1}' > /dev/null

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
ok "the internet listener shows the same logo" "$(curl -s $G/api/public | py "d['logo']['custom']")" "True"
has "the invitation page carries the configured window title" "$(curl -s $G/s/$L1_TOKEN)" "<title>Sunrise Radiology</title>"
has "...and the custom icon" "$(curl -s $G/s/$L1_TOKEN)" 'href="/branding/favicon?v='

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
# (other client addresses are simulated with other loopback source addresses: 127.0.0.x)
for i in $(seq 1 20); do lc "user$i" wrong$i --interface 127.0.0.7 > /dev/null; done
ok "an address that fails 20 times with 20 names is locked (429)" "$(lc admin SuperSecret123 --interface 127.0.0.7)" "429"
ok "another address is not affected" "$(lc admin SuperSecret123 --interface 127.0.0.8)" "200"
for i in 1 2 3 4 5; do lc victim wrong$i --interface 127.0.0.$((20+i)) > /dev/null; done
ok "an account is locked after 5 failures from different addresses" "$(lc victim wrong6 --interface 127.0.0.29)" "429"
FIRST=""; THIRD=""; LAST=""
for i in $(seq 1 22); do C=$(login $F capuser CapUserPass123); [ $i -eq 1 ] && FIRST=$C; [ $i -eq 3 ] && THIRD=$C; LAST=$C; done
ok "sessions per user are capped: the oldest is dropped" "$(code $F $FIRST GET /api/me)/$(code $F $THIRD GET /api/me)/$(code $F $LAST GET /api/me)" "401/200/200"
ok "Weasis links reuse the user's token" "$(tok $V)" "$(tok $V)"
curl -s -D js.h -o /dev/null $F/static/app.js; E=$(grep -i '^etag' js.h | sed 's/[^"]*\("[^"]*"\).*/\1/')
ok "static files: matching ETag -> 304" "$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $E" $F/static/app.js)" "304"
has "Permissions-Policy header" "$(curl -sI $F/)" "permissions-policy"
has "the page may show the thumbnails it loads (CSP img-src allows blob:)" "$(curl -sI $F/ | grep -i content-security-policy)" "img-src 'self' data: blob:"
has "API answers are no-store" "$(curl -sI -H "Cookie: portal_session=$A" $F/api/me)" "cache-control: no-store"
has "HSTS only over HTTPS" "$(curl -sI -H 'X-Forwarded-Proto: https' $F/)" "strict-transport-security"
if on_windows; then skip "database file is private (600)" "Unix file permissions do not exist on Windows; the portal sets them on Linux and macOS"
else ok "database file is private (600)" "$(stat -c %a $WORK/portal.db)" "600"; fi

echo "== Orthanc with and without an account"
# Orthanc may be protected by users (HTTP Basic) or have NO users and only accept a list of allowed
# clients / IPs. The portal must work with both, tell the administrator which one it is using, and
# explain a refusal in the words that fit that setup. The fake Orthanc switches between the setups.
otest() { body $F $A /api/admin/orthanc/test -X POST -H "$J" -d "${1:-{\}}"; }
sview() { body $F $A /api/admin/settings | py "$1"; }
# What the fake Orthanc received since the last mark_auth: any request? how many with an Authorization
# header? how many with Basic credentials? (The log is never emptied: on Windows a write still in
# progress could then land past the new end of the file. Counting lines from a mark avoids that race.)
mark_auth() { AUTH_FROM=$(wc -l < "$WORK/auth.log"); }
sent()  { python3 -c "import json,sys;L=[json.loads(l) for l in open(sys.argv[1]).readlines()[int(sys.argv[2]):] if l.strip()];print(len(L)>0, sum(1 for r in L if r['authorization']), sum(1 for r in L if (r['authorization'] or '').startswith('Basic ')))" "$WORK/auth.log" "${AUTH_FROM:-0}"; }

T=$(otest)
has "with an account: the test says which one it used" "$(echo "$T" | py "d['message']")" "with the account 'orthanc'"
ok "...and reports it" "$(echo "$T" | py "d['account']")" "orthanc"
ok "the saved account has a password" "$(sview "d['settings']['orthanc_password_set']")" "True"

# --- the portal is switched to "no account" while Orthanc still wants one
ok "switching to 'no account' is accepted" "$(put $A '{"orthanc_username":"","clear_orthanc_password":true}' | tail -1)" "200"
ok "...the user name is empty" "$(sview "repr(d['settings']['orthanc_username'])")" "''"
ok "...and no password is kept" "$(sview "d['settings']['orthanc_password_set']")" "False"
ok "Orthanc wants an account and the portal has none: 502" "$(code $F $V GET /api/studies)" "502"
has "...the message says the portal connects without an account" "$(body $F $V /api/studies)" "set to connect without an account"
T=$(otest)
ok "Test connection: refused" "$(echo "$T" | py "d['ok']")" "False"
has "...and advises entering the account" "$(echo "$T" | py "d['message']")" "Enter the account"

# --- an Orthanc without users, which accepts this machine
mock_auth none; mark_auth
ok "Orthanc without users: the study list works" "$(code $F $V GET /api/studies)" "200"
ok "...a study's series" "$(code $F $V GET /api/studies/$SID/series)" "200"
ok "...a ZIP download" "$(code $F $A GET /api/studies/$SID/download)" "200"
ok "...a thumbnail" "$(code $F $V GET /api/studies/$SID/thumbnail)" "200"
ok "...the viewer proxy (DICOMweb)" "$(code $F $V GET /dicom-web/studies)" "200"
ok "...sending to a DICOM node" "$(code $F $S POST /api/studies/$SID/send -H "$J" -d '{"target":"ris"}')" "200"
ok "no Authorization header is ever sent without an account" "$(sent)" "True 0 0"
T=$(otest)
ok "Test connection works without an account" "$(echo "$T" | py "str(d['ok'])+' '+str(d['account'])")" "True None"
has "...and says so" "$(echo "$T" | py "d['message']")" "without an account"

# --- an Orthanc without users that does NOT accept this machine
mock_auth deny
ok "machine not on Orthanc's allowed list: 502" "$(code $F $V GET /api/studies)" "502"
has "...the message points at the allowed list" "$(body $F $V /api/studies)" "allowed"
has "...and at the address used from Docker" "$(body $F $V /api/studies)" "Docker"
ok "...the viewer proxy refuses too (502)" "$(code $F $V GET /dicom-web/studies)" "502"
T=$(otest)
has "Test connection: Orthanc refuses this machine" "$(echo "$T" | py "d['message']")" "refuses this machine"
ok "...not ok" "$(echo "$T" | py "d['ok']")" "False"

# --- an account is configured: credentials are sent, whatever Orthanc does with them
put $A '{"orthanc_username":"orthanc","orthanc_password":"s3cr3tpw"}' > /dev/null
T=$(otest)
has "account configured but Orthanc still forbids: the message names the account" "$(echo "$T" | py "d['message']")" "does not allow the account 'orthanc'"
mock_auth basic; mark_auth
ok "Orthanc with users and the right account: works" "$(code $F $V GET /api/studies)" "200"
ok "...every request carried the account's credentials" "$(sent | awk '{print ($1=="True" && $2==$3 && $2>0) ? "yes" : "no"}')" "yes"
mock_auth none; mark_auth
ok "an account is sent even to an Orthanc that ignores it" "$(code $F $V GET /api/studies)/$(sent | awk '{print ($1=="True" && $2==$3) ? "sent" : "missing"}')" "200/sent"
mock_auth basic
put $A '{"orthanc_password":"wrong-password"}' > /dev/null
T=$(otest)
has "a wrong password is reported as such" "$(echo "$T" | py "d['message']")" "refused the account 'orthanc'"
ok "...the portal then answers 502" "$(code $F $V GET /api/studies)" "502"
put $A '{"orthanc_password":"s3cr3tpw"}' > /dev/null
ok "the right password restores service" "$(code $F $V GET /api/studies)" "200"

# --- the rules of the settings themselves
put $A '{"orthanc_username":""}' > /dev/null
ok "an empty user name forgets the password too (even without asking)" "$(sview "d['settings']['orthanc_password_set']")" "False"
T=$(otest '{"username":"orthanc","password":"s3cr3tpw"}')
ok "Test connection can try an account that is not saved yet" "$(echo "$T" | py "str(d['ok'])+' '+d['account']")" "True orthanc"
ok "...without changing the saved settings" "$(sview "repr(d['settings']['orthanc_username'])")" "''"
put $A '{"orthanc_username":"orthanc","orthanc_password":"s3cr3tpw"}' > /dev/null
ok "the account can be set again" "$(code $F $V GET /api/studies)" "200"

echo "== changing one's own password"
A_OTHER=$(login $F admin SuperSecret123)
ADMIN_ID=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='admin'][0]")
ok "an administrator changes their own password" "$(code $F $A PUT /api/admin/users/$ADMIN_ID -H "$J" -d '{"password":"NewSuperSecret456"}')" "200"
ok "...the session in use keeps working" "$(code $F $A GET /api/me)" "200"
ok "...every other session of that account is signed out" "$(code $F $A_OTHER GET /api/me)" "401"
ok "...and the new password works" "$(login $F admin NewSuperSecret456 | tr -d '\n' | wc -c | tr -d ' ')" "64"
code $F $A PUT /api/admin/users/$ADMIN_ID -H "$J" -d '{"password":"SuperSecret123"}' > /dev/null

echo "== administrators"
mkuser admin2 AdminTwoPass123 '"read","visualize","upload","download","send","admin"'
ID1=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='admin'][0]")
ID2=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username']=='admin2'][0]")
B=$(login $F admin2 AdminTwoPass123)
( curl -s -o /dev/null -w "%{http_code}" -X PUT -H "Cookie: portal_session=$A" -H "$H" -H "$J" -d '{"permissions":["read"]}' $F/api/admin/users/$ID2 > c1.out &
  curl -s -o /dev/null -w "%{http_code}" -X PUT -H "Cookie: portal_session=$B" -H "$H" -H "$J" -d '{"permissions":["read"]}' $F/api/admin/users/$ID1 > c2.out & wait )
ok "two administrators demoting each other: exactly one succeeds" "$(printf '%s\n%s\n' "$(cat c1.out)" "$(cat c2.out)" | grep -c '^200$')" "1"
ok "...and one active administrator always remains" "$(sqlval "select count(*) from users where active=1 and permissions like '%admin%'")" "1"

echo "== first start without an account"
# First start without any account, as the compose file of deploy/existing-orthanc does it
# (it passes ORTHANC_USERNAME and ORTHANC_PASSWORD as EMPTY values, and Orthanc has no users)
export MOCK_AUTH=none
stack_fresh ORTHANC_USERNAME= ORTHANC_PASSWORD=
unset MOCK_AUTH
cd "$WORK"
A=$(login $F admin SuperSecret123)
ok "empty ORTHANC_USERNAME / ORTHANC_PASSWORD start the portal without an account" "$(sview "repr(d['settings']['orthanc_username'])+str(d['settings']['orthanc_password_set'])")" "''False"
ok "...and it works against an Orthanc that has no users" "$(code $F $A GET /api/studies)" "200"
has "...Test connection says it is connected without an account" "$(otest | py "d['message']")" "Connected to Orthanc 1.12.10 without an account"

echo "== ports: busy ports, fallback and LISTENERS_FROM_ENV"
# hold_port PORT -> keeps 127.0.0.1:PORT busy (as "another program" would) until killed
hold_port() { "$PYTHON3" -c "import socket,time;s=socket.socket();s.bind(('127.0.0.1',$1));s.listen();time.sleep(60)" & sleep 0.5; }
A=$(login $F admin SuperSecret123)
put $A '{"bind_addr":"127.0.0.1:18090"}' > /dev/null      # saved in the database
portal_stop
hold_port 18090; HOLDER=$!
portal_start
ok "saved port busy at start-up -> falls back to BIND_ADDR" "$(curl -s -o /dev/null -w '%{http_code}' $F/healthz)" "200"
has "...and says so in the log" "$(cat "$WORK/portal.log")" "Falling back to 127.0.0.1:18080 (BIND_ADDR)"
kill $HOLDER 2>/dev/null; portal_stop
hold_port 18080; HOLDER=$!
portal_start BIND_ADDR=127.0.0.1:18080 LISTENERS_FROM_ENV=true
has "BIND_ADDR busy -> clear error telling how to choose another port" "$(cat "$WORK/portal.log")" "choose a free one with BIND_ADDR"
kill $HOLDER 2>/dev/null; portal_stop
# The database still holds 18090; with LISTENERS_FROM_ENV the environment wins on every start.
portal_start LISTENERS_FROM_ENV=true BIND_ADDR=127.0.0.1:18095 REMOTE_BIND_ADDR=127.0.0.1:18096
F=http://127.0.0.1:18095; G=http://127.0.0.1:18096
A=$(login $F admin SuperSecret123)
ok "LISTENERS_FROM_ENV: the environment wins over the saved port" "$(curl -s -o /dev/null -w '%{http_code}' $F/healthz)/$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 http://127.0.0.1:18090/healthz)" "200/000"
ok "...the internet listener too" "$(curl -s -o /dev/null -w '%{http_code}' $G/healthz)" "200"
ok "...Settings knows the addresses are locked" "$(sview "str(d['listeners_from_env'])+' '+d['settings']['bind_addr']")" "True 127.0.0.1:18095"
OUT=$(put $A '{"bind_addr":"127.0.0.1:18097"}')
ok "...changing an address from Settings is refused (400)" "$(echo "$OUT" | tail -1)" "400"
has "...with a message pointing to INTRANET_PORT" "$OUT" "INTRANET_PORT"
ok "...other settings can still be saved" "$(put $A '{"session_hours":11,"bind_addr":"127.0.0.1:18095"}' | tail -1)" "200"
F=http://127.0.0.1:18080; G=http://127.0.0.1:18081

echo; echo "RESULT: $PASS passed, $FAIL failed$([ "$SKIP" -gt 0 ] && echo ", $SKIP skipped")"
stack_stop
[ "$FAIL" -eq 0 ]
