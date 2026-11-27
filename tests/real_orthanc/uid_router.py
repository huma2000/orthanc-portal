"""Experiment: one address in front of several Orthancs that picks the server of each DICOMweb /
WADO request from the study UID it names (what the portal would do with several servers).

    python uid_router.py PORT name=http://host:port [name=http://host:port ...]

* /dicom-web/... and /wado: the study UID is read from the path (studies/{uid}/...) or the query
  (StudyInstanceUID, 0020000D, studyUID). The server is the one "pinned" for that UID
  (POST /__pin?uid=..&server=..: what "Open" would do), else the first server that has the study.
  A request naming no study UID is logged as NO-UID and goes to the first server.
* Everything else (Orthanc's REST API, /ohif/...) goes to the first server.
Every request is logged to stdout: method, path, server, status, reason.
"""
import base64, http.client, json, re, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

PORT = int(sys.argv[1])
SERVERS = [tuple(a.split("=", 1)) for a in sys.argv[2:]]
AUTH = "Basic " + base64.b64encode(b"orthanc:s3cr3tpw").decode()
HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers",
       "transfer-encoding", "upgrade", "content-length"}
pins, where = {}, {}
lock = threading.Lock()


def conn(url):
    u = urlsplit(url)
    return http.client.HTTPConnection(u.hostname, u.port, timeout=60)


def has_study(url, uid):
    c = conn(url)
    body = json.dumps({"Level": "Study", "Query": {"StudyInstanceUID": uid}, "Limit": 1})
    c.request("POST", "/tools/find", body, {"Authorization": AUTH, "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status == 200 and json.loads(data) != []


def study_uid(path, query):
    m = re.match(r"^/dicom-web/studies/([0-9.]+)", path)
    if m:
        return m.group(1)
    q = parse_qs(query)
    for k in ("StudyInstanceUID", "0020000D", "studyUID"):
        if q.get(k):
            return q[k][0]
    return None


def pick(path, query):
    if not (path.startswith("/dicom-web/") or path == "/wado"):
        return SERVERS[0], "not DICOMweb"
    uid = study_uid(path, query)
    if uid is None:
        return SERVERS[0], "NO-UID"
    with lock:
        if uid in pins:
            return next(s for s in SERVERS if s[0] == pins[uid]), f"pinned {uid}"
        if uid in where:
            return where[uid], f"remembered {uid}"
    for s in SERVERS:
        if has_study(s[1], uid):
            with lock:
                where[uid] = s
            return s, f"found {uid}"
    return SERVERS[0], f"nowhere {uid}"


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def handle_any(self):
        u = urlsplit(self.path)
        if u.path == "/__pin":
            q = parse_qs(u.query)
            with lock:
                pins[q["uid"][0]] = q["server"][0]
            self.send_response(204); self.send_header("Content-Length", "0"); self.end_headers()
            print(f"PIN {q['uid'][0]} -> {q['server'][0]}", flush=True)
            return
        (name, url), why = pick(u.path, u.query)
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
        c = conn(url)
        c.request(self.command, self.path, body, headers)
        r = c.getresponse()
        data = r.read()
        self.send_response(r.status)
        for k, v in r.getheaders():
            if k.lower() not in HOP:
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        if u.path.startswith("/dicom-web/") or u.path == "/wado":
            print(f"{self.command} {self.path[:150]} -> {name} [{r.status}] ({why})", flush=True)

    do_GET = do_POST = do_PUT = do_DELETE = handle_any


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
