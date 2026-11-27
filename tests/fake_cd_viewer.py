"""Writes a small fake of the Weasis CD viewer package (same layout as the real one: viewer/Weasis.exe,
viewer/app/Weasis.cfg, viewer/LICENSES/...) and prints its SHA-256. The test portals download it from
the fake Orthanc instead of GitHub (CD_VIEWER_URL / CD_VIEWER_SHA256, see lib.sh).

    python fake_cd_viewer.py OUT.zip
"""
import hashlib
import sys
import zipfile

CFG = (
    "[Application]\r\n"
    "app.classpath=$APPDIR\\weasis-launcher.jar\r\n"
    "app.mainclass=org.weasis.launcher.AppLauncher\r\n"
    "\r\n"
    "[JavaOptions]\r\n"
    "java-options=-Djpackage.app-version=4.8.1\r\n"
    "java-options=-splash:$APPDIR\\resources\\images\\about-round.png\r\n"
    "java-options=-Dgosh.port=17179\r\n"
)
FILES = {
    "viewer/Weasis.exe": b"MZ fake Weasis launcher",
    "viewer/app/Weasis.cfg": CFG.encode(),
    "viewer/app/conf/base.json": b'{"weasisPreferences": []}',
    "viewer/runtime/legal/java.base/LICENSE": b"GPLv2 with the Classpath Exception (fake)",
    "viewer/LICENSES/LICENSE": b"EPL-2.0 OR Apache-2.0 (fake)",
    "viewer/LICENSES/NOTICE.txt": b"Weasis 4.8.1 (fake package for the tests)",
    "DICOMDIR": b"must never replace the CD's own index",
}

out = sys.argv[1]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for name, data in FILES.items():
        info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))  # same bytes every time
        info.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(info, data)
print(hashlib.sha256(open(out, "rb").read()).hexdigest())
