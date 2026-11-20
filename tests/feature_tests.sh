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
ok "a rule needs a destination (400)" "$(put $A '{"forward_rules":[{"name":"CT out","target":""}]}' | tail -1)" "400"
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

echo; echo "RESULT: $PASS passed, $FAIL failed$([ "$SKIP" -gt 0 ] && echo ", $SKIP skipped")"
stack_stop
[ "$FAIL" -eq 0 ]
