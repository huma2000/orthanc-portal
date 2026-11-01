"""A fake, recent Orthanc used by the portal's automated tests (nothing here talks to a real PACS).

It answers just enough of Orthanc's REST API, the DICOMweb plugin and the OHIF plugin for the tests,
and records what it receives in $MOCK_LOG_DIR/*.log so tests can check what the portal asked.

    python3 mock_orthanc.py [port]          (default 18042; user "orthanc", password "s3cr3tpw")
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
    def _auth_ok(self): return self.headers.get("Authorization") == "Basic " + base64.b64encode(b"orthanc:s3cr3tpw").decode()
    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""   # always consumed BEFORE answering, like a real server

    def do_GET(self):
        if not self._auth_ok(): return self._json({"error": "unauthorized"}, 401)
        p = self.path.split("?")[0]
        if p == "/system": return self._json({"Version": VERSION, "Capabilities": ["HasExtendedFind", "HasExtendedChanges"]})
        if p == "/plugins": return self._json(["dicom-web", "ohif", "explorer.html"])
        if p == "/modalities": return self._json(MODALITIES)
        if p == f"/jobs/{JOB_ID}":
            log("jobs", {"path": p}); job_polls["n"] += 1
            return self._json({"ID": JOB_ID, "Type": "DicomModalityStore", "State": "Running" if job_polls["n"] < 3 else "Success", "Progress": 40 if job_polls["n"] < 3 else 100})
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
        if not self._auth_ok(): return self._json({"error": "unauthorized"}, 401)
        m = re.match(r"^/modalities/([^/]+)/echo$", self.path)
        if m:
            alias = unquote(m.group(1))
            if alias not in MODALITIES: return self._json({"error": "nf"}, 404)
            return self._json({"Message": "Error in the network protocol"}, 500) if alias == "down" else self._json({})
        if self.path == "/tools/find":
            q = json.loads(body); log("find", q)
            return self._json([STUDY] if q["Level"] == "Study" else [SERIES])
        if self.path == "/modalities/ris/store":
            log("store", json.loads(body)); return self._json({"ID": JOB_ID, "Path": f"/jobs/{JOB_ID}"})
        if self.path == "/instances":
            log("upload", {"len": len(body), "te": self.headers.get("Transfer-Encoding")})
            if body[:4] == b"BAD!": return self._json({"error": "bad"}, 400)
            return self._json({"ID": "x", "Status": "Success", "ParentStudy": STUDY_ID})
        self._json({"error": "nf"}, 404)

    def do_PUT(self):
        raw = self._body()
        if not self._auth_ok(): return self._json({"error": "unauthorized"}, 401)
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m: return self._json({"error": "nf"}, 404)
        alias = unquote(m.group(1)); body = json.loads(raw or b"{}"); log("modput", {"alias": alias, "body": body})
        MODALITIES[alias] = body; return self._json({})

    def do_DELETE(self):
        self._body()
        if not self._auth_ok(): return self._json({"error": "unauthorized"}, 401)
        m = re.match(r"^/modalities/([^/]+)$", self.path)
        if not m or unquote(m.group(1)) not in MODALITIES: return self._json({"error": "nf"}, 404)
        del MODALITIES[unquote(m.group(1))]; return self._json({})


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 18042), H).serve_forever()
