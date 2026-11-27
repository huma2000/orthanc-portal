"""Writes THIRD-PARTY-LICENSES.txt: the licence of every Rust crate built into the portal.

The portal's program contains these crates (all under permissive licences: MIT, Apache-2.0, BSD,
ISC, Zlib, Unicode...), whose licences ask that their notices go with the program. This script
reads `cargo metadata` (only the crates of the normal build, not those of the tests), takes each
crate's own licence files from the sources cargo downloaded, and writes them into one file.

Run it from the project root before making a release (the release zip includes the file):

    python tools/third_party_licenses.py
"""
import json
import os
import subprocess
import sys

OUT = "THIRD-PARTY-LICENSES.txt"
LICENCE_FILES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")


def main() -> int:
    meta = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"],
                                     capture_output=True, text=True, check=True).stdout)
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    packages = {p["id"]: p for p in meta["packages"]}
    root = meta["resolve"]["root"]
    seen, stack = set(), [root]
    while stack:  # the crates of the normal build (and build scripts), at any depth
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        for dep in nodes[current]["deps"]:
            if any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    crates = sorted((packages[i] for i in seen if i != root), key=lambda p: (p["name"], p["version"]))

    parts = [
        "Third-party software in Orthanc Portal\n"
        "======================================\n\n"
        f"The portal's program includes the following {len(crates)} Rust crates. Each one is listed with its\n"
        "licence (as its authors declare it) and the text of its licence files.\n"
        "Orthanc Portal itself is under the BSD 3-Clause License (see LICENSE).\n"
        "Weasis, put on patient CDs, has its own notice in viewer/LICENSES on each CD.\n"
    ]
    missing = []
    for crate in crates:
        folder = os.path.dirname(crate["manifest_path"])
        files = sorted(f for f in os.listdir(folder)
                       if f.upper().startswith(LICENCE_FILES) and os.path.isfile(os.path.join(folder, f)))
        header = f"\n{'=' * 78}\n{crate['name']} {crate['version']}\nLicence: {crate.get('license') or crate.get('license_file') or 'not declared'}\n"
        if crate.get("repository"):
            header += f"Source: {crate['repository']}\n"
        parts.append(header)
        if not files:
            missing.append(crate["name"])
            parts.append("(No licence file in the published crate: the licence named above applies.)\n")
        for name in files:
            with open(os.path.join(folder, name), encoding="utf-8", errors="replace") as f:
                parts.append(f"\n--- {name}\n\n{f.read().rstrip()}\n")
    with open(OUT, "w", encoding="utf-8", newline="\n") as f:
        f.write("".join(parts))
    print(f"{OUT}: {len(crates)} crates, {len(missing)} without a licence file"
          + (f" ({', '.join(missing)})" if missing else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
