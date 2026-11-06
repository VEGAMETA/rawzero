"""Is this RAW really one untouched frame? Noise statistics straight from the CFA data.

For each DNG, on the green photosites of even rows (one CFA plane, stride 2):
  * variance vs signal: in 32x32 plane tiles, noise variance from horizontal differences
    var((p[x] - p[x+1]) / sqrt 2); the 10% flattest tiles per signal bin reject texture.
    A single frame follows the Poisson-Gaussian model var = a * S + b; at equal ISO
    single frames of one sensor have equal `a`. Merging N frames divides `a` by ~N,
    denoising lowers it too.
  * shading: `a` fitted separately on the central and the corner tiles. If the HAL already
    multiplied the data by a lens shading gain g, both signal and noise were scaled, so
    var = g * a * S: `a_corner / a_center` approximates the pre-applied corner gain.
    An untouched RAW gives a ratio near 1 (corners are just darker, not noisier per unit).
  * whiteness: r_k = var(p[x] - p[x+k]) / var(p[x] - p[x+1]) on the same flat tiles.
    White (untouched) noise gives r_k = 1 for all k; low-pass filtering (denoise, merge
    with alignment interpolation) correlates neighbors and gives r_k > 1.

Usage: uv run --with rawpy --with numpy tools/raw_noise.py a.dng b.dng ...
Writes a markdown report to tools/reports/raw_noise.md.
"""
import sys
from pathlib import Path

import numpy as np
import rawpy

TILE = 32
BINS = np.geomspace(0.001, 0.6, 16)
MIN_TILES = 8


def green_plane(path):
    with rawpy.imread(str(path)) as r:
        raw = r.raw_image_visible.astype(np.float64)
        pat = r.raw_pattern  # color index per 2x2 cell: 0 R, 1 G, 2 B, 3 G2
        colors = r.color_desc.decode()
        black = np.array(r.black_level_per_channel, dtype=np.float64)
        white = float(r.white_level)
    # First green on an even row.
    gx = next(x for x in range(2) if colors[pat[0, x]] == "G")
    plane = raw[0::2, gx::2]
    bl = black[pat[0, gx]]
    return (plane - bl) / (white - bl), white - bl


def tiles(p):
    h, w = (p.shape[0] // TILE) * TILE, (p.shape[1] // TILE) * TILE
    t = p[:h, :w].reshape(h // TILE, TILE, w // TILE, TILE).swapaxes(1, 2)
    ny, nx = h // TILE, w // TILE
    yy, xx = np.meshgrid((np.arange(ny) + 0.5) / ny - 0.5, (np.arange(nx) + 0.5) / nx - 0.5, indexing="ij")
    radius = np.hypot(yy * 2, xx * 2).ravel() / np.sqrt(2)  # 0 center, 1 corner
    return t.reshape(-1, TILE, TILE), radius


def fit(mean, var1, ok):
    """Flattest 10% of tiles per signal bin -> weighted LSQ var = a*S + b."""
    rows, flat = [], np.zeros(len(mean), bool)
    for lo, hi in zip(BINS[:-1], BINS[1:]):
        idx = np.where(ok & (mean >= lo) & (mean < hi))[0]
        if len(idx) < MIN_TILES:
            continue
        keep = idx[np.argsort(var1[idx])[: max(4, len(idx) // 10)]]
        flat[keep] = True
        rows.append((mean[keep].mean(), var1[keep].mean()))
    rows = np.array(rows).reshape(-1, 2)
    if len(rows) < 2:
        return float("nan"), float("nan"), rows, flat
    A = np.stack([rows[:, 0], np.ones(len(rows))], 1) / rows[:, 1:2]
    a, b = np.linalg.lstsq(A, np.ones(len(rows)), rcond=None)[0]
    return a, b, rows, flat


def analyze(path):
    p, rng = green_plane(path)
    t, radius = tiles(p)
    mean = t.mean(axis=(1, 2))
    d = {k: (t[:, :, k:] - t[:, :, :-k]) for k in (1, 2, 4, 8)}
    var1 = d[1].var(axis=(1, 2)) / 2
    ok = t.max(axis=(1, 2)) < 0.95
    a, b, rows, flat = fit(mean, var1, ok)
    a_c = fit(mean, var1, ok & (radius < 0.35))[0]
    a_e = fit(mean, var1, ok & (radius > 0.75))[0]
    v1 = d[1][flat].var() if flat.any() else float("nan")
    ratios = {k: (d[k][flat].var() / v1 if flat.any() else float("nan")) for k in (2, 4, 8)}
    return dict(path=Path(path).parent.name + "/" + Path(path).name, levels=rng, a=a, b=b,
                corner=a_e / a_c, ratios=ratios, rows=rows)


def main(paths):
    out_name = "raw_noise.md"
    if paths and paths[0].startswith("--out="):
        out_name, paths = paths[0][6:], paths[1:]
    res = [analyze(p) for p in paths]
    out = ["# RAW noise check", "", "Green plane, flat tiles. `a`: shot-noise slope of var = a*S + b",
           "(normalized units, 1 = white). `corner`: a_corner / a_center, ~1 = shading not pre-applied.",
           "`r_k`: var(d_k)/var(d_1), 1.0 = white noise.", "",
           "| file | levels | a | b | corner | r_2 | r_4 | r_8 |", "|---|---|---|---|---|---|---|---|"]
    for r in res:
        q = r["ratios"]
        out.append(f"| {r['path']} | {r['levels']:.0f} | {r['a']:.3e} | {r['b']:.2e} | "
                   f"{r['corner']:.2f} | {q[2]:.3f} | {q[4]:.3f} | {q[8]:.3f} |")
    out += ["", "Per-bin (signal, variance):", ""]
    for r in res:
        out.append(f"- {r['path']}: " + ", ".join(f"({s:.4f}, {v:.2e})" for s, v in r["rows"]))
    text = "\n".join(out) + "\n"
    rep = Path(__file__).parent / "reports"
    rep.mkdir(exist_ok=True)
    (rep / out_name).write_text(text, encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main(sys.argv[1:])
