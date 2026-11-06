"""Summarize `adb shell dumpsys media.camera`: every HAL camera device, public or not.

Usage: adb shell dumpsys media.camera > dump.txt
       uv run --no-project tools/camera_dump.py dump.txt
Prints one row per HAL device: facing, focal length, sensor size and pixel array,
RAW support, logical multi-camera members, zoom ratio range, and whether cameraserver
exposes it to apps.
"""
import re
import sys

KEYS = {
    "android.lens.facing": "facing",
    "android.lens.info.availableFocalLengths": "focal",
    "android.sensor.info.physicalSize": "phys",
    "android.sensor.info.pixelArraySize": "pixels",
    "android.request.availableCapabilities": "caps",
    "android.logicalMultiCamera.physicalIds": "physical",
    "android.control.zoomRatioRange": "zoom",
    "android.sensor.info.colorFilterArrangement": "cfa",
    "android.sensor.info.whiteLevel": "white",
}


def parse(text):
    devices, cur, key = [], None, None
    for line in text.splitlines():
        m = re.match(r"== Camera HAL device (\S+) \(v[\d.]+\) static information: ==", line)
        if m:
            cur = {"name": m.group(1), "id": m.group(1).rsplit("/", 1)[-1]}
            devices.append(cur)
            key = None
            continue
        if line.startswith("== ") and "static information" not in line:
            cur, key = None, None
            continue
        if cur is None:
            continue
        m = re.match(r"\s+(android\.[\w.]+) \([0-9a-f]+\): \w+\[\d+\]", line)
        if m:
            key = KEYS.get(m.group(1))
            continue
        if key and line.strip().startswith("["):
            cur[key] = cur.get(key, "") + line.strip().strip("[]").strip() + " "
            continue
        key = None
    return devices


def physical_ids(raw):
    # Stored as NUL-separated ASCII bytes: "50 48 0 ..." -> "20", ...
    try:
        b = bytes(int(v) for v in raw.split())
        return [s.decode() for s in b.split(b"\0") if s]
    except ValueError:
        return raw.split()


def main(path):
    text = open(path, encoding="utf-8", errors="replace").read()
    public = set(re.findall(r'Device \d+ maps to "(\w+)"', text))
    hdr = f"{'id':>4} {'pub':>3} {'facing':6} {'f mm':>6} {'sensor mm':>12} {'pixels':>11} {'raw':>3} {'zoom':>11}  physical / caps"
    print(hdr)
    for d in sorted(parse(text), key=lambda d: int(d["id"]) if d["id"].isdigit() else 999):
        caps = d.get("caps", "").split()
        phys = physical_ids(d["physical"]) if "physical" in d else []
        sensor = "x".join(f"{float(v):.2f}" for v in d.get("phys", "").split()[:2])
        print(f"{d['id']:>4} {'yes' if d['id'] in public else '-':>3} {d.get('facing', '?').strip():6} "
              f"{d.get('focal', '?').strip()[:6]:>6} {sensor:>12} {'x'.join(d.get('pixels', '?').split()):>11} "
              f"{'RAW' if 'RAW' in caps else '-':>3} {'..'.join(v[:4] for v in d.get('zoom', '').split()):>11}  "
              f"{','.join(phys) if phys else ''} {' '.join(c for c in caps if c in ('LOGICAL_MULTI_CAMERA', 'SYSTEM_CAMERA', 'ULTRA_HIGH_RESOLUTION_SENSOR', 'REMOSAIC_REPROCESSING'))}")


if __name__ == "__main__":
    main(sys.argv[1])
