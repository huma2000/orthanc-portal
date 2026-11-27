# Testing against real Orthanc servers (and real OHIF)

The automated suites (`tests/*.sh`, `ui_tests.js`) use a fake Orthanc. This folder holds what was used to
check the portal against **real Orthanc servers with the real DICOMweb and OHIF plugins**, in a real
browser. It was first used on 2026-11-27, on Windows 11, to:

1. reproduce the breaking change of the next DICOMweb plugin release (`AllowedHosts`) and check the fix;
2. test choosing the Orthanc server of each viewer request from the study UID (multi-server plan, phase 0).

Nothing here is needed to run or build the portal.

## What was downloaded and installed

Nothing was installed system-wide. Every file went into one scratch folder (the Claude Code session's
scratchpad), not into the repository:

| File | From | SHA-256 |
|---|---|---|
| `Orthanc-1.13.1.exe` (Orthanc 1.13.1, standalone, no installer) | <https://orthanc.uclouvain.be/downloads/windows-64/orthanc/1.13.1/Orthanc.exe> | `34cec0bb4eadacb47938b0c7c89c934b241e67b66998869c0bb52821b819e384` |
| `OrthancDicomWeb-1.24.dll` (latest release) | <https://orthanc.uclouvain.be/downloads/windows-64/orthanc-dicomweb/OrthancDicomWeb-1.24.dll> | `d4311cef4cf63fa580be30d3177f8f0ae03499ebcfc9e1364a4141291e9e99f9` |
| `OrthancDicomWeb-mainline.dll` (next release, unreleased) | <https://orthanc.uclouvain.be/downloads/windows-64/orthanc-dicomweb/OrthancDicomWeb-mainline.dll> | `021985e6aaed6da01836ada59d00734baad2a5d1420c59095d44889b8ac61bda` |
| `OrthancOHIF-1.8.dll` | <https://orthanc.uclouvain.be/downloads/windows-64/orthanc-ohif/OrthancOHIF-1.8.dll> | `e5fdd9ea928a000ab5c75481929a6f79c2fe2fc1caaed1b2fdaa066512ed15e0` |
| `Orthanc-1.12.10.exe` (first try, not used: DICOMweb 1.24 needs Orthanc 1.13.0 or newer) | <https://orthanc.uclouvain.be/downloads/windows-64/orthanc/1.12.10/Orthanc.exe> | `69a42675e3d72e415647399f8cbdf98302fe966aa1c0d13a00ea4f3557ccdc5e` |
| `puppeteer-core` 24.43.1 (npm, with its 80 dependencies), to drive the browser | `npm install puppeteer-core@24` in the scratch folder | (npm lock file) |

The browser is the **Microsoft Edge already installed** on the machine, started headless with its own
empty profile folder (so the person's Edge, profile and tabs are never touched). `puppeteer-core` does
not download a browser.

The other tools are in this folder and need only Python 3 and Node.js:

* `make_dicom.py`: writes small but complete CT series (64x64, 16-bit, 12 slices), no libraries needed.
* `uid_router.py`: one address in front of several Orthancs that sends each DICOMweb / WADO request to
  the server holding the study it names (or the one "pinned" for that study), and logs every decision.
* `ohif_check.js`: opens OHIF through the portal for each study, records the requests, checks that an
  image was drawn, and saves a screenshot.

## Set-up

Two Orthancs, each from its own folder with its own `orthanc.json`:

```json
{
  "Name": "ORTHANC_A", "HttpPort": 28042, "DicomPort": 24242, "DicomAet": "ORTHANC_A",
  "StorageDirectory": "<folder>/a/db", "IndexDirectory": "<folder>/a/db",
  "RemoteAccessAllowed": true, "AuthenticationEnabled": true,
  "RegisteredUsers": { "orthanc": "s3cr3tpw" },
  "Plugins": [ "<folder>/a/plugins" ],
  "DicomWeb": { "Enable": true, "Root": "/dicom-web/", "EnableWado": true, "WadoRoot": "/wado" },
  "OHIF": { "DataSource": "dicom-web" }
}
```

* **A** (port 28042): `OrthancDicomWeb-1.24.dll` + `OrthancOHIF-1.8.dll` in `a/plugins`.
* **B** (port 28043): `OrthancDicomWeb-mainline.dll` + `OrthancOHIF-1.8.dll` in `b/plugins`.

Start each with `Orthanc-1.13.1.exe <folder>/a/orthanc.json`, and stop it with
`curl -u orthanc:s3cr3tpw -X POST http://127.0.0.1:28042/tools/shutdown`.

Test studies (same UID root, upload with `curl -u orthanc:s3cr3tpw -X POST --data-binary @file http://127.0.0.1:2804x/instances`):

```sh
R=1.2.826.0.1.3680043.9.7777
python make_dicom.py data/s1  $R.101 PA-001 "ALPHA^ONLY"   "CT ONLY ON A" 12 squares   # -> A
python make_dicom.py data/s2  $R.102 PB-002 "BRAVO^ONLY"   "CT ONLY ON B" 12 rings     # -> B
python make_dicom.py data/s3  $R.103 PC-003 "CHARLIE^BOTH" "CT ON BOTH"   12 rings     # -> B
python make_dicom.py data/s3a $R.103 PC-003 "CHARLIE^BOTH" "CT ON BOTH"   12 squares   # -> A (same UIDs, other pixels)
```

Router and portal (a debug build is fine):

```sh
python uid_router.py 28050 A=http://127.0.0.1:28042 B=http://127.0.0.1:28043
ORTHANC_URL=http://127.0.0.1:28050 ORTHANC_USERNAME=orthanc ORTHANC_PASSWORD=s3cr3tpw \
  DATABASE_PATH=<folder>/portal.db BIND_ADDR=127.0.0.1:28090 REMOTE_BIND_ADDR=127.0.0.1:28091 \
  ADMIN_PASSWORD=SuperSecret123 target/debug/orthanc-portal
```

Browser (PowerShell; Edge's launcher exits at once, the headless browser keeps running):

```powershell
Start-Process "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" `
  -ArgumentList "--headless=new","--user-data-dir=<folder>\edge-profile","--no-first-run","--remote-debugging-port=29222","about:blank"
```

Then `node ohif_check.js http://127.0.0.1:28090 <portal_session cookie> <output folder>`.

## Results (2026-11-27)

**DICOMweb `AllowedHosts`** (plugin mainline, after 1.24):

* Without `AllowedHosts`, any request with a forwarded host name other than `localhost` / `127.0.0.1`
  gets HTTP 500 "no 'Host' defined and no 'AllowedHosts' defined although there are forwarded HTTP
  headers". The portal always forwards the address people use, so OHIF and Weasis would show no images.
  Release 1.24 does not do this.
* With the fix, *Test connection* reports it with what to change, and the viewer relay answers 502 with
  the same advice instead of a bare 500. After `"AllowedHosts": ["*"]` in the `DicomWeb` section both
  pass, and the viewer gets its data through a host name.

**Choosing the server from the study UID** (`uid_router.py` in front of A and B, real OHIF in Edge):

* The same study on two servers has **the same Orthanc ID** on both (`e77eac4d-...` for study 103):
  Orthanc IDs alone cannot say which server is meant.
* Study 101 (only on A) and study 102 (only on B) open in OHIF with images drawn; all 16 DICOMweb
  requests of each succeed, each answered by the server that holds the study.
* Study 103 (on both, different pixels): pinned to B, OHIF shows B's rings and every request goes to
  B; pinned to A, it shows A's checkerboard and every request goes to A.
* The links Orthanc writes into its answers (`https://<portal>/dicom-web/studies/<uid>/...`) contain
  the study UID, so they are routed the same way.
* Weasis-style requests (QIDO studies / series / instances, a WADO-RS instance, a WADO-URI picture)
  with a real Weasis token all reach the right server. (Weasis itself was not installed.)
* **One kind of request names no study UID:** OHIF's list of the patient's other studies,
  `GET /dicom-web/studies?00100020=<PatientID>&...`. Routed to the first server only, it showed an
  empty list for the study on B. With several servers the portal must ask every server and merge the
  answers (QIDO answers are JSON arrays). Guests never get this request through (the guest rules
  require the study UID).
* Seen in passing: the browser fetches `/ohif/manifest.json` without the session cookie, so it gets
  401 (a harmless console message).

## Phase 2: the portal itself connected to both servers (2026-11-27)

No router this time: the portal's default server is A (`ORTHANC_URL=http://127.0.0.1:28042`) and B is added
as `archive` in the settings (`"orthanc_servers": [{ "key": "archive", "url": "http://127.0.0.1:28043", ... }]`).
An account `onlyb` (read, visualize) may use the archive only (`"servers": ["archive"]`).
`ohif_servers.js` opens real OHIF for each case: `node ohif_servers.js http://127.0.0.1:28090 <admin cookie> <onlyb cookie> <output folder>`.

* **Search** (admin): 101 on main, 102 on the archive (`archive:...` ID), 103 **one row** naming both
  (`servers: ["main", "archive"]`, its ID the main copy's). `onlyb` finds 102 and 103, both `archive:`
  IDs, and nothing of main. `/api/stats`: 2 + 2.
* **OHIF**, all with images drawn and every DICOMweb request answered 200:
  * admin, 101 (only on A) and 102 (only on B, no `server=`: found by its UID);
  * admin, 103 opened from the archive (`server=archive`): B's rings; from main: A's checkerboard;
  * `onlyb`, 103: B's rings, also when the address asks for `server=main` (ignored: not one of its servers);
  * `onlyb`, 101 (only on A): OHIF says the study is not available. Only B was asked.
* OHIF accepts the extra `server=` parameter in its address.
* OHIF's list of the patient's other studies (`studies?00100020=...`, no study UID) answered 200: the
  portal asks every server of the account and merges the answers.
