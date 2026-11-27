#!/bin/bash
# End-to-end tests of the newer features, on a fresh portal + fake Orthanc (see mock_orthanc.py):
# two-step sign-in, link notifications, anonymized / whole-patient links, QR codes, upload links,
# auto-forward rules, the patient CD export, backups, statistics and "recently viewed".
#   tests/feature_tests.sh            (uses target/debug/orthanc-portal; PORTAL_BIN=... to test another binary)
source "$(dirname "$0")/lib.sh"
stack_fresh
cd "$WORK"

# totp SECRET [offset seconds] -> the 6-digit code an authenticator app would show now
totp() { python3 - "$1" "${2:-0}" <<'PY'
import sys, hmac, hashlib, base64, struct, time
secret = base64.b32decode(sys.argv[1].replace(" ", "") + "=" * (-len(sys.argv[1].replace(" ", "")) % 8))
step = int((time.time() + int(sys.argv[2])) // 30)
h = hmac.new(secret, struct.pack(">Q", step), hashlib.sha1).digest()
o = h[19] & 15
print("%06d" % ((struct.unpack(">I", h[o:o + 4])[0] & 0x7fffffff) % 1000000))
PY
}
# post BASE COOKIE PATH JSON -> body (JSON answer)
post() { curl -s --max-time 15 -X POST -H "Cookie: portal_session=$2" -H "$H" -H "$J" -d "$4" "$1$3"; }
lastmail() { python3 - "$WORK/mail" <<'PY'
import sys, glob
from email import message_from_bytes, policy
files = sorted(glob.glob(sys.argv[1] + "/*.eml"))
if not files: print(""); sys.exit()
m = message_from_bytes(open(files[-1], "rb").read(), policy=policy.default)
print(m["Subject"]); print(m.get_body(("plain",)).get_content())
PY
}
mailcount() { ls "$WORK"/mail/*.eml 2>/dev/null | wc -l | tr -d ' '; }
# mkdicom FILE STUDY_UID SIZE -> a small DICOM Part 10 file (explicit VR little endian) of that study
mkdicom() { python3 - "$1" "$2" "$3" <<'PY'
import sys, struct
path, uid, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
def el(g, e, vr, v):
    if len(v) % 2: v += b"\0" if vr == b"UI" else b" "
    if vr in (b"OB", b"UN"): return struct.pack("<HH", g, e) + vr + b"\0\0" + struct.pack("<I", len(v)) + v
    return struct.pack("<HH", g, e) + vr + struct.pack("<H", len(v)) + v
data = b"\0" * 128 + b"DICM" + el(2, 0x10, b"UI", b"1.2.840.10008.1.2.1")
data += el(8, 0x60, b"CS", b"CT") + el(0x10, 0x20, b"LO", b"EXT-1") + el(0x20, 0x0D, b"UI", uid.encode())
data += el(0x7FE0, 0x10, b"OB", b"\x01" * max(0, size - len(data) - 12))
open(path, "wb").write(data)
PY
}

A=$(login $F admin SuperSecret123)
mkuser() { body $F $A /api/admin/users -X POST -H "$J" -d "{\"username\":\"$1\",\"password\":\"$2\",\"permissions\":[$3]$4}" > /dev/null; }
mkuser doctor DoctorPass123 '"read","visualize","download","share","upload","send"' ',"email":"doctor@example.org"'
mkuser viewer1 ViewerPass123 '"read","visualize"'
D=$(login $F doctor DoctorPass123); V=$(login $F viewer1 ViewerPass123)
put $A '{"share_public_url":"https://pacs.example.com"}' > /dev/null

echo "== two-step sign-in (off by default)"
ok "off: the password alone signs in" "$([ -n "$V" ] && echo yes)" "yes"
ok "...and the users are told" "$(body $F $V /api/me | py "d['features']['two_factor']")" "off"
ok "off: setting it up is refused (403)" "$(code $F $V POST /api/account/two-factor/setup -H "$J" -d '{"password":"ViewerPass123"}')" "403"
ok "the mode must be off, optional or required (400)" "$(put $A '{"two_factor":"sometimes"}' | tail -1)" "400"
put $A '{"two_factor":"optional"}' > /dev/null
ok "optional: setting it up needs the password (400)" "$(code $F $V POST /api/account/two-factor/setup -H "$J" -d '{"password":"wrong-password"}')" "400"
SETUP=$(post $F $V /api/account/two-factor/setup '{"password":"ViewerPass123"}')
SECRET=$(echo "$SETUP" | py "d['secret']")
has "...gives a QR code (SVG) and an otpauth:// address" "$(echo "$SETUP" | py "str('<svg' in d['qr'])+' '+d['uri'][:15]")" "True otpauth://totp/"
has "...for this account" "$(echo "$SETUP" | py "d['uri']")" "viewer1"
ok "...not active until confirmed" "$(body $F $V /api/me | py "d['two_factor']")" "False"
ok "a wrong first code is refused (400)" "$(code $F $V POST /api/account/two-factor/enable -H "$J" -d '{"code":"000000"}')" "400"
V2=$(login $F viewer1 ViewerPass123)   # another session of the same person (another computer)
CODES=$(post $F $V /api/account/two-factor/enable "{\"code\":\"$(totp "$SECRET")\"}")
ok "the right one switches it on and gives ten recovery codes" "$(echo "$CODES" | py "len(d['recovery_codes'])")" "10"
RC1=$(echo "$CODES" | py "d['recovery_codes'][0]")
ok "...the other sessions of the account end" "$(code $F $V2 GET /api/me)" "401"
ok "...this one goes on" "$(code $F $V GET /api/me)" "200"
ok "...the account says so (and never shows the secret)" "$(body $F $V /api/me | py "(d['two_factor'], d['recovery_codes_left'], 'totp_secret' in d)")" "(True, 10, False)"
STEP1=$(post $F "" /api/login '{"username":"viewer1","password":"ViewerPass123"}')
ok "sign-in: the password alone no longer opens a session" "$(echo "$STEP1" | py "d['two_factor']")" "code"
hasnt "...no cookie yet" "$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d '{"username":"viewer1","password":"ViewerPass123"}' $F/api/login)" "portal_session="
TICKET=$(echo "$STEP1" | py "d['ticket']")
ok "...a wrong code is refused (400)" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"123456\"}" $F/api/login/two-factor)" "400"
# The code used to confirm the set-up cannot be used again: wait for the next one.
sleep $(( 31 - $(date +%s) % 30 ))
CODE=$(totp "$SECRET")
OUT=$(curl -s -D - -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"$CODE\"}" $F/api/login/two-factor)
has "...the right code signs in (session cookie)" "$OUT" "portal_session="
TICKET=$(post $F "" /api/login '{"username":"viewer1","password":"ViewerPass123"}' | py "d['ticket']")
ok "...the same code a second time is refused (400)" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"$CODE\"}" $F/api/login/two-factor)" "400"
ok "...a recovery code works" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"$(echo $RC1 | tr 'A-Z' 'a-z')\"}" $F/api/login/two-factor)" "200"
TICKET=$(post $F "" /api/login '{"username":"viewer1","password":"ViewerPass123"}' | py "d['ticket']")
ok "...once" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"$RC1\"}" $F/api/login/two-factor)" "400"
ok "...a made-up ticket is refused (400)" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d "{\"ticket\":\"$(printf 'a%.0s' {1..64})\",\"code\":\"123456\"}" $F/api/login/two-factor)" "400"
ok "switching it off needs the password (400)" "$(code $F $V POST /api/account/two-factor/disable -H "$J" -d '{"password":"nope-nope-nope"}')" "400"
ok "...with it: off" "$(post $F $V /api/account/two-factor/disable '{"password":"ViewerPass123"}' | py "d['ok']")" "True"
ok "...the password alone signs in again" "$([ -n "$(login $F viewer1 ViewerPass123)" ] && echo yes)" "yes"
put $A '{"two_factor":"required"}' > /dev/null
STEP1=$(post $F "" /api/login '{"username":"doctor","password":"DoctorPass123"}')
ok "required: an account without it sets it up at sign-in" "$(echo "$STEP1" | py "d['two_factor']+' '+str('<svg' in d['qr'])")" "setup True"
DSECRET=$(echo "$STEP1" | py "d['secret']"); TICKET=$(echo "$STEP1" | py "d['ticket']")
OUT=$(curl -s -D hdr.txt -H "$H" -H "$J" -d "{\"ticket\":\"$TICKET\",\"code\":\"$(totp "$DSECRET")\"}" $F/api/login/two-factor)
DNEW=$(grep -i set-cookie hdr.txt | sed 's/.*portal_session=\([0-9a-f]*\);.*/\1/')
ok "...the first code confirms it, signs in and gives the recovery codes" "$(echo "$OUT" | py "(d['username'], len(d['recovery_codes']), d['two_factor'])")" "('doctor', 10, True)"
ok "...the session opened before it was required has ended" "$(code $F $D GET /api/me)" "401"
ok "...it cannot be switched off while required (403)" "$(code $F $DNEW POST /api/account/two-factor/disable -H "$J" -d '{"password":"DoctorPass123"}')" "403"
ok "an administrator resets it (lost phone)" "$(body $F $A /api/admin/users/2 -X PUT -H "$J" -d '{"reset_two_factor":true}' | py "d['two_factor']")" "False"
has "...in the audit log" "$(body $F $A /api/admin/audit)" "two_factor_reset"
put $A '{"two_factor":"off"}' > /dev/null
ok "off again: the password alone signs in" "$([ -n "$(login $F doctor DoctorPass123)" ] && echo yes)" "yes"
D=$(login $F doctor DoctorPass123)

echo "== my account: email and password"
ok "an invalid email address is refused (400)" "$(code $F $V PUT /api/account/email -H "$J" -d '{"email":"not an address"}')" "400"
ok "a valid one is kept" "$(body $F $V /api/account/email -X PUT -H "$J" -d '{"email":"Viewer <viewer@example.org>"}' | py "d['email']")" "viewer@example.org"
ok "changing my password needs the current one (400)" "$(code $F $V POST /api/account/password -H "$J" -d '{"current":"wrong-one-123","new":"NewViewerPass456"}')" "400"
ok "...with it: changed" "$(post $F $V /api/account/password '{"current":"ViewerPass123","new":"NewViewerPass456"}' | py "d['ok']")" "True"
ok "...this session goes on" "$(code $F $V GET /api/me)" "200"
ok "...the new password signs in" "$([ -n "$(login $F viewer1 NewViewerPass456)" ] && echo yes)" "yes"

echo "== share links: notify me, anonymized copy, every study of the patient, QR code"
ok "no mail server: notifications are not offered" "$(body $F $D /api/me | py "d['features']['share']['notify']")" "False"
put $A '{"smtp_host":"127.0.0.1","smtp_port":18025,"smtp_security":"none","smtp_from":"Demo PACS <pacs@example.com>"}' > /dev/null
ok "with one they are" "$(body $F $D /api/me | py "d['features']['share']['notify']")" "True"
mkuser nomail NoMailPass123 '"read","share"' ''
NM=$(login $F nomail NoMailPass123)
has "...it says where to add it" "$(body $F $NM /api/studies/$SID/share -X POST -H "$J" -d '{"notify":true}')" "My account"
L=$(body $F $D /api/studies/$SID/share -X POST -H "$J" -d '{"notify":true,"downloads":2}')
LT=$(echo "$L" | py "d['url'].rsplit('/s/',1)[1]"); LP=$(echo "$L" | py "d['password']"); LID=$(echo "$L" | py "d['id']")
ok "the link remembers it" "$(body $F $D /api/shares | py "[s['notify'] for s in d if s['id']==$LID][0]")" "True"
BEFORE=$(mailcount)
GC=$(unlock $LT "$LP"); sleep 1.5
ok "the guest opens it: the creator is emailed" "$(mailcount)" "$((BEFORE+1))"
OUT=$(lastmail)
has "...that it was opened" "$OUT" "was opened"
hasnt "...without the patient's name" "$OUT" "DOE"
unlock $LT "$LP" > /dev/null; sleep 1
ok "...only the first time" "$(mailcount)" "$((BEFORE+1))"
curl -s -o /dev/null -H "Cookie: share_session=$GC" $G/api/share/$LT/download; sleep 1.5
has "a download is told too, with the downloads left" "$(lastmail)" "Downloads left: 1"
ok "anonymized: switched off -> refused (403)" "$(put $A '{"share_anonymize":false}' > /dev/null; code $F $D POST /api/studies/$SID/share -H "$J" -d '{"anonymize":true}')" "403"
put $A '{"share_anonymize":true}' > /dev/null
AN=$(body $F $D /api/studies/$SID/share -X POST -H "$J" -d '{"anonymize":true}')
ok "anonymized: the link is created" "$(echo "$AN" | py "d['anonymized']")" "True"
ok "...Orthanc made a copy (synchronous, the original kept)" "$(tail -1 $WORK/modify.log | py "(d['action'], d['body']['KeepSource'], 'Asynchronous' in d['body'])")" "('anonymize', True, False)"
has "...for one made-up patient" "$(tail -1 $WORK/modify.log)" "ANON-"
AT=$(echo "$AN" | py "d['url'].rsplit('/s/',1)[1]"); AP=$(echo "$AN" | py "d['password']"); AID=$(echo "$AN" | py "d['id']")
AG=$(unlock $AT "$AP")
ok "...the guest sees the anonymized patient" "$(gbody $AG /api/share/$AT/study | py "d['patient']['name']")" "ANONYMOUS"
ok "...labelled in the archive" "$(grep -c shared-anonymized $WORK/labels.log)" "1"
ok "...revoking it" "$(code $F $D POST /api/shares/$AID/revoke)" "200"
sleep 1.5
ok "...deletes the copy in Orthanc" "$(grep -c a0a0a0a0 $WORK/delete.log)" "1"
ok "...and the link can never come back (409)" "$(code $F $D PUT /api/shares/$AID -H "$J" -d '{"minutes":60}')" "409"
PS=$(body $F $D /api/studies/$SID/share -X POST -H "$J" -d '{"patient":true}')
ok "every study of the patient: one link for both" "$(echo "$PS" | py "d['studies']")" "2"
ok "...several studies need the multi-study switch (403)" "$(put $A '{"multi_study_shares":false}' > /dev/null; code $F $D POST /api/studies/$SID/share -H "$J" -d '{"patient":true}')" "403"
put $A '{"multi_study_shares":true}' > /dev/null
S2=bbbbbbbb-cccccccc-dddddddd-eeeeeeee-ffffffff
code $F $A PUT /api/studies/$SID/labels/teaching > /dev/null; code $F $A PUT /api/studies/$S2/labels/teaching > /dev/null
ok "every study with a label: offered" "$(body $F $D /api/me | py "d['features']['share']['label']")" "True"
LS=$(body $F $D /api/shares -X POST -H "$J" -d '{"label":"teaching"}')
ok "...one link for every study with the label" "$(echo "$LS" | py "d['studies']")" "2"
ok "...found by Orthanc (all the label, ids only)" "$(tail -1 $WORK/find.log | py "(d['Labels'], d['LabelsConstraint'], d['Expand'])")" "(['teaching'], 'All', False)"
ok "...a label nobody has: refused (400)" "$(code $F $D POST /api/shares -H "$J" -d '{"label":"nobody-has-it"}')" "400"
ok "...an invalid label: refused (400)" "$(code $F $D POST /api/shares -H "$J" -d '{"label":"no spaces please"}')" "400"
ok "...a label AND a list of studies: refused (400)" "$(code $F $D POST /api/shares -H "$J" -d "{\"label\":\"teaching\",\"studies\":[\"$SID\"]}")" "400"
put $A '{"share_label":false}' > /dev/null
ok "...switched off: refused (403) and not offered" "$(code $F $D POST /api/shares -H "$J" -d '{"label":"teaching"}') $(body $F $D /api/me | py "d['features']['share']['label']")" "403 False"
put $A '{"share_label":true,"labels_enabled":false}' > /dev/null
ok "...labels switched off: refused too (403)" "$(code $F $D POST /api/shares -H "$J" -d '{"label":"teaching"}')" "403"
put $A '{"labels_enabled":true}' > /dev/null
OUT=$(curl -s -D - -H "Cookie: portal_session=$D" $F/api/shares/$LID/qr)
has "the QR code of a link is an SVG picture" "$OUT" "content-type: image/svg+xml"
has "...not kept by the browser" "$OUT" "no-store"
ok "...only for its creator or an administrator (403)" "$(code $F $NM GET /api/shares/$LID/qr)" "403"
ok "...only while it is active (409)" "$(code $F $D GET /api/shares/$AID/qr)" "409"

echo "== upload links: people outside send studies in"
ok "off by default: refused (403)" "$(code $F $D POST /api/upload-links -H "$J" -d '{}')" "403"
ok "...and the page shows nothing" "$(curl -s $G/api/upload/$(printf 'a%.0s' {1..48})/status | py "d['state']")" "unavailable"
put $A '{"upload_links":true}' > /dev/null
ok "on: offered to people with share + upload" "$(body $F $D /api/me | py "d['features']['upload_links']['enabled']")" "True"
ok "...not to a viewer" "$(body $F $V /api/me | py "d['features']['upload_links']['enabled']")" "False"
ok "...who cannot create one (403)" "$(code $F $V POST /api/upload-links -H "$J" -d '{}')" "403"
ok "a link above the size limit is refused (400)" "$(code $F $D POST /api/upload-links -H "$J" -d '{"max_mb":999999}')" "400"
UL=$(body $F $D /api/upload-links -X POST -H "$J" -d '{"max_mb":1,"note":"Mrs Doe previous MRI","notify":true}')
UT=$(echo "$UL" | py "d['url'].rsplit('/u/',1)[1]"); UP=$(echo "$UL" | py "d['password']"); UID2=$(echo "$UL" | py "d['id']")
has "created: a link to the internet side" "$(echo "$UL" | py "d['url']")" "https://pacs.example.com/u/"
ok "...with the default label" "$(echo "$UL" | py "d['label']")" "received"
ok "the page exists on the internet listener" "$(curl -s -o /dev/null -w '%{http_code}' $G/u/$UT)" "200"
ok "...not on the intranet (404)" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" $F/api/upload/$UT/status)" "404"
ok "it asks for the password" "$(curl -s $G/api/upload/$UT/status | py "d['state']")" "locked"
ok "sending without it: 401" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" --data-binary @/dev/null $G/api/upload/$UT/file)" "401"
ok "a wrong password: 400" "$(curl -s -o /dev/null -w '%{http_code}' -H "$H" -H "$J" -d '{"password":"WRONG-WRONG-WRONG"}' $G/api/upload/$UT/unlock)" "400"
UC=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"password\":\"$UP\"}" $G/api/upload/$UT/unlock | grep -i set-cookie | sed 's/.*upload_session=\([0-9a-f]*\);.*/\1/')
ok "the right one opens it" "$(curl -s -H "Cookie: upload_session=$UC" $G/api/upload/$UT/status | py "d['state']")" "open"
ok "...the upload cookie opens no shared study (401)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: share_session=$UC" $G/api/share/$LT/study)" "401"
mkdicom small.dcm 2.25.1001 2800; mkdicom big.dcm 2.25.1001 1100000; mkdicom other.dcm 2.25.2002 2800
mkdicom existing.dcm "$UID_" 2800; printf 'BAD!not dicom' > notes.txt
python3 -c "import zipfile,sys;zipfile.ZipFile(sys.argv[1],'w').write(sys.argv[2])" archive.zip small.dcm
BEFORE=$(mailcount)
OUT=$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @small.dcm $G/api/upload/$UT/file)
ok "a DICOM file is accepted" "$(echo "$OUT" | py "d['accepted']")" "True"
hasnt "...and the answer names no study" "$OUT" "$SID"
SMALL=$(wc -c < small.dcm | tr -d ' ')
ok "...it went to Orthanc" "$(tail -1 $WORK/upload.log | py "d['len']")" "$SMALL"
ok "...the new study got the label" "$(grep '"put": "received"' $WORK/labels.log | grep -c b0b0b0b0)" "1"
sleep 1.5
has "...and the creator is told" "$(lastmail)" "files received through your upload link"
ok "a file that is not DICOM is counted apart" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @notes.txt $G/api/upload/$UT/file | py "(d['accepted'], d['reason'])")" "(False, 'not_dicom')"
ok "a file over the link's size limit is refused (403)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @big.dcm $G/api/upload/$UT/file)" "403"
ok "the summary counts what arrived" "$(curl -s -H "Cookie: upload_session=$UC" $G/api/upload/$UT/summary | py "(d['files'], d['rejected'], d['bytes'])")" "(1, 1, $SMALL)"
UPLOADS=$(cat $WORK/upload.log | wc -l | tr -d ' ')
ok "a file of a study ALREADY in the archive is refused" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @existing.dcm $G/api/upload/$UT/file | py "(d['accepted'], d['reason'])")" "(False, 'already_in_archive')"
ok "...before Orthanc ever sees it" "$(cat $WORK/upload.log | wc -l | tr -d ' ')" "$UPLOADS"
has "...and it is in the audit log" "$(body $F $A /api/admin/audit)" "upload_link_refused"
FINDS=$(cat $WORK/find.log | wc -l | tr -d ' ')
ok "...the next file of that study is refused at once" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @existing.dcm $G/api/upload/$UT/file | py "d['reason']")" "already_in_archive"
ok "...without asking Orthanc again" "$(cat $WORK/find.log | wc -l | tr -d ' ')" "$FINDS"
ok "...and with one line in the audit log for the whole study" "$(body $F $A '/api/admin/audit?limit=1000' | py "sum(1 for e in d['entries'] if e['action']=='upload_link_refused')")" "1"
ok "a ZIP archive is refused (its contents cannot be checked first)" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @archive.zip $G/api/upload/$UT/file | py "(d['accepted'], d['reason'])")" "(False, 'not_dicom')"
ok "...nothing was counted against the size limit" "$(curl -s -H "Cookie: upload_session=$UC" $G/api/upload/$UT/summary | py "d['bytes']")" "$SMALL"
ok "...one email per batch" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @small.dcm $G/api/upload/$UT/file > /dev/null; sleep 1; echo $(( $(mailcount) - BEFORE )))" "1"
ok "the staff see the link and what came in" "$(body $F $D /api/upload-links | py "[(l['files_received'], l['files_rejected'], l['study_count'], l['status']) for l in d if l['id']==$UID2][0]")" "(2, 4, 1, 'active')"
ok "...and the study received (the new one, not the existing one)" "$(body $F $D /api/upload-links/$UID2/studies | py "[s['study_instance_uid'] for s in d]")" "['2.25.1001']"
ok "...its password again" "$(post $F $D /api/upload-links/$UID2/password '{}' | py "d['password']")" "$UP"
ok "...its QR code" "$(curl -s -o /dev/null -w '%{content_type}' -H "Cookie: portal_session=$D" $F/api/upload-links/$UID2/qr)" "image/svg+xml"
ok "...nobody else's links (403)" "$(code $F $NM GET /api/upload-links/$UID2/studies)" "403"
ok "a bigger limit" "$(body $F $D /api/upload-links/$UID2 -X PUT -H "$J" -d '{"max_mb":4}' | py "d['ok']")" "True"
ok "...lets the big file in" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @big.dcm $G/api/upload/$UT/file | py "d['accepted']")" "True"
ok "revoked: the sender is stopped at once (401)" "$(code $F $D POST /api/upload-links/$UID2/revoke > /dev/null; curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" -H "Cookie: upload_session=$UC" --data-binary @small.dcm $G/api/upload/$UT/file)" "401"
ok "...and the page says so" "$(curl -s $G/api/upload/$UT/status | py "d['state']")" "unavailable"

echo "== patient CD / USB export"
ok "needs the download permission (403)" "$(code $F $V GET "/api/export/cd?studies=$SID")" "403"
curl -s -o cd.zip -H "Cookie: portal_session=$D" "$F/api/export/cd?studies=$SID"
ok "a ZIP with the DICOMDIR, the images and a page for the patient" "$(python3 -c "import zipfile,sys;print(sorted(zipfile.ZipFile(sys.argv[1]).namelist()))" cd.zip)" "['DICOMDIR', 'IMAGES/IM0', 'README.html']"
ok "...the images untouched" "$(python3 -c "import zipfile,sys;print(zipfile.ZipFile(sys.argv[1]).read('IMAGES/IM0')==b'DICM-image'*50)" cd.zip)" "True"
has "...the page names the patient and the study" "$(python3 -c "import zipfile,sys;print(zipfile.ZipFile(sys.argv[1]).read('README.html').decode())" cd.zip)" "DOE JOHN"
hasnt "...and escapes what comes from DICOM" "$(python3 -c "import zipfile,sys;print(zipfile.ZipFile(sys.argv[1]).read('README.html').decode())" cd.zip)" "<script>alert"
mkdir -p viewer/bin; printf '[autorun]\nopen=bin/viewer.exe\n' > viewer/autorun.inf; printf 'MZ' > viewer/bin/viewer.exe
VDIR=$(python3 -c "import os,sys;print(os.path.abspath(sys.argv[1]))" viewer)
ok "a viewer folder must be a full path (400)" "$(put $A '{"cd_viewer_dir":"viewer"}' | tail -1)" "400"
put $A "$(python3 -c "import json,sys;print(json.dumps({'cd_viewer_dir':sys.argv[1]}))" "$VDIR")" > /dev/null
curl -s -o cd2.zip -H "Cookie: portal_session=$D" "$F/api/export/cd?studies=$SID"
ok "with a viewer folder: its files are on the disc too" "$(python3 -c "import zipfile,sys;print(sorted(zipfile.ZipFile(sys.argv[1]).namelist()))" cd2.zip)" "['DICOMDIR', 'IMAGES/IM0', 'README.html', 'autorun.inf', 'bin/viewer.exe']"
ok "...the temporary files are gone" "$(ls $WORK/exports 2>/dev/null | wc -l | tr -d ' ')" "0"
ok "switched off: refused (403)" "$(put $A '{"cd_export":false}' > /dev/null; code $F $D GET "/api/export/cd?studies=$SID")" "403"
put $A '{"cd_export":true}' > /dev/null

echo "== backups of the portal's database"
ok "no folder: nothing listed" "$(body $F $A /api/admin/backups | py "len(d['backups'])")" "0"
ok "...backing up now asks for the folder (400)" "$(code $F $A POST /api/admin/backups)" "400"
BDIR=$(python3 -c "import os,sys;print(os.path.abspath(sys.argv[1]))" "$WORK/backups")
put $A "$(python3 -c "import json,sys;print(json.dumps({'backup_dir':sys.argv[1],'backup_keep':2}))" "$BDIR")" > /dev/null
ok "with one: a backup now" "$(post $F $A /api/admin/backups '{}' | py "'portal-backup-' in d['written']")" "True"
ok "...it is a working SQLite database with the users" "$(python3 -c "import sqlite3,glob,sys;print(sqlite3.connect(glob.glob(sys.argv[1]+'/*.db')[0]).execute('select count(*) from users').fetchone()[0])" "$BDIR")" "4"
sleep 1.1; post $F $A /api/admin/backups '{}' > /dev/null; sleep 1.1; post $F $A /api/admin/backups '{}' > /dev/null
ok "...only the newest are kept (backup_keep)" "$(body $F $A /api/admin/backups | py "len(d['backups'])")" "2"
ok "a copy can be downloaded" "$(curl -s -H "Cookie: portal_session=$A" $F/api/admin/backup/download | head -c 15)" "SQLite format 3"
ok "...by administrators only (403)" "$(code $F $D GET /api/admin/backup/download)" "403"
has "...and it is in the audit log" "$(body $F $A /api/admin/audit)" "backup_downloaded"

echo "== statistics"
ST=$(body $F $A /api/admin/stats)
ok "twelve months" "$(echo "$ST" | py "len(d['months'])")" "12"
ok "...the archive's size" "$(echo "$ST" | py "d['storage']['studies']")" "1"
ok "...links created this month" "$(echo "$ST" | py "d['links']['shares_created'][-1] >= 3")" "True"
ok "...upload links" "$(echo "$ST" | py "d['links']['upload_links_created'][-1]")" "1"
ok "...who shares most" "$(echo "$ST" | py "d['top']['sharers'][0]['user']")" "doctor"
ok "Orthanc counted the months" "$(grep -c StudyDate $WORK/count.log | awk '{print ($1 >= 12)}')" "1"
ok "administrators only (403)" "$(code $F $D GET /api/admin/stats)" "403"

echo "== recently viewed (each person's own)"
ok "empty at first" "$(body $F $D /api/recent | py "len(d)")" "0"
ok "a study opened is noted" "$(post $F $D /api/recent/$SID "{\"uid\":\"$UID_\"}" | py "d['ok']")" "True"
ok "...and listed" "$(body $F $D /api/recent | py "d[0]['id']")" "$SID"
ok "...nobody else sees it" "$(body $F $A /api/recent | py "len(d)")" "0"
ok "an invalid UID is refused (400)" "$(code $F $D POST /api/recent/$SID -H "$J" -d '{"uid":"1.2.*"}')" "400"
ok "the list can be emptied" "$(code $F $D DELETE /api/recent > /dev/null; body $F $D /api/recent | py "len(d)")" "0"
ok "the length reaches the home page" "$(body $F $D /api/me | py "d['features']['recently_viewed']")" "8"

echo "== every feature can be switched off (and the browser is told)"
feat() { body $F $1 /api/me | py "$2"; }
# Things made while the features are on, to check what happens to them once switched off.
NL=$(body $F $D /api/studies/$SID/share -X POST -H "$J" -d '{"notify":true}')
NLT=$(echo "$NL" | py "d['url'].rsplit('/s/',1)[1]"); NLP=$(echo "$NL" | py "d['password']"); NLID=$(echo "$NL" | py "d['id']")
put $A '{"upload_links":true}' > /dev/null
U2=$(body $F $D /api/upload-links -X POST -H "$J" -d '{"max_mb":5}')
U2T=$(echo "$U2" | py "d['url'].rsplit('/u/',1)[1]"); U2P=$(echo "$U2" | py "d['password']"); U2ID=$(echo "$U2" | py "d['id']")
U2C=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"password\":\"$U2P\"}" $G/api/upload/$U2T/unlock | grep -i set-cookie | sed 's/.*upload_session=\([0-9a-f]*\);.*/\1/')

put $A '{"share_patient":false}' > /dev/null
ok "whole-patient links off: refused (403)" "$(code $F $D POST /api/studies/$SID/share -H "$J" -d '{"patient":true}')" "403"
ok "...not offered" "$(feat $D "d['features']['share']['patient']")" "False"
put $A '{"share_patient":true}' > /dev/null
put $A '{"share_qr":false}' > /dev/null
ok "QR codes off: a share link's refused (403)" "$(code $F $D GET /api/shares/$NLID/qr)" "403"
ok "...an upload link's too (403)" "$(code $F $D GET /api/upload-links/$U2ID/qr)" "403"
ok "...not offered" "$(feat $D "d['features']['share']['qr']")" "False"
put $A '{"share_qr":true}' > /dev/null
ok "QR codes on again" "$(code $F $D GET /api/shares/$NLID/qr)" "200"
put $A '{"share_notify":false}' > /dev/null
ok "notifications off: not offered (share and upload links)" "$(feat $D "(d['features']['share']['notify'], d['features']['upload_links']['notify'])")" "(False, False)"
ok "...asking for one is refused (share link: 403)" "$(code $F $D POST /api/studies/$SID/share -H "$J" -d '{"notify":true}')" "403"
ok "...(upload link: 403)" "$(code $F $D POST /api/upload-links -H "$J" -d '{"notify":true}')" "403"
BEFORE=$(mailcount); unlock $NLT "$NLP" > /dev/null; sleep 1.5
ok "...and a link that asked for them earlier stays quiet" "$(mailcount)" "$BEFORE"
put $A '{"share_notify":true}' > /dev/null
ok "an upload session works while upload links are on" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$U2C" --data-binary @other.dcm $G/api/upload/$U2T/file | py "d['accepted']")" "True"
ok "...nor to the study another upload link brought in" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$U2C" --data-binary @small.dcm $G/api/upload/$U2T/file | py "d['reason']")" "already_in_archive"
put $A '{"upload_links":false}' > /dev/null
ok "upload links off: a sender ALREADY signed in can send nothing more (410)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "$H" -H "Cookie: upload_session=$U2C" --data-binary @small.dcm $G/api/upload/$U2T/file)" "410"
ok "...nor read the summary (410)" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: upload_session=$U2C" $G/api/upload/$U2T/summary)" "410"
ok "...the staff cannot list them (403)" "$(code $F $D GET /api/upload-links)" "403"
ok "...not offered" "$(feat $D "d['features']['upload_links']['enabled']")" "False"
put $A '{"backups_enabled":false}' > /dev/null
ok "backups off: no list (403)" "$(code $F $A GET /api/admin/backups)" "403"
ok "...no backup now (403)" "$(code $F $A POST /api/admin/backups)" "403"
ok "...no download (403)" "$(code $F $A GET /api/admin/backup/download)" "403"
ok "...the screen is told" "$(feat $A "d['features']['options']['backups']")" "False"
put $A '{"backups_enabled":true}' > /dev/null
ok "backups on again" "$(code $F $A GET /api/admin/backups)" "200"
put $A '{"account_self_service":false}' > /dev/null
ok "own password and email off: changing the email is refused (403)" "$(code $F $D PUT /api/account/email -H "$J" -d '{"email":"x@example.org"}')" "403"
ok "...the password too (403)" "$(code $F $D POST /api/account/password -H "$J" -d '{"current":"DoctorPass123","new":"Another-Pass-123"}')" "403"
ok "...an administrator still can (Users)" "$(body $F $A /api/admin/users/2 -X PUT -H "$J" -d '{"email":"doc@example.org"}' | py "d['email']")" "doc@example.org"
ok "...the page is told" "$(feat $D "d['features']['options']['account_self_service']")" "False"
put $A '{"account_self_service":true}' > /dev/null
put $A '{"cd_export":false,"stats_page":false,"recently_viewed":false,"share_anonymize":false}' > /dev/null
ok "the other switches: CD export, statistics, recently viewed, anonymized copies (all 403)" "$(code $F $D GET "/api/export/cd?studies=$SID")/$(code $F $A GET /api/admin/stats)/$(code $F $D GET /api/recent)/$(code $F $D POST /api/studies/$SID/share -H "$J" -d '{"anonymize":true}')" "403/403/403/403"
ok "...and none is offered" "$(feat $A "(d['features']['options']['cd_export'], d['features']['options']['stats_page'], d['features']['recently_viewed'], d['features']['share']['anonymize'])")" "(False, False, 0, False)"
put $A '{"cd_export":true,"stats_page":true,"recently_viewed":true,"share_anonymize":true}' > /dev/null
D=$(login $F doctor DoctorPass123)

echo "== colours of modality buttons and labels"
BTNS=$(body $F $A /api/admin/settings | python3 -c "import sys,json;b=json.load(sys.stdin)['settings']['modality_buttons'];b[0]['color']='#D9480F';b[3]['color']='#f59f00';print(json.dumps({'modality_buttons':b,'label_colors':{'urgent':'#C92A2A','teaching':''}}))")
ok "colours are saved" "$(put $A "$BTNS" | tail -1)" "200"
ok "...a button's colour reaches every user (lower case)" "$(body $F $V /api/me | py "d['features']['modality_buttons'][0]['color']")" "#d9480f"
ok "...and each of its codes, for the tags (the RX button: CR and DX), on the invitation page too" "$(curl -s $G/api/public | py "(d['modality_colors']['CT'], d['modality_colors']['CR'], d['modality_colors']['DX'], 'MR' in d['modality_colors'])")" "('#d9480f', '#f59f00', '#f59f00', False)"
ok "...label colours: set ones kept, a label put back to Default forgotten" "$(body $F $V /api/me | py "d['features']['search']['label_colors']")" "{'urgent': '#c92a2a'}"
ok "...a colour that is not #rrggbb is refused (400)" "$(put $A '{"label_colors":{"urgent":"red"}}' | tail -1)" "400"
ok "...a label colour for an invalid label is refused (400)" "$(put $A '{"label_colors":{"no spaces":"#123456"}}' | tail -1)" "400"

echo "== auto-forward rules"
ok "a rule must send the study somewhere or notify somebody (400)" "$(put $A '{"forward_rules":[{"name":"CT out","target":""}]}' | tail -1)" "400"
ok "...an invalid address to notify is refused (400)" "$(put $A '{"forward_rules":[{"name":"Tell","notify":"not an address"}]}' | tail -1)" "400"
ok "...and so is an invalid label (400)" "$(put $A '{"forward_rules":[{"name":"Tell","notify":"rad@example.org","labels":["no spaces"]}]}' | tail -1)" "400"
ok "...and sane modality codes (400)" "$(put $A '{"forward_rules":[{"name":"CT out","target":"ris","modalities":["C*"]}]}' | tail -1)" "400"
put $A '{"forward_enabled":true,"forward_rules":[{"name":"CT out","target":"ris","modalities":["ct"],"calling_aet":"SCAN*"},{"name":"MR only","target":"ris","modalities":["MR"]}]}' > /dev/null
ok "the codes are tidied" "$(body $F $A /api/admin/settings | py "d['settings']['forward_rules'][0]['modalities']")" "['CT']"
ok "the Settings screen gets the destinations" "$(body $F $A /api/admin/settings | py "'ris' in [n['id'] for n in d['destinations']]")" "True"
# The first round only notes where Orthanc's list of changes ends (nothing old is sent).
for i in $(seq 1 40); do [ -n "$(sqlval "select value from settings where key='value:forward_seq'" 2>/dev/null)" ] && break; sleep 0.5; done
stores() { cat "$WORK/store.log" 2>/dev/null | wc -l | tr -d ' '; }
STORES_BEFORE=$(stores)
curl -s -X POST -d "{\"ChangeType\":\"StableStudy\",\"ID\":\"$SID\",\"ResourceType\":\"Study\"}" http://127.0.0.1:18042/__mock/change > /dev/null
for i in $(seq 1 40); do [ "$(stores)" -gt "$STORES_BEFORE" ] && break; sleep 0.5; done
ok "a stable CT study from SCANNER1 is sent by the matching rule (not by the MR one)" "$(( $(stores) - STORES_BEFORE ))" "1"
ok "...shown as an automatic transfer" "$(body $F $A /api/transfers | py "d[0]['username']")" "auto: CT out"
ok "...the Settings screen says so" "$(body $F $A /api/admin/settings | py "d['forward']['forwarded']")" "1"
put $A '{"forward_enabled":false}' > /dev/null
STORES_BEFORE=$(stores)
curl -s -X POST -d "{\"ChangeType\":\"StableStudy\",\"ID\":\"$SID\",\"ResourceType\":\"Study\"}" http://127.0.0.1:18042/__mock/change > /dev/null
sleep 17
ok "switched off: a new stable study is not sent" "$(( $(stores) - STORES_BEFORE ))" "0"
ok "...and the rules are kept for later" "$(body $F $A /api/admin/settings | py "len(d['settings']['forward_rules'])")" "2"

echo "== rules that notify by email (a CT from a given place, or with a given label)"
put $A "{\"public_base_url\":\"https://pacs.example.org/\",\"forward_enabled\":true,\"forward_rules\":[{\"name\":\"Teaching CT\",\"modalities\":[\"CT\"],\"institution\":\"riverside\",\"labels\":[\"teaching\",\"urgent\"],\"notify\":\"Dr Rad <rad@example.org>; second@example.org\"},{\"name\":\"Never\",\"labels\":[\"no-such-label\"],\"notify\":\"x@example.org\"}]}" > /dev/null
ok "the addresses are tidied, a rule that only notifies has no destination" "$(body $F $A /api/admin/settings | py "(d['settings']['forward_rules'][0]['notify'], d['settings']['forward_rules'][0]['target'])")" "('rad@example.org, second@example.org', '')"
for i in $(seq 1 40); do [ -n "$(sqlval "select value from settings where key='value:forward_seq'" 2>/dev/null)" ] && break; sleep 0.5; done
MAILS=$(mailcount); STORES_BEFORE=$(stores)
curl -s -X POST -d "{\"ChangeType\":\"StableStudy\",\"ID\":\"$SID\",\"ResourceType\":\"Study\"}" http://127.0.0.1:18042/__mock/change > /dev/null
for i in $(seq 1 40); do [ "$(mailcount)" -gt "$MAILS" ] && break; sleep 0.5; done
ok "a CT from Riverside with the label teaching is announced by email (by the matching rule only)" "$(( $(mailcount) - MAILS ))" "1"
OUT=$(lastmail)
has "...the subject names the rule, the modalities and the place" "$OUT" 'new study for the rule "Teaching CT" (CT, SR, Riverside Vets)'
has "...the body has the accession number and the labels" "$OUT" "Accession number: ACC1"
has "...and links to the study in the portal" "$OUT" "https://pacs.example.org/?study_uid=$UID_"
ok "...but never names the patient" "$(echo "$OUT" | grep -c -e DOE -e P-001)" "0"
ok "...nothing is sent to a DICOM node by a rule that only notifies" "$(( $(stores) - STORES_BEFORE ))" "0"
ok "...written in the audit log" "$(body $F $A '/api/admin/audit?limit=50' | py "[e['username'] for e in d['entries'] if e['action']=='auto_notified'][0]")" "auto: Teaching CT"
ok "...and counted on the Settings screen" "$(body $F $A /api/admin/settings | py "d['forward']['notified']")" "1"
curl -s -X POST -d "{\"ChangeType\":\"StableStudy\",\"ID\":\"$SID\",\"ResourceType\":\"Study\"}" http://127.0.0.1:18042/__mock/change > /dev/null
sleep 17
ok "the same study becoming stable again (more images) is not announced twice" "$(( $(mailcount) - MAILS ))" "1"
put $A '{"forward_enabled":false,"public_base_url":""}' > /dev/null

echo "== several Orthanc servers (the IDs of another server are written key:id)"
# A second fake Orthanc: the same study, under the same Orthanc ID, as on the default one.
mkdir -p "$WORK/m2"
MOCK_LOG_DIR="$WORK/m2" "$PYTHON3" "$TESTS/mock_orthanc.py" 18043 > "$WORK/mock2.out" 2>&1 & remember mock2 $!
sleep 0.6
ARCHIVE='{"key":"archive","label":"Archive","url":"http://127.0.0.1:18043","username":"orthanc"'
ok "a second Orthanc server is added" "$(put $A "{\"orthanc_servers\":[$ARCHIVE,\"password\":\"s3cr3tpw\"}]}" | tail -1)" "200"
ok "...its password is never sent back" "$(body $F $A /api/admin/settings | py "('password' in d['settings']['orthanc_servers'][0], d['settings']['orthanc_servers'][0]['password_set'])")" "(False, True)"
ok "...nor sent to another address without typing it again (400)" "$(put $A '{"orthanc_servers":[{"key":"archive","url":"http://127.0.0.1:18099","username":"orthanc"}]}' | tail -1)" "400"
ok "...nothing changed" "$(body $F $A /api/admin/settings | py "d['settings']['orthanc_servers'][0]['url']")" "http://127.0.0.1:18043"
ok "Test connection of that server, with its saved password" "$(body $F $A /api/admin/orthanc/test -X POST -H "$J" -d '{"server":"archive"}' | py "d['ok']")" "True"
ok "...its saved password is not tried on an address typed in the form" "$(body $F $A /api/admin/orthanc/test -X POST -H "$J" -d '{"server":"archive","url":"http://127.0.0.1:18042","username":"orthanc"}' | py "d['ok']")" "False"
AID="archive:$SID"
ok "the series of a study on that server: every ID says the server" "$(body $F $A /api/studies/$AID/series | py "len(d) > 0 and all(s['id'].startswith('archive:') and s['study_id'] == '$AID' for s in d)")" "True"
ok "...the same plain ID is still the default server's study" "$(body $F $A /api/studies/$SID/series | py "all(':' not in s['id'] for s in d)")" "True"
ok "a label goes on THAT server's study" "$(code $F $A PUT /api/studies/$AID/labels/second-server)/$(tail -1 "$WORK/m2/labels.log" | py "d['put']")" "200/second-server"
ok "...not on the default server's" "$(grep -c second-server "$WORK/labels.log")" "0"
# zips N -> how many study ZIPs the second Orthanc was asked for (every request is in its auth.log)
zips() { grep -c "/studies/$SID/archive" "$WORK/m2/auth.log"; }
Z0=$(zips)
ok "downloading it asks that server for the ZIP" "$(code $F $A GET /api/studies/$AID/download)/$(( $(zips) - Z0 ))" "200/1"
R0=$(cat "$WORK/m2/rendered.log" 2>/dev/null | wc -l)
ok "its thumbnail" "$(code $F $A GET /api/studies/$AID/thumbnail)" "200"
ok "...drawn by that server, not taken from the default server's cached one (same Orthanc ID)" "$(code $F $A GET /api/studies/$SID/thumbnail)/$(( $(cat "$WORK/m2/rendered.log" 2>/dev/null | wc -l) - R0 > 0 ))" "200/1"
has "the audit log names that server's study as key:id" "$(body $F $A /api/admin/audit | py "' '.join(e['detail'] for e in d['entries'] if e['action'] == 'download_study')")" "archive:$SID"
put $A '{"allow_modify":true}' > /dev/null
MJ=$(body $F $A /api/studies/$AID/modify -X POST -H "$J" -d '{"replace":{"StudyDescription":"ON ARCHIVE"}}' | py "d['job_id']")
ok "editing it starts a job on that server, and the job's ID says so" "$(echo "$MJ" | cut -d: -f1)/$([ -s "$WORK/m2/modify.log" ] && echo asked)" "archive/asked"
ok "...the job is followed there" "$(code $F $A GET "/api/jobs/$MJ")" "200"
put $A '{"allow_modify":false}' > /dev/null
ok "an unknown server: 404" "$(code $F $A GET /api/studies/nowhere:$SID/series)" "404"
ok "a malformed server key: 400" "$(code $F $A GET /api/studies/Archive:$SID/series)" "400"
ok "one ZIP of studies on two servers is refused (400)" "$(code $F $A GET "/api/download?studies=$SID,$AID")" "400"
ok "that server's own destinations are listed" "$(body $F $A "/api/modalities?server=archive" | py "'ris' in [n['id'] for n in d]")" "True"
ok "sending it: that server sends it" "$(code $F $A POST /api/studies/$AID/send -H "$J" -d '{"target":"ris"}')/$(tail -1 "$WORK/m2/store.log" | py "d['Resources']")" "200/['$SID']"
J2=$(grep -c "/jobs/" "$WORK/m2/auth.log")
ok "...and the transfer is followed there" "$(body $F $A /api/transfers | py "len(d) > 0")/$(( $(grep -c "/jobs/" "$WORK/m2/auth.log") > J2 ))" "True/1"
ok "sending studies of two servers at once is refused (400)" "$(code $F $A POST /api/send -H "$J" -d "{\"target\":\"ris\",\"studies\":[\"$SID\",\"$AID\"]}")" "400"
# Copy to another server: the portal reads each image from one and stores it on the other.
U0=$(cat "$WORK/m2/upload.log" 2>/dev/null | wc -l)
ok "copying a study to the archive" "$(body $F $A /api/studies/$SID/copy -X POST -H "$J" -d '{"server":"archive"}' | py "d['images']")" "8"
for i in 1 2 3 4 5 6 7 8 9 10; do [ "$(body $F $A /api/transfers | py "[t['state'] for t in d if t['direction'] == 'copy'][0]")" = "Success" ] && break; sleep 0.3; done
ok "...every image went there, and the transfer says so" "$(( $(cat "$WORK/m2/upload.log" | wc -l) - U0 ))/$(body $F $A /api/transfers | py "[(t['state'], t['instances'], t['target']) for t in d if t['direction'] == 'copy'][0]")" "8/('Success', 8, 'archive')"
ok "...not to the server it is on (400)" "$(code $F $A POST /api/studies/$SID/copy -H "$J" -d '{"server":"main"}')" "400"
ok "...nor to an unknown one (404)" "$(code $F $A POST /api/studies/$SID/copy -H "$J" -d '{"server":"nowhere"}')" "404"
ok "...deleting the original needs deleting switched on (403)" "$(code $F $A POST /api/studies/$SID/copy -H "$J" -d '{"server":"archive","delete_original":true}')" "403"
# Auto-forward on every server: a rule watches the servers it names.
ok "a rule watching a server that does not exist is refused (400)" "$(put $A '{"forward_rules":[{"name":"X","target":"ris","servers":["nowhere"]}]}' | tail -1)" "400"
ok "...and one copying to a server that does not exist (400)" "$(put $A '{"forward_rules":[{"name":"X","target":"server:nowhere"}]}' | tail -1)" "400"
ok "...and one copying to the server it watches (400)" "$(put $A '{"forward_rules":[{"name":"X","target":"server:archive","servers":["archive"]}]}' | tail -1)" "400"
put $A '{"forward_enabled":true,"forward_rules":[{"name":"Archive out","target":"ris","servers":["archive"]},{"name":"Archive to main","target":"server:main","servers":["archive"]}]}' > /dev/null
ok "...a server a rule watches cannot be removed (400)" "$(put $A '{"orthanc_servers":[]}' | tail -1)" "400"
for i in $(seq 1 40); do [ -n "$(sqlval "select value from settings where key='value:forward_seq:archive'" 2>/dev/null)" ] && break; sleep 0.5; done
S1=$(cat "$WORK/store.log" 2>/dev/null | wc -l); S2=$(cat "$WORK/m2/store.log" 2>/dev/null | wc -l); U1=$(cat "$WORK/upload.log" 2>/dev/null | wc -l)
curl -s -X POST -d "{\"ChangeType\":\"StableStudy\",\"ID\":\"$SID\",\"ResourceType\":\"Study\"}" http://127.0.0.1:18043/__mock/change > /dev/null
for i in $(seq 1 40); do [ "$(cat "$WORK/m2/store.log" 2>/dev/null | wc -l)" -gt "$S2" ] && [ "$(cat "$WORK/upload.log" | wc -l)" -ge $((U1 + 8)) ] && break; sleep 0.5; done
ok "a study arriving on the archive is sent by the archive, not by the default server" "$(( $(cat "$WORK/m2/store.log" | wc -l) - S2 ))/$(( $(cat "$WORK/store.log" 2>/dev/null | wc -l) - S1 ))" "1/0"
ok "...and copied to the default server by the other rule" "$(( $(cat "$WORK/upload.log" | wc -l) - U1 ))" "8"
ok "...both shown as automatic transfers" "$(body $F $A /api/transfers | py "sorted({t['username'] for t in d[:2]})")" "['auto: Archive out', 'auto: Archive to main']"
ok "...the Settings screen says when each server was read" "$(body $F $A /api/admin/settings | py "'archive' in d['forward']['checks']")" "True"
put $A '{"forward_enabled":false,"forward_rules":[]}' > /dev/null
# The pages that show one server at a time: ?server= (the default server without it).
asked() { grep -c "\"path\": \"$1" "$WORK/m2/auth.log"; }
put $A '{"jobs_page":true,"system_page":true,"stats_page":true,"query_retrieve":true,"worklists_enabled":true}' > /dev/null
N0=$(asked /system); J0=$(asked /jobs); M0=$(asked /modalities); W0=$(asked /worklists); Q0=$(asked /dicom-web/servers); T0=$(asked /tools/count-resources)
ok "System, Jobs, DICOM nodes, worklists, query / retrieve and Statistics of the archive ask the archive" "$(code $F $A GET "/api/admin/system?server=archive")/$(code $F $A GET "/api/admin/jobs?server=archive")/$(code $F $A GET "/api/admin/nodes?server=archive")/$(code $F $A GET "/api/worklists?server=archive")/$(code $F $A GET "/api/retrieve/sources?server=archive")/$(code $F $A GET "/api/admin/stats?server=archive&refresh=true")" "200/200/200/200/200/200"
ok "...every one of them reached it" "$(( $(asked /system) > N0 ))$(( $(asked /jobs) > J0 ))$(( $(asked /modalities) > M0 ))$(( $(asked /worklists) > W0 ))$(( $(asked /dicom-web/servers) > Q0 ))$(( $(asked /tools/count-resources) > T0 ))" "111111"
ok "...an unknown server: 404" "$(code $F $A GET "/api/admin/system?server=nowhere")/$(code $F $A GET "/api/admin/jobs?server=nowhere")" "404/404"
put $A '{"query_retrieve":false,"worklists_enabled":false}' > /dev/null
# Uploads into a chosen server.
U0=$(cat "$WORK/m2/upload.log" 2>/dev/null | wc -l)
ok "a staff upload into the archive" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: portal_session=$A" -H "$H" -H "Content-Type: application/dicom" --data-binary @small.dcm "$F/api/upload?server=archive")/$(( $(cat "$WORK/m2/upload.log" | wc -l) - U0 ))" "200/1"
ok "...not into an unknown server (404)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Cookie: portal_session=$A" -H "$H" -H "Content-Type: application/dicom" --data-binary @small.dcm "$F/api/upload?server=nowhere")" "404"
put $A '{"upload_links":true}' > /dev/null
UL3=$(body $F $A /api/upload-links -X POST -H "$J" -d '{"max_mb":1,"server":"archive"}')
ok "an upload link that stores into the archive" "$(echo "$UL3" | py "d['server']" 2>/dev/null || echo "$UL3")" "archive"
ok "...not into an unknown server (404)" "$(code $F $A POST /api/upload-links -H "$J" -d '{"server":"nowhere"}')" "404"
UT3=$(echo "$UL3" | py "d['url'].rsplit('/u/',1)[1]"); UP3=$(echo "$UL3" | py "d['password']"); UID3=$(echo "$UL3" | py "d['id']")
UC3=$(curl -s -D - -o /dev/null -H "$H" -H "$J" -d "{\"password\":\"$UP3\"}" $G/api/upload/$UT3/unlock | grep -i set-cookie | sed 's/.*upload_session=\([0-9a-f]*\);.*/\1/')
mkdicom third.dcm 2.25.3003 2800
U0=$(cat "$WORK/m2/upload.log" | wc -l); M0=$(cat "$WORK/upload.log" | wc -l)
ok "...what the person sends goes to the archive, not the default server" "$(curl -s -X POST -H "$H" -H "Cookie: upload_session=$UC3" --data-binary @third.dcm $G/api/upload/$UT3/file | py "d['accepted']")/$(( $(cat "$WORK/m2/upload.log" | wc -l) - U0 ))/$(( $(cat "$WORK/upload.log" | wc -l) - M0 ))" "True/1/0"
ok "...and its studies are listed with the archive's IDs" "$(body $F $A /api/upload-links/$UID3/studies | py "len(d) > 0 and all(s['id'].startswith('archive:') for s in d)")" "True"
put $A '{"upload_links":false}' > /dev/null
ok "recently viewed remembers the server" "$(post $F $A /api/recent/$AID "{\"uid\":\"$UID_\"}" | py "d['ok']")/$(body $F $A /api/recent | py "d[0]['id']")" "True/$AID"
# A link to that server's study: the guest's page, download and viewer use that server.
L2=$(body $F $A /api/studies/$AID/share -X POST -H "$J" -d '{"downloads":1}')
L2T=$(echo "$L2" | py "d['url'].rsplit('/s/',1)[1]"); L2P=$(echo "$L2" | py "d['password']")
GC2=$(unlock $L2T "$L2P")
ok "a link to it: the guest sees the study" "$(curl -s -H "Cookie: share_session=$GC2" -H "$H" "$G/api/share/$L2T/study" | py "d['study']['uid']")" "$UID_"
Z0=$(zips)
ok "...downloads it from that server" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: share_session=$GC2" -H "$H" "$G/api/share/$L2T/download")/$(( $(zips) - Z0 ))" "200/1"
PROXY_BEFORE=$(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l)
ok "...and views it from that server" "$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: share_session=$GC2" "$G/dicom-web/studies/$UID_/series")/$(( $(cat "$WORK/m2/proxy.log" | wc -l) - PROXY_BEFORE ))" "200/1"
echo "== several Orthanc servers: searching them all, and who may use which"
# Both fake Orthancs hold the same studies (same UIDs, same Orthanc IDs).
ok "the page is told about both servers" "$(body $F $A /api/me | py "[s['key'] for s in d['orthanc_servers']]")" "['main', 'archive']"
ok "a search asks both by default: one row per study, naming both servers" "$(body $F $A "/api/studies?patient_name=DOE" | py "(d['servers'], sorted({tuple(s['servers']) for s in d['items']}), all(':' not in s['id'] for s in d['items']))")" "(['main', 'archive'], [('main', 'archive')], True)"
ok "...with the ID of each copy (for Open the copy on)" "$(body $F $A "/api/studies?patient_name=DOE" | py "[c['id'] for s in d['items'] if s['id'] == '$SID' for c in s['copies']]")" "['$SID', 'archive:$SID']"
ok "...each study once" "$(body $F $A "/api/studies?patient_name=DOE" | py "len(d['items']) == len({s['study_instance_uid'] for s in d['items']})")" "True"
ok "only the archive: its IDs say so" "$(body $F $A "/api/studies?servers=archive" | py "(d['servers'], len(d['items']) > 0 and all(s['id'].startswith('archive:') and s['server'] == 'archive' for s in d['items']))")" "(['archive'], True)"
ok "an unknown server is refused (400)" "$(code $F $A GET "/api/studies?servers=nowhere")" "400"
ok "series of the archive: series and study IDs say so" "$(body $F $A "/api/series?servers=archive" | py "len(d['items']) > 0 and all(x['id'].startswith('archive:') and x['study']['id'].startswith('archive:') for x in d['items'])")" "True"
ok "series of both: each series once, naming both" "$(body $F $A "/api/series" | py "sorted({tuple(x['servers']) for x in d['items']})")" "[('main', 'archive')]"
ok "too deep a page over several servers is refused (400)" "$(code $F $A GET "/api/studies?offset=6000")" "400"
ok "the number of studies: on each server" "$(body $F $A /api/stats | py "[s['key'] for s in d['servers']]")" "['main', 'archive']"
# The distinct study IDs of both fake Orthancs (the main one also holds copies made by earlier checks).
DISTINCT=$( (curl -s -u orthanc:s3cr3tpw http://127.0.0.1:18042/studies; echo; curl -s -u orthanc:s3cr3tpw http://127.0.0.1:18043/studies) | python3 -c "import sys,json;print(len({i for line in sys.stdin if line.strip() for i in json.loads(line)}))")
ok "...and in all, a study on both counted once" "$(body $F $A /api/stats | py "d['studies']")" "$DISTINCT"
M1=$(cat "$WORK/proxy.log" 2>/dev/null | wc -l); M2=$(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l)
ok "a viewer's search naming no study asks both servers, and gives each study once" "$(body $F $A "/dicom-web/studies?00100020=P-001" | py "len(d)")/$(( $(cat "$WORK/proxy.log" 2>/dev/null | wc -l) - M1 ))/$(( $(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l) - M2 ))" "1/1/1"

# An account that may use the archive only.
ok "an account cannot be limited to no server (400)" "$(code $F $A POST /api/admin/users -H "$J" -d '{"username":"nosrv","password":"LongPassword123","permissions":["read"],"servers":[]}')" "400"
ok "...nor to a server that does not exist (400)" "$(code $F $A POST /api/admin/users -H "$J" -d '{"username":"nosrv","password":"LongPassword123","permissions":["read"],"servers":["nowhere"]}')" "400"
mkuser arch LongPassword123 '"read","visualize","download","upload","send"' ',"servers":["archive"]'
AR=$(login $F arch LongPassword123)
ok "the account is limited to the archive" "$(body $F $A /api/admin/users | py "[u['servers'] for u in d['users'] if u['username'] == 'arch']")" "[['archive']]"
ok "...its page offers the archive only" "$(body $F $AR /api/me | py "[s['key'] for s in d['orthanc_servers']]")" "['archive']"
ok "...it searches the archive only" "$(body $F $AR /api/studies | py "(d['servers'], all(s['id'].startswith('archive:') and s.get('servers', ['archive']) == ['archive'] for s in d['items']))")" "(['archive'], True)"
ok "...the default server cannot be searched (400, like an unknown one)" "$(code $F $AR GET "/api/studies?servers=main")" "400"
ok "...a study of the default server does not exist for it (404)" "$(code $F $AR GET /api/studies/$SID/series)/$(code $F $AR GET /api/studies/$SID/download)/$(code $F $AR GET /api/studies/$SID/thumbnail)" "404/404/404"
ok "...the same study on the archive does" "$(code $F $AR GET /api/studies/$AID/series)" "200"
ok "...it cannot copy to the default server (404)" "$(code $F $AR POST /api/studies/$AID/copy -H "$J" -d '{"server":"main"}')" "404"
ok "...it cannot share or send the default server's study (404)" "$(code $F $AR POST /api/studies/$SID/send -H "$J" -d '{"target":"ris"}')" "404"
ok "...bulk sending refuses the default server's studies (404)" "$(code $F $AR POST /api/send -H "$J" -d "{\"target\":\"ris\",\"studies\":[\"$SID\"]}")" "404"
has "...uploading (into the default server) is refused" "$(body $F $AR /api/upload -X POST -H "Content-Type: application/dicom" --data-binary "DICM")" "cannot use the Orthanc server"
put $A '{"query_retrieve":true,"worklists_enabled":true,"upload_links":true}' > /dev/null
mkuser archq LongPassword123 '"read","retrieve","worklists","share","upload"' ',"servers":["archive"]'
AQ=$(login $F archq LongPassword123)
has "...query / retrieve, worklists and upload links say they need the default server" "$(body $F $AQ /api/retrieve/sources) $(body $F $AQ /api/worklists) $(body $F $AQ /api/upload-links -X POST -H "$J" -d '{}')" "cannot use the Orthanc server"
ok "...all refused (403)" "$(code $F $AQ GET /api/retrieve/sources)/$(code $F $AQ GET /api/worklists)/$(code $F $AQ POST /api/upload-links -H "$J" -d '{}')" "403/403/403"
ok "...an upload link into its own server is allowed" "$(body $F $AQ /api/upload-links -X POST -H "$J" -d '{"server":"archive"}' | py "d['server']")" "archive"
put $A '{"query_retrieve":false,"worklists_enabled":false,"upload_links":false}' > /dev/null
ok "...only the archive's counts" "$(body $F $AR /api/stats | py "[s['key'] for s in d['servers']]")" "['archive']"
M1=$(cat "$WORK/proxy.log" 2>/dev/null | wc -l); M2=$(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l)
ok "...its viewer reads the study from the archive, never from the default server" "$(code $F $AR GET "/dicom-web/studies/$UID_/series")/$(( $(cat "$WORK/proxy.log" 2>/dev/null | wc -l) - M1 ))/$(( $(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l) - M2 ))" "200/0/1"
M1=$(cat "$WORK/proxy.log" 2>/dev/null | wc -l); M2=$(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l)
ok "...and its viewer's searches too" "$(code $F $AR GET "/dicom-web/studies?00100020=P-001")/$(( $(cat "$WORK/proxy.log" 2>/dev/null | wc -l) - M1 ))/$(( $(cat "$WORK/m2/proxy.log" 2>/dev/null | wc -l) - M2 ))" "200/0/1"
ARID=$(body $F $A /api/admin/users | py "[u['id'] for u in d['users'] if u['username'] == 'arch'][0]")
ok "back to every server" "$(code $F $A PUT /api/admin/users/$ARID -H "$J" -d '{"servers":null}')/$(code $F $AR GET /api/studies/$SID/series)" "200/200"
ok "...an update without servers keeps them" "$(code $F $A PUT /api/admin/users/$ARID -H "$J" -d '{"servers":["archive"]}')/$(code $F $A PUT /api/admin/users/$ARID -H "$J" -d '{"email":""}')/$(code $F $AR GET /api/studies/$SID/series)" "200/200/404"

# Roles give their servers to their users.
ROLES=$(body $F $A /api/admin/settings | py "json.dumps([dict(r, servers=['archive']) if r['key'] == 'viewer' else r for r in d['settings']['roles']])")
ok "a role limited to the archive" "$(put $A "{\"roles\":$ROLES}" | tail -1)" "200"
mkuser rolev LongPassword123 '"read","visualize"' ',"role":"viewer"'
ok "...a new account with that role gets it" "$(body $F $A /api/admin/users | py "[u['servers'] for u in d['users'] if u['username'] == 'rolev']")" "[['archive']]"
ok "...a server a role uses cannot be removed (400)" "$(put $A '{"orthanc_servers":[]}' | tail -1)" "400"
ROLES=$(body $F $A /api/admin/settings | py "json.dumps([dict(r, servers=None) for r in d['settings']['roles']])")
ok "...back to every server: its users follow" "$(put $A "{\"roles\":$ROLES}" | tail -1)/$(body $F $A /api/admin/users | py "[u['servers'] for u in d['users'] if u['username'] == 'rolev']")" "200/[None]"

# A server that does not answer: the others' results, and a notice.
stop_pid mock2
ok "a server down: the other's results, and which one did not answer" "$(body $F $A /api/studies | py "(len(d['items']) > 0, [u['server'] for u in d.get('unavailable', [])])")" "(True, ['archive'])"
ok "...the count leaves it out" "$(body $F $A /api/stats | py "([s['key'] for s in d['servers']], [u['server'] for u in d.get('unavailable', [])])")" "(['main'], ['archive'])"
ok "...only that server chosen: its error (502)" "$(code $F $A GET "/api/studies?servers=archive")" "502"
ok "...an account limited to it gets the error too, not the other server's studies" "$(code $F $AR GET /api/studies)" "502"
ok "switched off: its studies are not reachable (404)" "$(put $A "{\"orthanc_servers\":[$ARCHIVE,\"enabled\":false}]}" | tail -1)/$(code $F $A GET /api/studies/$AID/series)" "200/404"
ok "...and its password was kept" "$(body $F $A /api/admin/settings | py "d['settings']['orthanc_servers'][0]['password_set']")" "True"
put $A '{"orthanc_servers":[]}' > /dev/null
stop_pid mock2

echo; echo "RESULT: $PASS passed, $FAIL failed$([ "$SKIP" -gt 0 ] && echo ", $SKIP skipped")"
stack_stop
[ "$FAIL" -eq 0 ]
