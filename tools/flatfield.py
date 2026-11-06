"""Flat-field check of lens shading: shoot a uniformly lit white wall, then compare
corner vs center level per CFA channel, raw and after the DNG GainMap (OpcodeList2).

  raw corner/center ~0.3..0.6 and ~1 after the map -> the map is needed and correct.
  raw ~1 and >1 after the map                       -> the HAL already corrected shading,
                                                       applying the map again over-brightens.
Color shading: G/R and G/B corner/center after the map should be ~1.

Usage: uv run --no-project --with rawpy --with numpy tools/flatfield.py wall1.dng ...
"""
import struct
import sys
from pathlib import Path

import numpy as np
import rawpy

GRID = (12, 16)  # rows, cols of the measurement grid


def gain_maps(path):
    """GainMap opcodes from OpcodeList2 in any IFD: list of (top, left, rows, cols, gains)."""
    d = Path(path).read_bytes()
    e = "<" if d[:2] == b"II" else ">"
    u16 = lambda o: struct.unpack(e + "H", d[o:o + 2])[0]
    u32 = lambda o: struct.unpack(e + "I", d[o:o + 4])[0]
    queue, seen, maps = [u32(4)], set(), []
    while queue:
        off = queue.pop()
        if off == 0 or off in seen:
            continue
        seen.add(off)
        n = u16(off)
        for i in range(n):
            p = off + 2 + 12 * i
            tag, typ, cnt = u16(p), u16(p + 2), u32(p + 4)
            if tag == 330:
                queue += [u32(u32(p + 8) + 4 * k) if cnt > 1 else u32(p + 8) for k in range(cnt)]
            if tag == 51009:
                b = d[u32(p + 8):u32(p + 8) + cnt]
                o = 4
                for _ in range(struct.unpack(">I", b[:4])[0]):
                    oid, _, _, ln = struct.unpack(">IIII", b[o:o + 16])
                    if oid == 9:
                        q = b[o + 16:o + 16 + ln]
                        top, left = struct.unpack(">II", q[:8])
                        nv, nh = struct.unpack(">II", q[32:40])
                        mp = struct.unpack(">I", q[72:76])[0]
                        g = np.frombuffer(q[76:76 + 4 * nv * nh * mp], dtype=">f4").reshape(nv, nh)
                        maps.append((top % 2, left % 2, g.astype(np.float64)))
                    o += 16 + ln
        queue.append(u32(off + 2 + 12 * n))
    return maps


def upsample(g, shape):
    """Bilinear, grid corners on image corners (Camera2 / DngCreator convention)."""
    nv, nh = g.shape
    y = np.linspace(0, nv - 1, shape[0])
    x = np.linspace(0, nh - 1, shape[1])
    y0 = np.minimum(y.astype(int), max(nv - 2, 0)); fy = y - y0
    x0 = np.minimum(x.astype(int), max(nh - 2, 0)); fx = x - x0
    y1, x1 = np.minimum(y0 + 1, nv - 1), np.minimum(x0 + 1, nh - 1)
    a = g[y0][:, x0] * (1 - fx) + g[y0][:, x1] * fx
    b = g[y1][:, x0] * (1 - fx) + g[y1][:, x1] * fx
    return a * (1 - fy[:, None]) + b * fy[:, None]


def grid_mean(p):
    h, w = p.shape[0] // GRID[0] * GRID[0], p.shape[1] // GRID[1] * GRID[1]
    return p[:h, :w].reshape(GRID[0], h // GRID[0], GRID[1], w // GRID[1]).mean(axis=(1, 3))


def corner_center(m):
    c = m[GRID[0] // 2 - 1:GRID[0] // 2 + 1, GRID[1] // 2 - 1:GRID[1] // 2 + 1].mean()
    k = np.array([m[0, 0], m[0, -1], m[-1, 0], m[-1, -1]]).mean()
    return k / c


def analyze(path):
    with rawpy.imread(str(path)) as r:
        raw = r.raw_image_visible.astype(np.float64)
        pat, desc = r.raw_pattern, r.color_desc.decode()
        black = np.array(r.black_level_per_channel, float)
        white = float(r.white_level)
    maps = {(t, l): g for t, l, g in gain_maps(path)}
    out = {}
    for y in range(2):
        for x in range(2):
            ch = desc[pat[y, x]] + ("2" if desc[pat[y, x]] == "G" and y == 1 else "")
            plane = (raw[y::2, x::2] - black[pat[y, x]]) / (white - black[pat[y, x]])
            raw_m = grid_mean(plane)
            g = maps.get((y, x))
            cor_m = grid_mean(plane * upsample(g, plane.shape)) if g is not None else None
            out[ch] = (raw_m, cor_m, None if g is None else g.max())
    return out


def main(paths):
    print(f"{'file':28} {'ch':3} {'center':>7} {'raw c/c':>8} {'map c/c':>8} {'map max':>8}")
    for p in paths:
        res = analyze(p)
        for ch, (raw_m, cor_m, gmax) in res.items():
            c = raw_m[GRID[0] // 2 - 1:GRID[0] // 2 + 1, GRID[1] // 2 - 1:GRID[1] // 2 + 1].mean()
            cc = "-" if cor_m is None else f"{corner_center(cor_m):.3f}"
            gm = "-" if gmax is None else f"{gmax:.2f}"
            print(f"{Path(p).parent.name + '/' + Path(p).name:28} {ch:3} {c:7.4f} {corner_center(raw_m):8.3f} {cc:>8} {gm:>8}")
        if all(v[1] is not None for v in res.values()):
            g = (res["G"][1] + res["G2"][1]) / 2
            print(f"{'':28} color after map: (R/G) c/c {corner_center(res['R'][1] / g):.3f}, (B/G) c/c {corner_center(res['B'][1] / g):.3f}")


if __name__ == "__main__":
    main(sys.argv[1:])
