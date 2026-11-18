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
STUDY = {"ID": STUDY_ID, "Type": "Study", "Series": [SERIES_ID, SR_ID],
         "MainDicomTags": {"StudyInstanceUID": "1.2.826.0.1.3680043.8.498.1", "StudyDate": "20240517",
                           "StudyDescription": "CT CHEST <script>alert(1)</script>", "AccessionNumber": "ACC1"},
         "PatientMainDicomTags": {"PatientName": "DOE^JOHN", "PatientID": "P-001", "PatientBirthDate": "19800502", "PatientSex": "M"},
         "RequestedTags": {"ModalitiesInStudy": "CT\\SR", "NumberOfStudyRelatedInstances": "120"}}
SERIES = {"ID": SERIES_ID, "ParentStudy": STUDY_ID, "Instances": ["i1", "i2", "i3"],
          "MainDicomTags": {"SeriesInstanceUID": "1.2.3.4", "Modality": "CT", "SeriesDescription": "Chest 1mm", "SeriesNumber": "2", "BodyPartExamined": "CHEST"}}
SR_SERIES = {"ID": SR_ID, "ParentStudy": STUDY_ID, "Instances": ["s1", "s2", "s3", "s4", "s5"],
             "MainDicomTags": {"SeriesInstanceUID": "1.2.3.5", "Modality": "SR", "SeriesDescription": "Dose report", "SeriesNumber": "9"}}
JPEG = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00fake-thumbnail-bytes\xff\xd9"
MODALITIES = {"ris": {"AET": "RIS", "Host": "10.0.0.20", "Port": 104, "Manufacturer": "Generic", "AllowStore": True, "AllowFind": True},
              "archive-only": {"AET": "ARC", "Host": "10.0.0.21", "Port": 104, "AllowStore": False},
              "down": {"AET": "DOWN", "Host": "10.9.9.9", "Port": 104, "AllowStore": True, "AllowEcho": True}}
job_polls = {"n": 0}
MODE = {"auth": os.environ.get("MOCK_AUTH", "basic")}   # see the docstring


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


def demo_add_study(n, name, pid, date, desc, modality, body, images, received):
    # Hex digits and dashes only, like real Orthanc ids (the portal refuses anything else).
    sid, ser = f"{n:08x}-0000-0000-0000-00000000000a", f"{n:08x}-0000-0000-0000-00000000000b"
    DEMO_SERIES[ser] = {"ID": ser, "ParentStudy": sid, "Instances": [f"demo{n:04d}-i{k}" for k in range(images)],
                        "MainDicomTags": {"SeriesInstanceUID": f"1.2.999.{n}.1", "Modality": modality,
                                          "SeriesDescription": desc, "SeriesNumber": "1", "BodyPartExamined": body}}
    DEMO_STUDIES[sid] = {"ID": sid, "Type": "Study", "Series": [ser], "LastUpdate": received,
                         "MainDicomTags": {"StudyInstanceUID": f"1.2.999.{n}", "StudyDate": date, "StudyTime": "101500",
                                           "StudyDescription": desc, "AccessionNumber": f"ACC{n:04d}"},
                         "PatientMainDicomTags": {"PatientName": name, "PatientID": pid, "PatientBirthDate": "19750101", "PatientSex": "F" if n % 2 else "M"},
                         "RequestedTags": {"ModalitiesInStudy": modality, "NumberOfStudyRelatedInstances": str(images)}}
    return sid


if DEMO:
    for i, (name, mod, desc, body) in enumerate([
            ("GARCIA^MARIA", "CT", "CT HEAD WITHOUT CONTRAST", "HEAD"), ("SMITH^JOHN", "MR", "MRI LUMBAR SPINE", "LSPINE"),
            ("NGUYEN^LINH", "US", "ABDOMEN ULTRASOUND", "ABDOMEN"), ("MULLER^HANS", "DX", "CHEST PA AND LATERAL", "CHEST"),
            ("ROSSI^GIULIA", "MG", "SCREENING MAMMOGRAPHY", "BREAST"), ("KOWALSKI^PIOTR", "CT", "CT CHEST WITH CONTRAST", "CHEST"),
            ("DUBOIS^CLAIRE", "MR", "MRI RIGHT KNEE", "KNEE"), ("OKAFOR^EMEKA", "CR", "LEFT WRIST 3 VIEWS", "HAND"),
            ("SATO^YUKI", "US", "THYROID ULTRASOUND", "NECK"), ("BROWN^EMMA", "CT", "CT ABDOMEN PELVIS", "ABDOMEN"),
            ("SILVA^RAFAEL", "XA", "CORONARY ANGIOGRAPHY", "HEART"), ("JOHANSSON^ANNA", "MR", "MRI BRAIN", "HEAD"),
            ("PATEL^RAVI", "DX", "PELVIS AP", "PELVIS"), ("WILLIAMS^OLIVIA", "NM", "BONE SCAN WHOLE BODY", "WHOLEBODY")]):
        demo_add_study(i + 1, name, f"P-{1000 + i}", f"202611{17 - i:02d}", desc, mod, body, 3 + i % 4, f"202611{17 - i:02d}T09{i:02d}00")


def demo_find(q):
    """Enough of /tools/find for the demo: patient / description filters, modalities, dates, sorting, paging."""
    query = q.get("Query", {})
    def like(value, pattern):
        p = (pattern or "").strip("*").lower()
        return not p or p in (value or "").lower()
    if q["Level"] == "Study":
        items = list(DEMO_STUDIES.values())
        items = [s for s in items if like(s["PatientMainDicomTags"]["PatientName"], query.get("PatientName"))
                 and like(s["PatientMainDicomTags"]["PatientID"], query.get("PatientID"))
                 and like(s["MainDicomTags"]["StudyDescription"], query.get("StudyDescription"))]
        if query.get("ModalitiesInStudy"):
            wanted = set(query["ModalitiesInStudy"].split("\\"))
            items = [s for s in items if set(s["RequestedTags"]["ModalitiesInStudy"].split("\\")) & wanted]
        if query.get("StudyDate"):
            lo, _, hi = query["StudyDate"].partition("-")
            items = [s for s in items if (not lo or s["MainDicomTags"]["StudyDate"] >= lo) and (not hi or s["MainDicomTags"]["StudyDate"] <= hi)]
        by_received = any(o.get("Key") == "LastUpdate" for o in q.get("OrderBy", []))
        items.sort(key=(lambda s: s["LastUpdate"]) if by_received else (lambda s: s["MainDicomTags"]["StudyDate"]), reverse=True)
    else:
        items = list(DEMO_SERIES.values())
        if query.get("Modality"):
            wanted = set(query["Modality"].split("\\"))
            items = [s for s in items if s["MainDicomTags"]["Modality"] in wanted]
        items = [s for s in items if like(s["MainDicomTags"]["SeriesDescription"], query.get("SeriesDescription"))]
        if query.get("BodyPartExamined"):   # exact match, like Orthanc
            items = [s for s in items if s["MainDicomTags"]["BodyPartExamined"].upper() == query["BodyPartExamined"].upper()]
    since = q.get("Since", 0)
    return items[since:since + q.get("Limit", 100)]


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


def log(name, obj):
    with open(os.path.join(LOG_DIR, name + ".log"), "a") as f:
        f.write(json.dumps(obj) + "\n")


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def _send(self, body, ctype, code=200, extra=None):
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items(): self.send_header(k, v)
        self.end_headers(); self.wfile.write(body)

    def _json(self, obj, code=200): self._send(json.dumps(obj).encode(), "application/json", code)
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
        if not self._gate(): return
        p = self.path.split("?")[0]
        if p == "/system": return self._json({"Version": VERSION, "Capabilities": ["HasExtendedFind", "HasExtendedChanges"]})
        if p == "/plugins": return self._json(["dicom-web", "ohif", "explorer.html"])
        if DEMO:
            if p == "/statistics": return self._json({"CountStudies": len(DEMO_STUDIES), "CountSeries": len(DEMO_SERIES)})
            m = re.match(r"^/studies/([^/]+)(/series)?$", p)
            if m and m.group(1) in DEMO_STUDIES:
                study = DEMO_STUDIES[m.group(1)]
                return self._json([DEMO_SERIES[s] for s in study["Series"]] if m.group(2) else study)
            if p.startswith("/series/") and p[8:] in DEMO_SERIES: return self._json(DEMO_SERIES[p[8:]])
            m = re.match(r"^/instances/(demo(\d+)-i\d+)/rendered$", p)
            if m:
                width = re.search(r"width=(\d+)", self.path)
                return self._send(demo_png(width.group(1) if width else 128, int(m.group(2))), "image/png")
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
        if p == f"/studies/{STUDY_ID}": return self._json(STUDY)
        if p == f"/studies/{STUDY_ID}/series": return self._json([SERIES, SR_SERIES])
        if p == f"/series/{SERIES_ID}": return self._json(SERIES)
        if p == f"/series/{SR_ID}": return self._json(SR_SERIES)
        m = re.match(r"^/instances/([^/]+)/rendered$", p)
        if m:
            iid = m.group(1)
            log("rendered", {"id": iid, "query": self.path.split("?", 1)[-1], "accept": self.headers.get("Accept")})
            if iid in ("i1", "i3"): return self._send(JPEG, "image/jpeg")
            return self._send(b'{"Message":"Unsupported image"}', "application/json", 415)   # i2 + report images: no preview
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
        if self.path == "/__mock/auth":   # test control, not part of Orthanc: switches the protection mode
            MODE["auth"] = json.loads(body)["mode"]; return self._json({"mode": MODE["auth"]})
        if not self._gate(): return
        m = re.match(r"^/modalities/([^/]+)/echo$", self.path)
        if m:
            alias = unquote(m.group(1))
            if alias not in MODALITIES: return self._json({"error": "nf"}, 404)
            return self._json({"Message": "Error in the network protocol"}, 500) if alias == "down" else self._json({})
        if self.path == "/tools/find":
            q = json.loads(body); log("find", q)
            if DEMO: return self._json(demo_find(q))
            return self._json([STUDY] if q["Level"] == "Study" else [SERIES])
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
            return self._json({"ID": "x", "Status": "Success", "ParentStudy": STUDY_ID})
        self._json({"error": "nf"}, 404)

    def do_PUT(self):
        raw = self._body()
        if not self._gate(): return
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m: return self._json({"error": "nf"}, 404)
        alias = unquote(m.group(1)); body = json.loads(raw or b"{}"); log("modput", {"alias": alias, "body": body})
        MODALITIES[alias] = body; return self._json({})

    def do_DELETE(self):
        self._body()
        if not self._gate(): return
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m or unquote(m.group(1)) not in MODALITIES: return self._json({"error": "nf"}, 404)
        del MODALITIES[unquote(m.group(1))]; return self._json({})


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 18042), H).serve_forever()
