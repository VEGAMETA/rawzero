//! Classic step: raw-domain noise reduction (before shading, WB and demosaic).
//!
//! Why raw: after demosaic and color conversion noise becomes spatially and chromatically
//! correlated and is much harder to separate (Plötz & Roth, "Benchmarking Denoising
//! Algorithms with Real Photographs", CVPR 2017). Design follows the local, block-matching
//! free family (GALOSH, 2026: local Walsh-Hadamard shrinkage for luminance, luminance-guided
//! regression for chroma):
//!
//! 1. Each 2x2 Bayer cell -> Y (mean), Cr = R - Gm, Cb = B - Gm, Gd = G1 - G2 (Gm = mean of
//!    the greens). Invertible; the four planes have nearly independent noise.
//! 2. Noise model var(x) = S*x + O (Poisson-Gaussian, Foi et al. 2008) is fitted blindly from
//!    Gd: two greens of one cell see the same light, so G1 - G2 is almost pure noise with
//!    var = 2(S*Gm + O). No dependence on HAL metadata (stock Xiaomi DNGs carry fake profiles).
//! 3. Y: 8x8 orthonormal Walsh-Hadamard blocks, stride 4, signal-dependent sigma per block;
//!    pass 1 hard threshold, pass 2 empirical Wiener against the pass-1 pilot (the two-step
//!    structure of BM3D without block matching); a fraction of the original grain is kept.
//! 4. Cr, Cb: guided filter (He, Sun, Tang, TPAMI 2013) with the denoised Y as guide: strong
//!    smoothing of chroma blotches, chroma edges kept where luminance has edges.
//! 5. Gd: local Wiener shrinkage.
//! Deterministic, local, no learned components.

use crate::image::Mosaic;
use crate::meta::{CfaPattern, B, G, R};

/// Fraction of the luminance noise left in (grain instead of "watercolor"). Owner's look.
pub const LUMA_GRAIN_KEEP: f32 = 0.25;
/// Hard threshold in units of the local sigma (BM3D uses 2.7).
const HARD_LAMBDA: f32 = 2.7;
/// Guided filter radius on the half-resolution planes (16 px at full resolution).
const CHROMA_RADIUS: usize = 8;

/// var(x) = s * x + o for a single photosite, x normalized (0 = black, 1 = white), before WB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoiseModel {
    pub s: f32,
    pub o: f32,
}

impl NoiseModel {
    #[inline]
    pub fn var(&self, x: f32) -> f32 {
        (self.s * x.max(0.0) + self.o).max(1e-12)
    }
}

/// Half-resolution decorrelated planes of a Bayer mosaic.
struct Planes {
    w: usize,
    h: usize,
    y: Vec<f32>,
    cr: Vec<f32>,
    cb: Vec<f32>,
    gd: Vec<f32>,
}

/// Cell offsets (dx, dy) of R, G1, G2, B in the 2x2 pattern; G1 is the green on row 0.
fn offsets(cfa: CfaPattern) -> [(usize, usize); 4] {
    let lay = cfa.layout();
    let pos = |p: usize| (p & 1, p >> 1);
    let r = (0..4).find(|&p| lay[p] == R).unwrap();
    let b = (0..4).find(|&p| lay[p] == B).unwrap();
    let g1 = (0..2).find(|&p| lay[p] == G).unwrap();
    let g2 = (2..4).find(|&p| lay[p] == G).unwrap();
    [pos(r), pos(g1), pos(g2), pos(b)]
}

fn split(m: &Mosaic, cfa: CfaPattern) -> Planes {
    let (w, h) = (m.width / 2, m.height / 2);
    let o = offsets(cfa);
    let n = w * h;
    let (mut y, mut cr, mut cb, mut gd) = (vec![0.0; n], vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    for cy in 0..h {
        for cx in 0..w {
            let at = |(dx, dy): (usize, usize)| m.data[(2 * cy + dy) * m.width + 2 * cx + dx];
            let (r, g1, g2, b) = (at(o[0]), at(o[1]), at(o[2]), at(o[3]));
            let gm = 0.5 * (g1 + g2);
            let i = cy * w + cx;
            y[i] = 0.25 * (r + g1 + g2 + b);
            cr[i] = r - gm;
            cb[i] = b - gm;
            gd[i] = g1 - g2;
        }
    }
    Planes { w, h, y, cr, cb, gd }
}

fn merge(p: &Planes, cfa: CfaPattern, orig: &Mosaic) -> Mosaic {
    let mut out = orig.clone();
    let o = offsets(cfa);
    for cy in 0..p.h {
        for cx in 0..p.w {
            let i = cy * p.w + cx;
            // Inverse of split: Gm = Y - (Cr + Cb) / 4.
            let gm = p.y[i] - 0.25 * (p.cr[i] + p.cb[i]);
            let v = [p.cr[i] + gm, gm + 0.5 * p.gd[i], gm - 0.5 * p.gd[i], p.cb[i] + gm];
            for (k, &(dx, dy)) in o.iter().enumerate() {
                out.data[(2 * cy + dy) * orig.width + 2 * cx + dx] = v[k];
            }
        }
    }
    out
}

/// Box mean with radius r (window 2r+1, clamped at borders), separable running sums.
/// Horizontal pass in parallel over rows; vertical pass as running sums of whole rows
/// (contiguous, vectorizes) instead of strided column walks.
fn box_mean(v: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    use rayon::prelude::*;
    let mut tmp = vec![0.0f32; v.len()];
    tmp.par_chunks_mut(w).zip(v.par_chunks(w)).for_each(|(out, row)| {
        let mut acc = 0.0f64;
        let mut cnt = 0usize;
        for x in 0..=r.min(w - 1) {
            acc += row[x] as f64;
            cnt += 1;
        }
        for x in 0..w {
            out[x] = (acc / cnt as f64) as f32;
            if x + r + 1 < w {
                acc += row[x + r + 1] as f64;
                cnt += 1;
            }
            if x >= r {
                acc -= row[x - r] as f64;
                cnt -= 1;
            }
        }
    });
    let mut out = vec![0.0f32; v.len()];
    let mut acc = vec![0.0f64; w];
    let mut cnt = 0usize;
    let add = |acc: &mut [f64], row: &[f32], sign: f64| acc.iter_mut().zip(row).for_each(|(a, &x)| *a += sign * x as f64);
    for y in 0..=r.min(h - 1) {
        add(&mut acc, &tmp[y * w..(y + 1) * w], 1.0);
        cnt += 1;
    }
    for y in 0..h {
        let inv = 1.0 / cnt as f64;
        out[y * w..(y + 1) * w].iter_mut().zip(&acc).for_each(|(o, &a)| *o = (a * inv) as f32);
        if y + r + 1 < h {
            add(&mut acc, &tmp[(y + r + 1) * w..(y + r + 2) * w], 1.0);
            cnt += 1;
        }
        if y >= r {
            add(&mut acc, &tmp[(y - r) * w..(y - r + 1) * w], -1.0);
            cnt -= 1;
        }
    }
    out
}

/// Blind Poisson-Gaussian fit from the green difference plane: in signal bins of the smoothed
/// green level, var(Gd) = 2 (S * Gm + O); the median of Gd^2 is robust to the little detail Gd
/// carries (median of a squared normal = 0.4549 sigma^2). Weighted least squares on the bins.
pub fn estimate_noise(m: &Mosaic, cfa: CfaPattern) -> Option<NoiseModel> {
    let p = split(m, cfa);
    let gm: Vec<f32> = p.y.iter().zip(p.cr.iter().zip(&p.cb)).map(|(&y, (&a, &b))| y - 0.25 * (a + b)).collect();
    let level = box_mean(&gm, p.w, p.h, 2);
    const BINS: usize = 24;
    let edges: Vec<f32> = (0..=BINS).map(|i| 0.002 * (300.0f32).powf(i as f32 / BINS as f32)).collect();
    let mut buckets: Vec<Vec<f32>> = vec![Vec::new(); BINS];
    let step = (p.gd.len() / 600_000).max(1);
    for i in (0..p.gd.len()).step_by(step) {
        let l = level[i];
        if !(l > edges[0] && l < 0.9) {
            continue;
        }
        let b = edges.partition_point(|&e| e <= l).saturating_sub(1).min(BINS - 1);
        buckets[b].push(p.gd[i] * p.gd[i]);
    }
    let mut pts = Vec::new();
    for (b, mut v) in buckets.into_iter().enumerate() {
        if v.len() < 200 {
            continue;
        }
        let mid = v.len() / 2;
        let med = *v.select_nth_unstable_by(mid, f32::total_cmp).1;
        let x = (edges[b] * edges[b + 1]).sqrt();
        pts.push((x as f64, (med / 0.4549 / 2.0) as f64));
    }
    if pts.len() < 2 {
        return None;
    }
    // WLS for var = S x + O with weights 1/var^2 (relative error).
    let (mut sw, mut swx, mut swy, mut swxx, mut swxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(x, v) in &pts {
        let wt = 1.0 / (v * v);
        sw += wt;
        swx += wt * x;
        swy += wt * v;
        swxx += wt * x * x;
        swxy += wt * x * v;
    }
    let det = sw * swxx - swx * swx;
    if det.abs() < 1e-30 {
        return None;
    }
    let s = (sw * swxy - swx * swy) / det;
    let o = (swy - s * swx) / sw;
    let s = s.max(0.0) as f32;
    let o = (o.max(0.0) as f32).max(1e-10);
    (s > 0.0 || o > 1e-10).then_some(NoiseModel { s, o })
}

/// Orthonormal 8-point Walsh-Hadamard transform in place (self-inverse).
#[inline]
fn wht8(v: &mut [f32; 8]) {
    let mut h = 1;
    while h < 8 {
        let mut i = 0;
        while i < 8 {
            for j in i..i + h {
                let (a, b) = (v[j], v[j + h]);
                v[j] = a + b;
                v[j + h] = a - b;
            }
            i += 2 * h;
        }
        h *= 2;
    }
    let k = 1.0 / 8f32.sqrt();
    for x in v.iter_mut() {
        *x *= k;
    }
}

fn wht8x8(b: &mut [[f32; 8]; 8]) {
    for row in b.iter_mut() {
        wht8(row);
    }
    for c in 0..8 {
        let mut col = [0.0f32; 8];
        for r in 0..8 {
            col[r] = b[r][c];
        }
        wht8(&mut col);
        for r in 0..8 {
            b[r][c] = col[r];
        }
    }
}

/// 8-point Kaiser window, w(n) = I0(beta sqrt(1 - (2n/(N-1) - 1)^2)) / I0(beta).
fn kaiser8(beta: f32) -> [f32; 8] {
    let i0 = |x: f32| {
        // Series for the modified Bessel function of order 0.
        let (mut sum, mut term) = (1.0f32, 1.0f32);
        for k in 1..20 {
            term *= (x / 2.0) * (x / 2.0) / (k * k) as f32;
            sum += term;
        }
        sum
    };
    std::array::from_fn(|n| {
        let t = 2.0 * n as f32 / 7.0 - 1.0;
        i0(beta * (1.0 - t * t).max(0.0).sqrt()) / i0(beta)
    })
}

/// Two-pass block shrinkage of the luminance plane. `var` is the per-pixel noise variance.
fn denoise_luma(y: &[f32], var: &[f32], w: usize, h: usize) -> Vec<f32> {
    const N: usize = 8;
    // Dense overlap plus a Kaiser aggregation window (BM3D, beta = 2) hides block edges,
    // which hard thresholding otherwise leaves visible at high noise.
    const STEP: usize = 2;
    let kaiser = kaiser8(2.0);
    if w < N || h < N {
        return y.to_vec();
    }
    let starts = |len: usize| {
        let mut s: Vec<usize> = (0..=len - N).step_by(STEP).collect();
        if *s.last().unwrap() != len - N {
            s.push(len - N);
        }
        s
    };
    let (xs, ys) = (starts(w), starts(h));
    let block = |src: &[f32], x0: usize, y0: usize| -> [[f32; 8]; 8] {
        std::array::from_fn(|r| std::array::from_fn(|c| src[(y0 + r) * w + x0 + c]))
    };
    let block_sigma2 = |x0: usize, y0: usize| -> f32 {
        let mut s = 0.0;
        for r in 0..N {
            for c in 0..N {
                s += var[(y0 + r) * w + x0 + c];
            }
        }
        s / (N * N) as f32
    };

    // Pass 1: hard threshold, weight 1 / (sigma^2 * nonzero coefficients).
    let pass1 = |x0: usize, y0: usize| -> ([[f32; 8]; 8], f32) {
        let s2 = block_sigma2(x0, y0);
        let thr = HARD_LAMBDA * s2.sqrt();
        let mut b = block(y, x0, y0);
        wht8x8(&mut b);
        let mut nz = 0usize;
        for (r, row) in b.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                if (r, c) != (0, 0) && v.abs() < thr {
                    *v = 0.0;
                } else {
                    nz += 1;
                }
            }
        }
        wht8x8(&mut b);
        (b, 1.0 / (s2 * nz as f32))
    };
    let pilot = aggregate(w, h, &xs, &ys, &kaiser, pass1);

    // Pass 2: empirical Wiener with the pilot's spectrum.
    let pass2 = |x0: usize, y0: usize| -> ([[f32; 8]; 8], f32) {
        let s2 = block_sigma2(x0, y0);
        let mut p = block(&pilot, x0, y0);
        let mut b = block(y, x0, y0);
        wht8x8(&mut p);
        wht8x8(&mut b);
        let mut wn = 0.0f32;
        for r in 0..N {
            for c in 0..N {
                let pp = p[r][c] * p[r][c];
                let g = if (r, c) == (0, 0) { 1.0 } else { pp / (pp + s2) };
                b[r][c] *= g;
                wn += g * g;
            }
        }
        wht8x8(&mut b);
        (b, 1.0 / (s2 * wn.max(1.0)))
    };
    aggregate(w, h, &xs, &ys, &kaiser, pass2)
}

/// Filters every block (in parallel, one block row per task) and accumulates the
/// Kaiser-weighted results. Accumulation runs sequentially in a fixed order, so the output
/// is bit-identical regardless of thread count (CLAUDE.md: reproducible hashes).
fn aggregate<F>(w: usize, h: usize, xs: &[usize], ys: &[usize], kaiser: &[f32; 8], f: F) -> Vec<f32>
where
    F: Fn(usize, usize) -> ([[f32; 8]; 8], f32) + Sync,
{
    use rayon::prelude::*;
    let mut acc = vec![0.0f32; w * h];
    let mut wsum = vec![0.0f32; w * h];
    // Bounded batches keep the per-block results small (32 block rows at a time).
    for batch in ys.chunks(32) {
        let rows: Vec<Vec<([[f32; 8]; 8], f32)>> = batch.par_iter().map(|&y0| xs.iter().map(|&x0| f(x0, y0)).collect()).collect();
        for (&y0, row) in batch.iter().zip(rows) {
            for (&x0, (b, wt)) in xs.iter().zip(row) {
                for r in 0..8 {
                    for c in 0..8 {
                        let i = (y0 + r) * w + x0 + c;
                        let k = wt * kaiser[r] * kaiser[c];
                        acc[i] += k * b[r][c];
                        wsum[i] += k;
                    }
                }
            }
        }
    }
    acc.iter().zip(&wsum).map(|(a, s)| a / s).collect()
}

/// Guided filter of `p` by guide `g` (He et al. 2013), eps per pixel.
fn guided(g: &[f32], p: &[f32], eps: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mi = box_mean(g, w, h, r);
    let mp = box_mean(p, w, h, r);
    let gg: Vec<f32> = g.iter().map(|v| v * v).collect();
    let gp: Vec<f32> = g.iter().zip(p).map(|(a, b)| a * b).collect();
    let mii = box_mean(&gg, w, h, r);
    let mip = box_mean(&gp, w, h, r);
    let mut a = vec![0.0f32; g.len()];
    let mut b = vec![0.0f32; g.len()];
    for i in 0..g.len() {
        let var = (mii[i] - mi[i] * mi[i]).max(0.0);
        let cov = mip[i] - mi[i] * mp[i];
        a[i] = cov / (var + eps[i]);
        b[i] = mp[i] - a[i] * mi[i];
    }
    let ma = box_mean(&a, w, h, r);
    let mb = box_mean(&b, w, h, r);
    (0..g.len()).map(|i| ma[i] * g[i] + mb[i]).collect()
}

/// Denoises a normalized mosaic (after levels, before shading/WB) with the given model.
pub fn denoise_mosaic(m: &Mosaic, cfa: CfaPattern, nm: NoiseModel) -> Mosaic {
    if m.width < 32 || m.height < 32 {
        return m.clone();
    }
    let mut p = split(m, cfa);
    let (w, h) = (p.w, p.h);
    // Smoothed component levels for signal-dependent variances (noise-free enough).
    let ys = box_mean(&p.y, w, h, 2);
    let crs = box_mean(&p.cr, w, h, 2);
    let cbs = box_mean(&p.cb, w, h, 2);
    let gm: Vec<f32> = (0..w * h).map(|i| ys[i] - 0.25 * (crs[i] + cbs[i])).collect();

    // var(Y) = (S*Y + O)/4; var(Cr) = vR + vG/2; var(Cb) = vB + vG/2; var(Gd) = 2 vG.
    let var_y: Vec<f32> = ys.iter().map(|&v| nm.var(v) / 4.0).collect();
    let yd = denoise_luma(&p.y, &var_y, w, h);
    let y_out: Vec<f32> = yd.iter().zip(&p.y).map(|(&d, &o)| d + LUMA_GRAIN_KEEP * (o - d)).collect();

    let vg: Vec<f32> = gm.iter().map(|&g| nm.var(g)).collect();
    // Guide regularization: 4x the guide's noise variance, so residual luma noise in the
    // (already denoised) guide cannot imprint itself on chroma; real luma edges are far above.
    let eps: Vec<f32> = var_y.iter().map(|v| 4.0 * v).collect();
    p.cr = guided(&yd, &p.cr, &eps, w, h, CHROMA_RADIUS);
    p.cb = guided(&yd, &p.cb, &eps, w, h, CHROMA_RADIUS);
    // Gd: local Wiener, signal energy from a 3x3 window.
    let gd2: Vec<f32> = p.gd.iter().map(|v| v * v).collect();
    let e = box_mean(&gd2, w, h, 1);
    for i in 0..w * h {
        let n2 = 2.0 * vg[i];
        p.gd[i] *= (1.0 - n2 / e[i].max(1e-12)).max(0.0);
    }
    p.y = y_out;
    merge(&p, cfa, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift + Box-Muller, no dependency.
    struct Rng(u64);
    impl Rng {
        fn u(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        }
        fn n(&mut self) -> f32 {
            (-2.0 * self.u().ln()).sqrt() * (std::f32::consts::TAU * self.u()).cos()
        }
    }

    fn noisy(w: usize, h: usize, nm: NoiseModel, f: impl Fn(usize, usize) -> f32) -> (Mosaic, Mosaic) {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let clean: Vec<f32> = (0..w * h).map(|i| f(i % w, i / w)).collect();
        let noisy = clean.iter().map(|&v| v + nm.var(v).sqrt() * rng.n()).collect();
        (Mosaic { width: w, height: h, data: clean }, Mosaic { width: w, height: h, data: noisy })
    }

    fn rmse(a: &Mosaic, b: &Mosaic) -> f32 {
        (a.data.iter().zip(&b.data).map(|(x, y)| (x - y) * (x - y)).sum::<f32>() / a.data.len() as f32).sqrt()
    }

    #[test]
    fn split_merge_roundtrip() {
        for cfa in [CfaPattern::Rggb, CfaPattern::Grbg, CfaPattern::Gbrg, CfaPattern::Bggr] {
            let m = Mosaic { width: 6, height: 4, data: (0..24).map(|i| i as f32 * 0.1).collect() };
            let out = merge(&split(&m, cfa), cfa, &m);
            for (a, b) in out.data.iter().zip(&m.data) {
                assert!((a - b).abs() < 1e-5, "{cfa:?}");
            }
        }
    }

    #[test]
    fn estimates_known_noise_model() {
        let truth = NoiseModel { s: 4e-4, o: 2e-5 };
        // Smooth gradient scene covering many signal levels.
        let (_, n) = noisy(1024, 768, truth, |x, y| 0.01 + 0.5 * (x as f32 / 1024.0) * (y as f32 / 768.0 + 0.2));
        let est = estimate_noise(&n, CfaPattern::Rggb).unwrap();
        assert!((est.s / truth.s - 1.0).abs() < 0.15, "{est:?}");
        assert!((est.o / truth.o - 1.0).abs() < 0.5, "{est:?}");
    }

    #[test]
    fn reduces_noise_and_keeps_edges() {
        let nm = NoiseModel { s: 1e-3, o: 5e-5 };
        // Step edge: dark left half, bright right half, colored (R channel brighter).
        let f = |x: usize, y: usize| {
            let base = if x < 128 { 0.05 } else { 0.3 };
            if x % 2 == 0 && y % 2 == 0 { base * 1.5 } else { base }
        };
        let (clean, n) = noisy(256, 128, nm, f);
        let d = denoise_mosaic(&n, CfaPattern::Rggb, nm);
        let (before, after) = (rmse(&n, &clean), rmse(&d, &clean));
        assert!(after < 0.6 * before, "rmse {before} -> {after}");
        // Edge stays sharp: mean of the columns right next to the edge keeps the step.
        let col = |m: &Mosaic, x: usize| (0..m.height).map(|y| m.data[y * m.width + x]).sum::<f32>() / m.height as f32;
        assert!(col(&d, 131) - col(&d, 124) > 0.2, "edge smeared: {} {}", col(&d, 124), col(&d, 131));
    }

    #[test]
    fn wht_is_orthonormal() {
        let mut b: [[f32; 8]; 8] = std::array::from_fn(|r| std::array::from_fn(|c| (r * 8 + c) as f32));
        let orig = b;
        let e0: f32 = orig.iter().flatten().map(|v| v * v).sum();
        wht8x8(&mut b);
        let e1: f32 = b.iter().flatten().map(|v| v * v).sum();
        assert!((e0 - e1).abs() / e0 < 1e-5);
        wht8x8(&mut b);
        for (a, o) in b.iter().flatten().zip(orig.iter().flatten()) {
            assert!((a - o).abs() < 1e-3);
        }
    }
}
