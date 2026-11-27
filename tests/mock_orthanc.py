"""A fake, recent Orthanc used by the portal's automated tests (nothing here talks to a real PACS).

It answers just enough of Orthanc's REST API, the DICOMweb plugin and the OHIF plugin for the tests,
and records what it receives in $MOCK_LOG_DIR/*.log so tests can check what the portal asked.

    python3 mock_orthanc.py [port]          (default 18042; user "orthanc", password "s3cr3tpw")

How it protects its REST API can be changed while it runs (POST /__mock/auth {"mode": ...}) or at
start-up (environment variable MOCK_AUTH). The three real-world setups the portal must cope with:

    basic   Orthanc has users: HTTP Basic credentials are required (401 without them)   [default]
    none    Orthanc has NO users and accepts this client (an "allowed clients / IPs" list that includes us)
    deny    Orthanc has NO users and this client is NOT on the allowed list (403 for everything)

Every request's Authorization header is recorded in auth.log, so tests can prove that the portal
sends credentials only when it was told to.
"""
import base64, json, os, re, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

LOG_DIR = os.environ.get("MOCK_LOG_DIR", "/tmp/portal-tests")
VERSION = os.environ.get("MOCK_VERSION", "1.12.10")
os.makedirs(LOG_DIR, exist_ok=True)

STUDY_ID = "aaaaaaaa-bbbbbbbb-cccccccc-dddddddd-eeeeeeee"
SERIES_ID = "11111111-22222222-33333333-44444444-55555555"   # a CT series (3 images)
SR_ID = "99999999-88888888-77777777-66666666-55555555"       # a report: NO pixel data, more instances than the CT
JOB_ID = "27d7b341-e0e2-4825-95ba-fe8b4c2c304d"
STUDY = {"ID": STUDY_ID, "Type": "Study", "Series": [SERIES_ID, SR_ID], "ParentPatient": "77777777-66666666-55555555-44444444-33333333",
         "MainDicomTags": {"StudyInstanceUID": "1.2.826.0.1.3680043.8.498.1", "StudyDate": "20240517",
                           "StudyDescription": "CT CHEST <script>alert(1)</script>", "AccessionNumber": "ACC1",
                           "ReferringPhysicianName": "HOUSE^GREGORY", "InstitutionName": "Riverside Vets"},
         "PatientMainDicomTags": {"PatientName": "DOE^JOHN", "PatientID": "P-001", "PatientBirthDate": "19800502", "PatientSex": "M",
                                  "OtherPatientIDs": "OLD-77"},
         "RequestedTags": {"ModalitiesInStudy": "CT\\SR", "NumberOfStudyRelatedInstances": "120"}}
SERIES = {"ID": SERIES_ID, "ParentStudy": STUDY_ID, "Instances": ["a1", "a2", "a3"],
          "MainDicomTags": {"SeriesInstanceUID": "1.2.3.4", "Modality": "CT", "SeriesDescription": "Chest 1mm", "SeriesNumber": "2", "BodyPartExamined": "CHEST"}}
SR_SERIES = {"ID": SR_ID, "ParentStudy": STUDY_ID, "Instances": ["b1", "b2", "b3", "b4", "b5"],
             "MainDicomTags": {"SeriesInstanceUID": "1.2.3.5", "Modality": "SR", "SeriesDescription": "Dose report", "SeriesNumber": "9"}}
JPEG = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00fake-thumbnail-bytes\xff\xd9"
MODALITIES = {"ris": {"AET": "RIS", "Host": "10.0.0.20", "Port": 104, "Manufacturer": "Generic", "AllowStore": True, "AllowFind": True},
              "archive-only": {"AET": "ARC", "Host": "10.0.0.21", "Port": 104, "AllowStore": False},
              "down": {"AET": "DOWN", "Host": "10.9.9.9", "Port": 104, "AllowStore": True, "AllowEcho": True}}
job_polls = {"n": 0}
LABELS = {}   # Orthanc labels: study id -> set of labels


def with_labels(study):
    """A study as Orthanc returns it: with its "Labels"."""
    return dict(study, Labels=sorted(LABELS.get(study["ID"], ())))


def all_labels():
    return sorted(set().union(*LABELS.values())) if LABELS else []
MODE = {"auth": os.environ.get("MOCK_AUTH", "basic"),   # see the docstring
        "copies": 1}   # how many times a studies search finds the fixture study (POST /__mock/copies), for long lists


# ---- demo mode (MOCK_DEMO=1) -------------------------------------------------------------------
# For trying the portal by hand (tests/dev_env.sh), not for the automated tests: a dozen studies with
# real (generated) preview images, simple searching and sorting, and uploads that create new studies.
# Nothing below runs unless MOCK_DEMO=1, so the automated tests see exactly the fixture above.
DEMO = os.environ.get("MOCK_DEMO") == "1"
DEMO_STUDIES, DEMO_SERIES = {}, {}


def demo_png(width, seed):
    """A grey PNG that looks vaguely like a scan: a bright disc on black, different for each seed."""
    import zlib, struct, math
    w = h = max(32, min(int(width or 128), 512))
    cx, cy, r = w * (0.45 + (seed % 5) * 0.025), h * (0.5 - (seed % 3) * 0.03), w * (0.30 + (seed % 4) * 0.03)
    rows = []
    for y in range(h):
        row = bytearray(b"\x00")
        for x in range(w):
            d = math.hypot(x - cx, (y - cy) * 1.15) / r
            v = 0 if d > 1 else int(200 * (1 - d * d) ** 0.4 * (0.75 + 0.25 * math.sin(x * 0.21 + seed) * math.cos(y * 0.17)))
            row += bytes((max(0, v),) * 3)
        rows.append(bytes(row))
    def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"".join(rows))) + chunk(b"IEND", b""))


def demo_add_study(n, name, pid, date, desc, modality, body, images, received, referring="", institution=""):
    # Hex digits and dashes only, like real Orthanc ids (the portal refuses anything else).
    sid, ser = f"{n:08x}-0000-0000-0000-00000000000a", f"{n:08x}-0000-0000-0000-00000000000b"
    DEMO_SERIES[ser] = {"ID": ser, "ParentStudy": sid, "Instances": [f"{n:08x}-{k:08x}" for k in range(images)],
                        "MainDicomTags": {"SeriesInstanceUID": f"1.2.999.{n}.1", "Modality": modality,
                                          "SeriesDescription": desc, "SeriesNumber": "1", "BodyPartExamined": body}}
    DEMO_STUDIES[sid] = {"ID": sid, "Type": "Study", "Series": [ser], "LastUpdate": received,
                         "MainDicomTags": {"StudyInstanceUID": f"1.2.999.{n}", "StudyDate": date, "StudyTime": "101500",
                                           "StudyDescription": desc, "AccessionNumber": f"ACC{n:04d}",
                                           "ReferringPhysicianName": referring, "InstitutionName": institution},
                         "PatientMainDicomTags": {"PatientName": name, "PatientID": pid, "PatientBirthDate": f"19{(50 + n * 3) % 100:02d}0{1 + n % 9}15",
                                                  "PatientSex": "F" if n % 2 else "M", "OtherPatientIDs": f"OLD-{n * 7}" if n % 3 == 0 else ""},
                         "RequestedTags": {"ModalitiesInStudy": modality, "NumberOfStudyRelatedInstances": str(images)}}
    return sid


# Which demo archive this fake Orthanc holds (several run side by side to try several Orthanc servers,
# see tests/dev_env.sh): "main" (default), "archive" (older studies) or "research" (teaching cases).
# The same study number on two of them is the same study (same UID, same Orthanc ID), as on real
# servers that both hold a study.
DEMO_SET = os.environ.get("MOCK_DEMO_SET", "main")

if DEMO and DEMO_SET == "archive":
    for i, (name, pid, mod, desc, body, year) in enumerate([
            ("SMITH^JOHN", "P-1001", "DX", "LUMBAR SPINE X-RAY (2019)", "LSPINE", "2019"),
            ("GARCIA^MARIA", "P-1000", "CT", "CT HEAD (BASELINE)", "HEAD", "2021"),
            ("THOMPSON^GEORGE", "P-2001", "MR", "MRI LEFT SHOULDER", "SHOULDER", "2018"),
            ("LOPEZ^CARMEN", "P-2002", "US", "PELVIC ULTRASOUND", "PELVIS", "2020"),
            ("IVANOV^DMITRI", "P-2003", "CT", "CT SINUSES", "HEAD", "2017"),
            ("HANSEN^INGRID", "P-2004", "MG", "DIAGNOSTIC MAMMOGRAPHY", "BREAST", "2022"),
            ("KIM^JISOO", "P-2005", "DX", "CHEST PA", "CHEST", "2019"),
            ("ADEYEMI^TUNDE", "P-2006", "MR", "MRI CERVICAL SPINE", "CSPINE", "2021")]):
        demo_add_study(101 + i, name, pid, f"{year}0{3 + i % 6}1{i}", desc, mod, body, 3 + i % 3, f"{year}0{3 + i % 6}1{i}T101500",
                       ["HOUSE^GREGORY", "YANG^CRISTINA"][i % 2], "Riverside Vets")
    # On the main server too (same study): one row with both badges, "Open the copy on..."
    demo_add_study(15, "SMITH^JOHN", "P-1001", "20250310", "MRI LUMBAR SPINE (FOLLOW-UP)", "MR", "LSPINE", 4, "20250312T080000", "HOUSE^GREGORY", "Riverside Vets")
    demo_add_study(4, "MULLER^HANS", "P-1003", "20261114", "CHEST PA AND LATERAL", "DX", "CHEST", 6, "20261115T080000", "SHEPHERD^DEREK", "Hilltop Animal Hospital")
    LABELS[sorted(DEMO_STUDIES)[2]] = {"teaching"}
elif DEMO and DEMO_SET == "research":
    for i, (name, mod, desc, body) in enumerate([
            ("ANON^CASE^001", "CT", "TEACHING: CT PULMONARY EMBOLISM", "CHEST"), ("ANON^CASE^002", "MR", "TEACHING: MRI MS PLAQUES", "HEAD"),
            ("ANON^CASE^003", "CT", "TEACHING: CT APPENDICITIS", "ABDOMEN"), ("ANON^CASE^004", "DX", "TEACHING: PNEUMOTHORAX", "CHEST"),
            ("ANON^CASE^005", "US", "TEACHING: GALLSTONES", "ABDOMEN"), ("ANON^CASE^006", "NM", "TEACHING: BONE METASTASES", "WHOLEBODY")]):
        demo_add_study(201 + i, name, f"R-{300 + i}", f"2026{i + 1:02d}15", desc, mod, body, 4 + i % 3, f"2026{i + 1:02d}15T120000", "", "Research")
    # A study copied here from the main server (same study): one row, two servers.
    demo_add_study(12, "JOHANSSON^ANNA", "P-1011", "20261106", "MRI BRAIN", "MR", "HEAD", 3, "20261120T150000", "SHEPHERD^DEREK", "Riverside Vets")
    for n in (0, 2):
        LABELS[sorted(DEMO_STUDIES)[n]] = {"teaching"}
elif DEMO:
    for i, (name, mod, desc, body) in enumerate([
            ("GARCIA^MARIA", "CT", "CT HEAD WITHOUT CONTRAST", "HEAD"), ("SMITH^JOHN", "MR", "MRI LUMBAR SPINE", "LSPINE"),
            ("NGUYEN^LINH", "US", "ABDOMEN ULTRASOUND", "ABDOMEN"), ("MULLER^HANS", "DX", "CHEST PA AND LATERAL", "CHEST"),
            ("ROSSI^GIULIA", "MG", "SCREENING MAMMOGRAPHY", "BREAST"), ("KOWALSKI^PIOTR", "CT", "CT CHEST WITH CONTRAST", "CHEST"),
            ("DUBOIS^CLAIRE", "MR", "MRI RIGHT KNEE", "KNEE"), ("OKAFOR^EMEKA", "CR", "LEFT WRIST 3 VIEWS", "HAND"),
            ("SATO^YUKI", "US", "THYROID ULTRASOUND", "NECK"), ("BROWN^EMMA", "CT", "CT ABDOMEN PELVIS", "ABDOMEN"),
            ("SILVA^RAFAEL", "XA", "CORONARY ANGIOGRAPHY", "HEART"), ("JOHANSSON^ANNA", "MR", "MRI BRAIN", "HEAD"),
            ("PATEL^RAVI", "DX", "PELVIS AP", "PELVIS"), ("WILLIAMS^OLIVIA", "NM", "BONE SCAN WHOLE BODY", "WHOLEBODY")]):
        demo_add_study(i + 1, name, f"P-{1000 + i}", f"202611{17 - i:02d}", desc, mod, body, 3 + i % 4, f"202611{17 - i:02d}T09{i:02d}00",
                       ["HOUSE^GREGORY", "YANG^CRISTINA", "SHEPHERD^DEREK"][i % 3], ["Riverside Vets", "Hilltop Animal Hospital"][i % 2])
    # Two more studies of SMITH^JOHN (P-1001), to try "all studies of this patient"
    demo_add_study(15, "SMITH^JOHN", "P-1001", "20250310", "MRI LUMBAR SPINE (FOLLOW-UP)", "MR", "LSPINE", 4, "20250310T101500", "HOUSE^GREGORY", "Riverside Vets")
    demo_add_study(16, "SMITH^JOHN", "P-1001", "20240122", "LUMBAR SPINE X-RAY", "DX", "LSPINE", 2, "20240122T101500", "YANG^CRISTINA", "Riverside Vets")
    ids = sorted(DEMO_STUDIES)
    for n, labels in [(0, {"urgent"}), (1, {"insurance", "second-opinion"}), (3, {"urgent", "teaching"}), (6, {"teaching"}), (9, {"insurance"})]:
        LABELS[ids[n]] = set(labels)


def demo_tag(resource, key):
    """The value of a tag for a study or a series (looking in the parent study and patient too)."""
    if key == "ModalitiesInStudy":
        return resource.get("RequestedTags", {}).get("ModalitiesInStudy", "")
    for source in (resource, DEMO_STUDIES.get(resource.get("ParentStudy"), {})):
        for part in ("MainDicomTags", "PatientMainDicomTags"):
            if key in source.get(part, {}):
                return source[part][key]
    return ""


def demo_matches(value, key, wanted):
    """Orthanc's matching rules: lists with \\, ranges for dates, wildcards * and ?, else exact (no case)."""
    import fnmatch
    value = value or ""
    if key == "ModalitiesInStudy" or "\\" in wanted:
        return bool(set(value.split("\\")) & set(wanted.split("\\")))
    if key.endswith("Date") and "-" in wanted:
        lo, _, hi = wanted.partition("-")
        return bool(value) and (not lo or value >= lo) and (not hi or value <= hi)
    return fnmatch.fnmatchcase(value.lower(), wanted.lower())


def demo_find(q):
    """Enough of /tools/find for the demo: any main tag, labels, sorting (OrderBy), paging."""
    level = q["Level"]
    items = list(DEMO_STUDIES.values()) if level == "Study" else list(DEMO_SERIES.values())
    for key, wanted in q.get("Query", {}).items():
        if key == "SOPInstanceUID":   # the demo images of a series are <series UID>.1, .2...
            items = [r for r in items if wanted.startswith(r["MainDicomTags"]["SeriesInstanceUID"] + ".")]
            continue
        items = [r for r in items if demo_matches(demo_tag(r, key), key, wanted)]
    if level == "Study" and "Labels" in q:
        wanted, mode = set(q["Labels"]), q.get("LabelsConstraint", "All")
        has = lambda s: LABELS.get(s["ID"], set())
        test = {"All": lambda s: wanted <= has(s), "Any": lambda s: bool(wanted & has(s)), "None": lambda s: not (wanted & has(s))}[mode]
        items = [s for s in items if test(s)]
    for order in reversed(q.get("OrderBy", [])):   # stable sorts, last key first
        key = order["Key"]
        value = (lambda r: r.get("LastUpdate", "")) if order.get("Type") == "Metadata" else (lambda r, k=key: demo_tag(r, k).lower())
        items.sort(key=value, reverse=order.get("Direction") == "DESC")
    since = q.get("Since", 0)
    found = items[since:since + q.get("Limit", 100)]
    return [with_labels(s) for s in found] if level == "Study" else [with_parent_uid(r, q) for r in found]


def with_parent_uid(series, q):
    """A series as Orthanc returns it when the find asked for its parent study's StudyInstanceUID."""
    if "StudyInstanceUID" not in q.get("RequestedTags", []):
        return series
    study = all_studies().get(series["ParentStudy"], {})
    return {**series, "RequestedTags": {"StudyInstanceUID": study.get("MainDicomTags", {}).get("StudyInstanceUID", "")}}


DEMO_JOBS = {}


def demo_store(alias, body):
    """A transfer that takes about 25 seconds (images arrive gradually); "down" fails after 8 seconds."""
    import time, uuid
    job = str(uuid.uuid4())
    study = DEMO_STUDIES.get((body.get("Resources") or [""])[0])
    images = 40 * (int(study["RequestedTags"]["NumberOfStudyRelatedInstances"]) if study else 3)
    DEMO_JOBS[job] = {"start": time.time(), "images": images, "alias": alias}
    return {"ID": job, "Path": f"/jobs/{job}"}


def demo_job(job_id):
    import time
    job = DEMO_JOBS[job_id]
    elapsed = time.time() - job["start"]
    content = {"InstancesCount": job["images"], "FailedInstancesCount": 0, "RemoteAet": MODALITIES[job["alias"]].get("AET", "")}
    if job["alias"] == "down" and elapsed >= 8:
        content["FailedInstancesCount"] = job["images"] - int(job["images"] * 0.3)
        return {"ID": job_id, "Type": "DicomModalityStore", "State": "Failure", "Progress": 30, "EffectiveRuntime": 8.0,
                "ErrorDescription": "Cannot connect to the remote DICOM node (DOWN at 10.9.9.9:104)", "Content": content}
    progress = min(100, int(elapsed / 25 * 100))
    return {"ID": job_id, "Type": "DicomModalityStore", "State": "Success" if progress >= 100 else "Running",
            "Progress": progress, "EffectiveRuntime": round(min(elapsed, 25), 1), "Content": content}


def demo_upload():
    """Each uploaded file becomes a new study, received now (so it appears first on the home page)."""
    import time
    n = 100 + len(DEMO_STUDIES)
    return demo_add_study(n, f"UPLOADED^FILE {n - 113}", f"U-{n}", time.strftime("%Y%m%d"), "UPLOADED FROM THE PORTAL",
                          "OT", "", 1, time.strftime("%Y%m%dT%H%M%S"))


import threading
LOG_LOCK = threading.Lock()   # requests arrive in parallel: one line at a time


def log(name, obj):
    with LOG_LOCK, open(os.path.join(LOG_DIR, name + ".log"), "a") as f:
        f.write(json.dumps(obj) + "\n")


# ---- the optional features (Settings > Features): images, tags, changes, jobs, peers, DICOMweb,
# query / retrieve, worklists, system. Works on the fixture study and, in demo mode, on the demo data.
import uuid as _uuid, time as _time

JOBS = {}          # instant jobs (modify, anonymize, move, retrieve...): id -> job
WORKLISTS = {}     # id -> {"ID", "Tags"}
LOG_LEVEL = {"level": "default"}
PEERS = {"branch": {"Url": "https://branch.example.com/orthanc/", "Username": "secret-user"}}
DICOMWEB = {"cloud": {"Url": "https://dicomweb.example.com/", "Username": "secret-user"}}
PDF = b"%PDF-1.4\n% fake report\n"
REMOTE = [  # what the remote systems "have" (query / retrieve)
    {"StudyInstanceUID": "1.2.999.900", "PatientName": "REMOTE^ROSA", "PatientID": "R-1", "StudyDate": "20261001",
     "StudyDescription": "CT ABDOMEN (OTHER CLINIC)", "AccessionNumber": "X1", "ModalitiesInStudy": "CT", "NumberOfStudyRelatedInstances": "40"},
    {"StudyInstanceUID": "1.2.826.0.1.3680043.8.498.1", "PatientName": "DOE^JOHN", "PatientID": "P-001", "StudyDate": "20240517",
     "StudyDescription": "CT CHEST", "AccessionNumber": "ACC1", "ModalitiesInStudy": "CT\\SR", "NumberOfStudyRelatedInstances": "120"},
]


def instant_job(kind, content=None, state="Success"):
    job = str(_uuid.uuid4())
    JOBS[job] = {"ID": job, "Type": kind, "State": state, "Progress": 100 if state == "Success" else 0, "Priority": 0,
                 "CreationTime": _time.strftime("%Y%m%dT%H%M%S"), "CompletionTime": _time.strftime("%Y%m%dT%H%M%S"),
                 "EffectiveRuntime": 0.4, "ErrorDescription": "Success" if state == "Success" else "Failure", "Content": content or {}}
    return {"ID": job, "Path": f"/jobs/{job}"}


STUDY2_ID = "bbbbbbbb-cccccccc-dddddddd-eeeeeeee-ffffffff"   # a second study (links for several studies)
STUDY2 = {"ID": STUDY2_ID, "Type": "Study", "Series": [SERIES_ID],
          "MainDicomTags": {"StudyInstanceUID": "1.2.826.0.1.3680043.8.498.2", "StudyDate": "20250101", "StudyDescription": "MR KNEE"},
          "PatientMainDicomTags": {"PatientName": "DOE^JOHN", "PatientID": "P-001"},
          "RequestedTags": {"ModalitiesInStudy": "MR", "NumberOfStudyRelatedInstances": "3"}}


PATIENT_ID = "77777777-66666666-55555555-44444444-33333333"   # the patient of STUDY and STUDY2
ANON = {}       # anonymized copies made by POST /studies/{id}/anonymize without "Asynchronous": id -> study
OAUTH = []      # token requests received by /__mock/oauth/token (Microsoft 365 email tests)
CHANGES = []    # Orthanc's list of changes (GET /changes); the tests add to it (POST /__mock/change)
GITHUB = {"status": 200, "release": {"tag_name": "v0.0.1", "name": "Old", "html_url": "https://github.com/huma2000/orthanc-portal/releases/tag/v0.0.1",
                                     "published_at": "2020-01-01T00:00:00Z", "body": "", "draft": False, "prerelease": False}}


def all_studies():
    return DEMO_STUDIES if DEMO else {STUDY_ID: STUDY, STUDY2_ID: STUDY2, **ANON}


def uploaded_study_uid(body):
    """The StudyInstanceUID of an uploaded DICOM file made by the tests (explicit VR: tag, "UI", length)."""
    at = body.find(b"\x20\x00\x0d\x00UI")
    if at < 0: return None
    n = int.from_bytes(body[at + 6:at + 8], "little")
    return body[at + 8:at + 8 + n].rstrip(b"\x00 ").decode()


def study_for_upload(uid):
    """Like Orthanc: an image goes to the study with its UID, or starts a new one."""
    for st in all_studies().values():
        if st["MainDicomTags"]["StudyInstanceUID"] == uid: return st["ID"]
    sid = f"{len(ANON) + 1:08x}-b0b0b0b0-b0b0b0b0-b0b0b0b0-b0b0b0b0"
    ANON[sid] = {"ID": sid, "Type": "Study", "Series": [SERIES_ID],
                 "MainDicomTags": {"StudyInstanceUID": uid, "StudyDate": "20240101", "StudyDescription": "RECEIVED"},
                 "PatientMainDicomTags": {"PatientName": "SENT^BY^LINK", "PatientID": "EXT-1"}}
    return sid


def media_zip():
    """A real ZIP with a DICOMDIR (what /tools/create-media returns), for the patient CD export."""
    import io, zipfile
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("DICOMDIR", b"DICM-index")
        z.writestr("IMAGES/IM0", b"DICM-image" * 50)
    return buffer.getvalue()


def all_series():
    return DEMO_SERIES if DEMO else {SERIES_ID: SERIES, SR_ID: SR_SERIES}


def instances_of(series):
    out = []
    for n, iid in enumerate(series["Instances"]):
        pdf = series["MainDicomTags"].get("Modality") == "DOC"
        out.append({"ID": iid, "Type": "Instance", "ParentSeries": series["ID"],
                    "MainDicomTags": {"InstanceNumber": str(n + 1), "SOPInstanceUID": f"{series['MainDicomTags']['SeriesInstanceUID']}.{n + 1}"},
                    "RequestedTags": {"SOPClassUID": "1.2.840.10008.5.1.4.1.1.104.1" if pdf else "1.2.840.10008.5.1.4.1.1.2"}})
    return out


def find_instance(iid):
    for series in all_series().values():
        if iid in series["Instances"]:
            return series
    return None


def apply_replace(study_id, replace):
    """Modify / move in the demo: the new values go into the study (a real Orthanc rewrites the files)."""
    study = all_studies().get(study_id)
    if not study: return
    for key, value in replace.items():
        part = "PatientMainDicomTags" if key.startswith("Patient") or key == "OtherPatientIDs" else "MainDicomTags"
        study[part][key] = value


def extra_get(h, p, query):
    if not DEMO and p == f"/studies/{STUDY2_ID}": return h._json(with_labels(STUDY2))
    if not DEMO and p == f"/studies/{STUDY2_ID}/series": return h._json([SERIES])
    m = re.match(r"^/studies/([^/]+)(/series)?$", p)
    if m and m.group(1) in ANON:
        return h._json([SERIES] if m.group(2) else with_labels(ANON[m.group(1)]))
    if p == f"/patients/{PATIENT_ID}":
        return h._json({"ID": PATIENT_ID, "Type": "Patient", "Studies": [STUDY_ID, STUDY2_ID]})
    if p == "/changes":
        last = CHANGES[-1]["Seq"] if CHANGES else 0
        if "last" in query: return h._json({"Changes": CHANGES[-1:], "Done": True, "Last": last})
        found = re.search(r"since=(\d+)", query)
        since = int(found.group(1)) if found else 0
        page = [c for c in CHANGES if c["Seq"] > since][:100]
        return h._json({"Changes": page, "Done": True, "Last": page[-1]["Seq"] if page else since})
    m = re.match(r"^/instances/([^/]+)/metadata/RemoteAET$", p)
    if m: return h._send(b"SCANNER1", "text/plain")
    m = re.match(r"^/series/([^/]+)/instances$", p)
    if m and m.group(1) in all_series(): return h._json(instances_of(all_series()[m.group(1)]))
    m = re.match(r"^/instances/([^/]+)/(tags|preview|pdf|file)$", p)
    if m:
        series = find_instance(m.group(1))
        if not series: return h._json({"error": "nf"}, 404)
        kind = m.group(2)
        log("instance", {"id": m.group(1), "what": kind})
        if kind == "tags":
            return h._json({"0010,0010": {"Name": "PatientName", "Type": "String", "Value": "DOE^JOHN"},
                            "0008,0060": {"Name": "Modality", "Type": "String", "Value": series["MainDicomTags"].get("Modality", "")},
                            "0008,1032": {"Name": "ProcedureCodeSequence", "Type": "Sequence",
                                          "Value": [{"0008,0100": {"Name": "CodeValue", "Type": "String", "Value": "CT-CHEST"}}]},
                            "7fe0,0010": {"Name": "PixelData", "Type": "Null", "Value": None}})
        if kind == "pdf": return h._send(PDF, "application/pdf")
        if kind == "file": return h._send(b"DICM-one-file", "application/dicom")
        return h._send(JPEG, "image/jpeg")
    m = re.match(r"^/series/([^/]+)/nifti$", p)
    if m and m.group(1) in all_series(): log("nifti", {"id": m.group(1)}); return h._send(b"\x1f\x8bfake-nifti", "application/gzip")
    if p == "/jobs":
        jobs = list(JOBS.values()) + ([demo_job(j) | {"CreationTime": "20261118T090000", "Priority": 0} for j in DEMO_JOBS] if DEMO else [])
        return h._json(jobs if "expand" in query else [j["ID"] for j in jobs])
    if p.startswith("/jobs/") and p[6:] in JOBS: return h._json(JOBS[p[6:]])
    if p == "/peers": return h._json(PEERS if "expand" in query else list(PEERS))
    if p == "/dicom-web/servers": return h._json(DICOMWEB if "expand" in query else list(DICOMWEB))
    m = re.match(r"^/queries/([^/]+)/answers$", p)
    if m: return h._json(REMOTE)
    if p in ("/worklists", "/worklists/"):
        return h._json([dict(w["Tags"], ID=w["ID"]) for w in WORKLISTS.values()])
    m = re.match(r"^/worklists/([^/]+)$", p)
    if m: return h._json(WORKLISTS[m.group(1)]) if m.group(1) in WORKLISTS else h._json({"error": "nf"}, 404)
    if p == "/tools/log-level": return h._send(LOG_LEVEL["level"].encode(), "text/plain")
    if p == "/tools/metrics-prometheus": return h._send(b"orthanc_count_studies 1\n", "text/plain")
    m = re.match(r"^/plugins/([^/]+)$", p)
    if m: return h._json({"ID": m.group(1), "Version": "1.0", "Description": f"The {m.group(1)} plugin"})
    if p.startswith("/stone-webviewer") or p.startswith("/volview") or p.startswith("/wsi"):
        log("proxy", {"path": h.path})
        return h._send(b"<!doctype html><title>viewer</title>", "text/html")
    return None


def extra_post(h, path, body):
    p = path.split("?")[0]
    data = json.loads(body or b"{}") if body[:1] in (b"{", b"[") else {}
    m = re.match(r"^/(studies|series)/([^/]+)/(modify|anonymize)$", p)
    if m:
        kind, rid, action = m.groups()
        if rid not in (all_studies() if kind == "studies" else all_series()): return h._json({"error": "nf"}, 404)
        log("modify", {"kind": kind, "id": rid, "action": action, "body": data})
        if DEMO and action == "modify" and kind == "studies": apply_replace(rid, data.get("Replace", {}))   # the tests keep their fixture
        if action == "anonymize" and kind == "studies" and not data.get("Asynchronous"):
            # Synchronous: Orthanc answers with the new study at once (the anonymized copy of a share link).
            source = all_studies()[rid]
            copy_id = f"{len(ANON) + 1:08x}-a0a0a0a0-a0a0a0a0-a0a0a0a0-a0a0a0a0"
            replace = data.get("Replace", {})
            ANON[copy_id] = {**source, "ID": copy_id,
                             "MainDicomTags": {**source["MainDicomTags"], "StudyInstanceUID": f"2.25.{len(ANON) + 1}"},
                             "PatientMainDicomTags": {"PatientName": replace.get("PatientName", "ANONYMIZED"),
                                                      "PatientID": replace.get("PatientID", "ANON")}}
            return h._json({"ID": copy_id, "Path": f"/studies/{copy_id}", "Type": "Study"})
        return h._json(instant_job("ResourceModification", {"ID": rid}))
    if p in ("/tools/create-archive", "/tools/create-media"):
        log("archive", {"tool": p, "body": data})
        if p == "/tools/create-media": return h._send(media_zip(), "application/zip")
        return h._send(b"PK\x03\x04several-studies", "application/zip")
    if p == "/tools/create-dicom":
        log("create_dicom", {"parent": data.get("Parent"), "tags": data.get("Tags"), "content": (data.get("Content") or "")[:40]})
        study = all_studies().get(data.get("Parent"))
        if not study: return h._json({"error": "nf"}, 404)
        if DEMO:
            uid = data["Tags"]["SeriesInstanceUID"]
            sid = next((s for s, v in DEMO_SERIES.items() if v["MainDicomTags"]["SeriesInstanceUID"] == uid), None)
            if not sid:
                sid = f"{len(DEMO_SERIES) + 500:08x}-0000-0000-0000-00000000000c"
                DEMO_SERIES[sid] = {"ID": sid, "ParentStudy": study["ID"], "Instances": [],
                                    "MainDicomTags": {"SeriesInstanceUID": uid, "Modality": data["Tags"]["Modality"],
                                                      "SeriesDescription": data["Tags"]["SeriesDescription"], "SeriesNumber": "9000"}}
                study["Series"].append(sid)
            DEMO_SERIES[sid]["Instances"].append(f"{len(DEMO_SERIES) + 500:08x}-{len(DEMO_SERIES[sid]['Instances']):08x}")
        return h._json({"ID": "new-instance", "Path": "/instances/new-instance"})
    m = re.match(r"^/jobs/([^/]+)/(cancel|pause|resume|resubmit)$", p)
    if m:
        log("job_action", {"id": m.group(1), "action": m.group(2)})
        if m.group(1) in JOBS: JOBS[m.group(1)]["State"] = {"cancel": "Failure", "pause": "Paused", "resume": "Running", "resubmit": "Pending"}[m.group(2)]
        return h._json({})
    m = re.match(r"^/peers/([^/]+)/store$", p)
    if m:
        log("store", {"peer": unquote(m.group(1)), **data})
        return h._json(instant_job("OrthancPeerStore", {"Peer": [unquote(m.group(1))], "InstancesCount": 3}))
    m = re.match(r"^/dicom-web/servers/([^/]+)/(stow|get|retrieve)$", p)
    if m:
        name, action = unquote(m.group(1)), m.group(2)
        log("dicomweb_client", {"server": name, "action": action, "body": data})
        if action == "stow": return h._json(instant_job("DicomWebStowClient", {"InstancesCount": 3}))
        if action == "retrieve": return h._json(instant_job("DicomWebRetrieve", {}))
        return h._json([{"0020000D": {"vr": "UI", "Value": [r["StudyInstanceUID"]]}, "00100010": {"vr": "PN", "Value": [{"Alphabetic": r["PatientName"]}]},
                         "00100020": {"vr": "LO", "Value": [r["PatientID"]]}, "00080020": {"vr": "DA", "Value": [r["StudyDate"]]},
                         "00081030": {"vr": "LO", "Value": [r["StudyDescription"]]}, "00080061": {"vr": "CS", "Value": r["ModalitiesInStudy"].split("\\")}}
                        for r in REMOTE])
    m = re.match(r"^/modalities/([^/]+)/(query|move)$", p)
    if m:
        alias = unquote(m.group(1))
        if alias not in MODALITIES: return h._json({"error": "nf"}, 404)
        log("qr", {"alias": alias, "action": m.group(2), "body": data})
        if m.group(2) == "query": return h._json({"ID": "q1", "Path": "/queries/q1"})
        if DEMO:  # the "retrieved" study appears in the archive
            uid = data["Resources"][0]["StudyInstanceUID"]
            r = next((x for x in REMOTE if x["StudyInstanceUID"] == uid), None)
            if r and not any(s["MainDicomTags"]["StudyInstanceUID"] == uid for s in DEMO_STUDIES.values()):
                sid = demo_add_study(200 + len(DEMO_STUDIES), r["PatientName"], r["PatientID"], r["StudyDate"], r["StudyDescription"],
                                     "CT", "ABDOMEN", 4, _time.strftime("%Y%m%dT%H%M%S"))
                DEMO_STUDIES[sid]["MainDicomTags"]["StudyInstanceUID"] = uid
        return h._json(instant_job("DicomMoveScu", {}))
    if p == "/worklists/create":
        wid = str(_uuid.uuid4())
        WORKLISTS[wid] = {"ID": wid, "Tags": data.get("Tags", {})}
        log("worklists", {"create": data.get("Tags", {})})
        return h._json({"ID": wid, "Path": f"/worklists/{wid}"})
    return None


def extra_delete(h, path):
    m = re.match(r"^/(studies|series|instances|worklists)/([^/]+)$", path)
    if not m: return None
    kind, rid = m.groups()
    log("delete", {"kind": kind, "id": rid})
    if kind == "worklists":
        return h._json({}) if WORKLISTS.pop(rid, None) else h._json({"error": "nf"}, 404)
    if kind == "studies":
        if rid not in all_studies(): return h._json({"error": "nf"}, 404)
        if DEMO: del DEMO_STUDIES[rid]
        ANON.pop(rid, None)
        return h._json({})
    if kind == "series":
        if rid not in all_series(): return h._json({"error": "nf"}, 404)
        if DEMO:
            series = DEMO_SERIES.pop(rid); study = DEMO_STUDIES.get(series["ParentStudy"])
            if study and rid in study["Series"]: study["Series"].remove(rid)
        return h._json({})
    series = find_instance(rid)
    if not series: return h._json({"error": "nf"}, 404)
    if DEMO: series["Instances"].remove(rid)
    return h._json({})


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def _send(self, body, ctype, code=200, extra=None):
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items(): self.send_header(k, v)
        self.end_headers(); self.wfile.write(body)
        return True

    def _json(self, obj, code=200): return self._send(json.dumps(obj).encode(), "application/json", code)
    def _gate(self):
        """Decides whether this request may pass, like Orthanc's authentication / allowed-clients setup.
        Answers 401 or 403 itself and returns False when it may not."""
        auth = self.headers.get("Authorization")
        log("auth", {"method": self.command, "path": self.path.split("?")[0], "authorization": auth})
        if MODE["auth"] == "none": return True
        if MODE["auth"] == "deny":
            self._json({"error": "forbidden"}, 403); return False
        if auth == "Basic " + base64.b64encode(b"orthanc:s3cr3tpw").decode(): return True
        self._json({"error": "unauthorized"}, 401); return False
    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""   # always consumed BEFORE answering, like a real server

    def do_GET(self):
        # Stands in for GitHub's latest release (the portal's update check); no Orthanc account.
        if self.path == "/__mock/cd-viewer.zip":   # the fake Weasis package (tests/fake_cd_viewer.py, written by lib.sh)
            path = os.path.join(os.environ.get("MOCK_LOG_DIR", "/tmp/portal-tests"), "fake-cd-viewer.zip")
            if not os.path.exists(path): return self._json({"error": "no package"}, 404)
            return self._send(open(path, "rb").read(), "application/zip")
        if self.path == "/__mock/github/latest":
            if GITHUB.get("status", 200) != 200: return self._json({"message": "x"}, GITHUB["status"])
            return self._json(GITHUB["release"])
        if not self._gate(): return
        p = self.path.split("?")[0]
        if p == "/system": return self._json({"Version": VERSION, "Capabilities": ["HasExtendedFind", "HasExtendedChanges"]})
        if p == "/plugins":   # MOCK_PLUGINS=stone-webviewer,volview adds plugins (the dev environment)
            return self._json(["dicom-web", "ohif", "explorer.html"] + [x for x in os.environ.get("MOCK_PLUGINS", "").split(",") if x])
        if extra_get(self, p, self.path.split("?", 1)[1] if "?" in self.path else "") is not None: return
        if p == "/tools/labels": return self._json(all_labels())
        if p == "/studies": return self._json(list(all_studies().keys()))   # every study's ID
        m = re.match(r"^/studies/([^/]+)/instances$", p)   # every image of a study (copy to another server)
        if m and m.group(1) in all_studies():
            series = [all_series()[x] for x in all_studies()[m.group(1)]["Series"] if x in all_series()]
            return self._json([i for se in series for i in instances_of(se)])
        m = re.match(r"^/instances/([^/]+)/file$", p)
        if m:
            log("file", {"id": m.group(1)})
            return self._send(b"DICM-file-" + m.group(1).encode(), "application/dicom")
        m = re.match(r"^/studies/([^/]+)/labels$", p)
        if m: return self._json(sorted(LABELS.get(m.group(1), ())))
        if DEMO:
            if p == "/statistics": return self._json({"CountStudies": len(DEMO_STUDIES), "CountSeries": len(DEMO_SERIES)})
            m = re.match(r"^/studies/([^/]+)(/series)?$", p)
            if m and m.group(1) in DEMO_STUDIES:
                study = DEMO_STUDIES[m.group(1)]
                return self._json([DEMO_SERIES[s] for s in study["Series"]] if m.group(2) else with_labels(study))
            if p.startswith("/series/") and p[8:] in DEMO_SERIES: return self._json(DEMO_SERIES[p[8:]])
            m = re.match(r"^/instances/(([0-9a-f]{8})-[0-9a-f]{8})/(rendered|preview)$", p)
            if m:
                width = re.search(r"width=(\d+)", self.path)
                return self._send(demo_png(width.group(1) if width else 128, int(m.group(2), 16)), "image/png")
        if p == "/statistics": return self._json({"CountPatients": 1, "CountStudies": 1, "CountSeries": 2, "CountInstances": 8})
        if p == "/modalities": return self._json(MODALITIES)
        if p == f"/jobs/{JOB_ID}":
            log("jobs", {"path": p}); job_polls["n"] += 1
            running = job_polls["n"] < 3
            return self._json({"ID": JOB_ID, "Type": "DicomModalityStore", "State": "Running" if running else "Success",
                               "Progress": 40 if running else 100, "EffectiveRuntime": 1.5 if running else 3.5,
                               "Content": {"InstancesCount": 120, "FailedInstancesCount": 0, "RemoteAet": "RIS"}})
        if DEMO and p.startswith("/jobs/") and p[6:] in DEMO_JOBS:
            return self._json(demo_job(p[6:]))
        if p == f"/studies/{STUDY_ID}": return self._json(with_labels(STUDY))
        if p == f"/studies/{STUDY_ID}/series": return self._json([SERIES, SR_SERIES])
        if p == f"/series/{SERIES_ID}": return self._json(SERIES)
        if p == f"/series/{SR_ID}": return self._json(SR_SERIES)
        m = re.match(r"^/instances/([^/]+)/rendered$", p)
        if m:
            iid = m.group(1)
            log("rendered", {"id": iid, "query": self.path.split("?", 1)[-1], "accept": self.headers.get("Accept")})
            if iid in ("a1", "a3"): return self._send(JPEG, "image/jpeg")
            return self._send(b'{"Message":"Unsupported image"}', "application/json", 415)   # a2 + report images: no preview
        if p.endswith("/archive") or p.endswith("/media"):
            data = b"PK\x03\x04fake-zip-content" * 1000
            self.send_response(200); self.send_header("Content-Type", "application/zip"); self.send_header("Transfer-Encoding", "chunked"); self.end_headers()
            for i in range(0, len(data), 4096):
                chunk = data[i:i + 4096]; self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
            self.wfile.write(b"0\r\n\r\n"); return
        if p.startswith("/dicom-web/") or p == "/wado" or p.startswith("/ohif"):
            log("proxy", {"path": self.path, "host": self.headers.get("Host"), "accept": self.headers.get("Accept"),
                          "ae": self.headers.get("Accept-Encoding"), "cookie": self.headers.get("Cookie")})
            if p.startswith("/ohif"):
                if p.endswith(".js"): return self._send(b"console.log('ohif')", "text/javascript", extra={"ETag": '"abc"'})
                return self._send(b"<!doctype html><title>OHIF</title>", "text/html")
            if p == "/wado": return self._send(b"DICM-fake", "application/dicom")
            return self._json([{"0020000D": {"vr": "UI", "Value": ["1.2.3"]}}])
        self._json({"error": "not found"}, 404)

    def do_POST(self):
        body = self._body()
        if self.path == "/__mock/github":   # test control: the release the fake GitHub publishes, or its HTTP status
            GITHUB.update(json.loads(body)); return self._json(GITHUB)
        if self.path == "/__mock/auth":   # test control, not part of Orthanc: switches the protection mode
            MODE["auth"] = json.loads(body)["mode"]; return self._json({"mode": MODE["auth"]})
        if self.path == "/__mock/change":   # test control: Orthanc records a change (e.g. a study became stable)
            change = json.loads(body); change["Seq"] = (CHANGES[-1]["Seq"] if CHANGES else 0) + 1; CHANGES.append(change)
            return self._json(change)
        if self.path == "/__mock/oauth/token":   # stands in for Microsoft Entra ID (client credentials)
            from urllib.parse import parse_qs
            form = {k: v[0] for k, v in parse_qs(body.decode() if isinstance(body, bytes) else body).items()}
            OAUTH.append({k: form.get(k, "") for k in ("grant_type", "client_id", "scope")})
            if form.get("client_secret") != "good-secret":
                return self._json({"error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided.\r\nTrace ID: 1"}, 401)
            return self._json({"token_type": "Bearer", "expires_in": 3599, "access_token": "mock-access-token"})
        if self.path == "/__mock/oauth/requests":   # test control: the token requests received so far
            return self._json(OAUTH)
        if self.path == "/__mock/copies":   # test control: how many studies a search finds
            MODE["copies"] = int(json.loads(body)["n"]); return self._json({"copies": MODE["copies"]})
        if not self._gate(): return
        m = re.match(r"^/modalities/([^/]+)/echo$", self.path)
        if m:
            alias = unquote(m.group(1))
            if alias not in MODALITIES: return self._json({"error": "nf"}, 404)
            return self._json({"Message": "Error in the network protocol"}, 500) if alias == "down" else self._json({})
        if extra_post(self, self.path, body) is not None: return
        if self.path == "/tools/find":
            q = json.loads(body); log("find", q)
            if DEMO: return self._json(demo_find(q))
            query = q.get("Query", {})
            if q["Level"] != "Study" and ("SeriesInstanceUID" in query or "SOPInstanceUID" in query):
                # "Does this series / image belong to that study?" (the portal asks it for guests).
                # Series and images whose UID starts with 8.8. belong to some OTHER study.
                ours = query.get("StudyInstanceUID") in (STUDY["MainDicomTags"]["StudyInstanceUID"], STUDY2["MainDicomTags"]["StudyInstanceUID"])
                foreign = any(str(query.get(k, "")).startswith("8.8.") for k in ("SeriesInstanceUID", "SOPInstanceUID"))
                if not ours or foreign: return self._json([])
                if q["Level"] == "Instance" and "SOPInstanceUID" not in query:   # all the images of a series
                    return self._json([{"ID": f"i-{u}", "Type": "Instance", "MainDicomTags": {"SOPInstanceUID": u}} for u in ("6.7", "6.8", "3")])
                return self._json([SERIES])
            if q["Level"] != "Study": return self._json([with_parent_uid(SERIES, q)])
            if "Labels" in q and q.get("Expand") is False:   # ids of the studies with ALL these labels (share by label)
                found = [st["ID"] for st in all_studies().values() if set(q["Labels"]) <= LABELS.get(st["ID"], set())]
                return self._json(found[:q.get("Limit", 100)])
            if "StudyInstanceUID" in query:   # one study (or a list "a\\b"), as Orthanc matches UIDs
                wanted = str(query["StudyInstanceUID"]).split("\\")
                found = [with_labels(st) for st in all_studies().values() if st["MainDicomTags"]["StudyInstanceUID"] in wanted]
                return self._json(found if q.get("Expand", True) else [st["ID"] for st in found])
            found = [with_labels(STUDY)] * MODE["copies"]
            return self._json(found[q.get("Since", 0):q.get("Since", 0) + q.get("Limit", 100)])
        if self.path == "/tools/count-resources":
            q = json.loads(body); log("count", q)
            studies = DEMO_STUDIES.values() if DEMO else [STUDY]
            return self._json({"Count": sum(1 for s in studies if set(q.get("Labels", [])) <= LABELS.get(s["ID"], set()))})
        if DEMO and self.path == "/instances":
            if body[:4] == b"BAD!": return self._json({"error": "bad"}, 400)
            return self._json({"ID": "x", "Status": "Success", "ParentStudy": demo_upload()})
        m = re.match(r"^/modalities/([^/]+)/store$", self.path)
        if DEMO and m and unquote(m.group(1)) in MODALITIES:
            return self._json(demo_store(unquote(m.group(1)), json.loads(body)))
        if self.path == "/modalities/ris/store":
            log("store", json.loads(body)); return self._json({"ID": JOB_ID, "Path": f"/jobs/{JOB_ID}"})
        if self.path == "/instances":
            log("upload", {"len": len(body), "te": self.headers.get("Transfer-Encoding")})
            if body[:4] == b"BAD!": return self._json({"error": "bad"}, 400)
            uid = uploaded_study_uid(body)
            return self._json({"ID": "x", "Status": "Success", "ParentStudy": study_for_upload(uid) if uid else STUDY_ID})
        self._json({"error": "nf"}, 404)

    def do_PUT(self):
        raw = self._body()
        if not self._gate(): return
        if self.path == "/tools/log-level":
            LOG_LEVEL["level"] = raw.decode().strip(); log("loglevel", {"level": LOG_LEVEL["level"]}); return self._send(b"", "text/plain")
        m = re.match(r"^/studies/([^/]+)/labels/([^/]+)$", self.path)
        if m:
            if m.group(1) not in (DEMO_STUDIES if DEMO else {STUDY_ID: 1, STUDY2_ID: 1, **ANON}): return self._json({"error": "nf"}, 404)
            log("labels", {"put": m.group(2), "study": m.group(1)})
            LABELS.setdefault(m.group(1), set()).add(unquote(m.group(2))); return self._json({})
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m: return self._json({"error": "nf"}, 404)
        alias = unquote(m.group(1)); body = json.loads(raw or b"{}"); log("modput", {"alias": alias, "body": body})
        MODALITIES[alias] = body; return self._json({})

    def do_DELETE(self):
        self._body()
        if not self._gate(): return
        if extra_delete(self, self.path) is not None: return
        m = re.match(r"^/studies/([^/]+)/labels/([^/]+)$", self.path)
        if m:
            labels = LABELS.get(m.group(1), set()); labels.discard(unquote(m.group(2)))
            if not labels: LABELS.pop(m.group(1), None)
            return self._json({})
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m or unquote(m.group(1)) not in MODALITIES: return self._json({"error": "nf"}, 404)
        del MODALITIES[unquote(m.group(1))]; return self._json({})


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 18042), H).serve_forever()
