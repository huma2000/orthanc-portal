"""Writes small but complete CT series (explicit VR little endian, 64x64, 16-bit) with no
dependencies, for testing viewers against a real Orthanc.

    python make_dicom.py OUT_DIR STUDY_UID PATIENT_ID PATIENT_NAME DESCRIPTION SLICES [PATTERN]
"""
import math, os, struct, sys


def el(group, elem, vr, value):
    if isinstance(value, str):
        value = value.encode("ascii")
        if len(value) % 2:
            value += b"\0" if vr == "UI" else b" "
    elif len(value) % 2:
        value += b"\0"
    tag = struct.pack("<HH", group, elem)
    if vr in ("OB", "OW", "SQ", "UN", "UT"):
        return tag + vr.encode() + b"\0\0" + struct.pack("<I", len(value)) + value
    return tag + vr.encode() + struct.pack("<H", len(value)) + value


def us(v):
    return struct.pack("<H", v)


def slice_file(study, series, sop, pid, name, desc, n, total, pattern):
    rows = cols = 64
    pixels = bytearray()
    for y in range(rows):
        for x in range(cols):
            if pattern == "rings":
                r = math.hypot(x - 32, y - 32)
                v = int(1000 + 800 * math.sin(r / 3 + n))
            else:  # squares
                v = 1800 if ((x // 8 + y // 8 + n) % 2) else 200
            pixels += struct.pack("<H", max(0, min(4095, v)))
    meta_body = (el(0x0002, 0x0001, "OB", b"\0\1") + el(0x0002, 0x0002, "UI", "1.2.840.10008.5.1.4.1.1.2")
                 + el(0x0002, 0x0003, "UI", sop) + el(0x0002, 0x0010, "UI", "1.2.840.10008.1.2.1")
                 + el(0x0002, 0x0012, "UI", "1.2.826.0.1.3680043.9.7777.1"))
    meta = el(0x0002, 0x0000, "UL", struct.pack("<I", len(meta_body))) + meta_body
    z = n * 2.0
    ds = b"".join([
        el(0x0008, 0x0016, "UI", "1.2.840.10008.5.1.4.1.1.2"),
        el(0x0008, 0x0018, "UI", sop),
        el(0x0008, 0x0020, "DA", "20261120"),
        el(0x0008, 0x0030, "TM", "101500"),
        el(0x0008, 0x0050, "SH", "ACC" + study[-6:].replace(".", "")),
        el(0x0008, 0x0060, "CS", "CT"),
        el(0x0008, 0x0080, "LO", "Test Hospital"),
        el(0x0008, 0x1030, "LO", desc),
        el(0x0008, 0x103E, "LO", desc + " series"),
        el(0x0010, 0x0010, "PN", name),
        el(0x0010, 0x0020, "LO", pid),
        el(0x0010, 0x0030, "DA", "19700101"),
        el(0x0010, 0x0040, "CS", "O"),
        el(0x0018, 0x0050, "DS", "2"),
        el(0x0020, 0x000D, "UI", study),
        el(0x0020, 0x000E, "UI", series),
        el(0x0020, 0x0011, "IS", "1"),
        el(0x0020, 0x0013, "IS", str(n + 1)),
        el(0x0020, 0x0032, "DS", f"-32\\-32\\{z}"),
        el(0x0020, 0x0037, "DS", "1\\0\\0\\0\\1\\0"),
        el(0x0020, 0x0052, "UI", study + ".9"),
        el(0x0028, 0x0002, "US", us(1)),
        el(0x0028, 0x0004, "CS", "MONOCHROME2"),
        el(0x0028, 0x0010, "US", us(rows)),
        el(0x0028, 0x0011, "US", us(cols)),
        el(0x0028, 0x0030, "DS", "1\\1"),
        el(0x0028, 0x0100, "US", us(16)),
        el(0x0028, 0x0101, "US", us(12)),
        el(0x0028, 0x0102, "US", us(11)),
        el(0x0028, 0x0103, "US", us(0)),
        el(0x0028, 0x1050, "DS", "1000"),
        el(0x0028, 0x1051, "DS", "2000"),
        el(0x0028, 0x1052, "DS", "0"),
        el(0x0028, 0x1053, "DS", "1"),
        el(0x7FE0, 0x0010, "OW", bytes(pixels)),
    ])
    return b"\0" * 128 + b"DICM" + meta + ds


def main():
    out, study, pid, name, desc, slices = sys.argv[1:7]
    pattern = sys.argv[7] if len(sys.argv) > 7 else "squares"
    os.makedirs(out, exist_ok=True)
    series = study + ".1"
    for n in range(int(slices)):
        sop = f"{series}.{n + 1}"
        with open(os.path.join(out, f"{n + 1:03}.dcm"), "wb") as f:
            f.write(slice_file(study, series, sop, pid, name, desc, n, int(slices), pattern))


main()
