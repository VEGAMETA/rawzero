//! Night mode: align and merge a burst of RAW frames (HDR+ style), streaming.
//!
//! S. W. Hasinoff et al., "Burst photography for high dynamic range and low-light imaging on
//! mobile cameras", SIGGRAPH Asia 2016; analysis and reference implementation in A. Monod,
//! J. Delon, T. Veit, "An Analysis and Implementation of the HDR+ Burst Denoising Method",
//! IPOL 2021.
//!
//! - Alignment: Gaussian-ish pyramid of the half-resolution luminance (2x2 Bayer cell mean),
//!   16x16 tiles, coarse-to-fine search (L2 at coarse levels, L1 at the finest). Offsets are
//!   whole half-resolution pixels = even RAW pixels, so the Bayer phase is preserved.
//! - Merge: per 8x8 tile (stride 4, Kaiser-windowed overlap) of each CFA plane, in the
//!   orthonormal Walsh-Hadamard basis (the paper uses the DFT; any orthonormal basis keeps the
//!   per-coefficient noise variance): T = (1/N) sum_z [T_z + A_z (T_0 - T_z)],
//!   A_z = |D|^2 / (|D|^2 + c sigma^2), D = T_0 - T_z. Where an alternate frame disagrees with
//!   the reference beyond noise (motion, misalignment) it is replaced by the reference: no
//!   ghosts. The sum is additive over z, so frames are merged as they arrive and only the
//!   reference plus accumulators are kept in memory.

use crate::denoise::{estimate_noise, NoiseModel};
use crate::image::{Mosaic, RawImage};
use crate::levels::apply_levels;
use crate::meta::{FrameMeta, CfaPattern};
use crate::Error;

/// Robustness constant c of the HDR+ merge (higher = more denoising, less motion rejection).
pub const MERGE_C: f32 = 8.0;
const ALIGN_TILE: usize = 16;
const MERGE_TILE: usize = 8;
const MERGE_STEP: usize = 4;

/// Half-resolution plane with dimensions.
#[derive(Clone)]
struct Plane {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

impl Plane {
    #[inline]
    fn at(&self, x: isize, y: isize) -> f32 {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        self.v[y * self.w + x]
    }
    fn downsample(&self, f: usize) -> Plane {
        let (w, h) = ((self.w / f).max(1), (self.h / f).max(1));
        let mut v = vec![0.0; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut s = 0.0;
                for dy in 0..f {
                    for dx in 0..f {
                        s += self.at((x * f + dx) as isize, (y * f + dy) as isize);
                    }
                }
                v[y * w + x] = s / (f * f) as f32;
            }
        }
        Plane { w, h, v }
    }
}

/// The 4 CFA planes (cell positions 0..3, row-major) at half resolution.
fn planes(m: &Mosaic) -> [Plane; 4] {
    let (w, h) = (m.width / 2, m.height / 2);
    std::array::from_fn(|p| {
        let (dx, dy) = (p & 1, p >> 1);
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                v.push(m.data[(2 * y + dy) * m.width + 2 * x + dx]);
            }
        }
        Plane { w, h, v }
    })
}

fn luma(p: &[Plane; 4]) -> Plane {
    let v = (0..p[0].v.len()).map(|i| 0.25 * (p[0].v[i] + p[1].v[i] + p[2].v[i] + p[3].v[i])).collect();
    Plane { w: p[0].w, h: p[0].h, v }
}

/// Pyramid levels (finest first) and their downsampling factors relative to the previous.
const PYR: [usize; 3] = [2, 4, 4];

fn pyramid(y: &Plane) -> Vec<Plane> {
    let mut out = vec![y.clone()];
    for f in PYR {
        let next = out.last().unwrap().downsample(f);
        out.push(next);
    }
    out
}

/// Per-tile offsets (dx, dy) of the alternate frame in finest-level pixels.
struct Flow {
    tx: usize,
    ty: usize,
    off: Vec<(i32, i32)>,
}

impl Flow {
    fn at_pixel(&self, x: usize, y: usize) -> (i32, i32) {
        let i = (x / ALIGN_TILE).min(self.tx - 1);
        let j = (y / ALIGN_TILE).min(self.ty - 1);
        self.off[j * self.tx + i]
    }
}

/// Coarse-to-fine tile alignment (HDR+ section 4: L2 search at coarse levels, L1 at the
/// finest; radius 4 coarse, 2 at the finest level).
fn align(ref_pyr: &[Plane], alt_pyr: &[Plane]) -> Flow {
    use rayon::prelude::*;
    let levels = ref_pyr.len();
    let mut prev: Option<(Flow, usize)> = None;
    for l in (0..levels).rev() {
        let (r, a) = (&ref_pyr[l], &alt_pyr[l]);
        let t = ALIGN_TILE;
        let (tx, ty) = (r.w.div_ceil(t).max(1), r.h.div_ceil(t).max(1));
        let radius: i32 = if l == 0 { 2 } else { 4 };
        let l1 = l == 0;
        let off: Vec<(i32, i32)> = (0..tx * ty)
            .into_par_iter()
            .map(|k| {
                let (i, j) = (k % tx, k / tx);
                let (x0, y0) = ((i * t) as isize, (j * t) as isize);
                // Initial guess from the coarser level, scaled up.
                let (gx, gy) = match &prev {
                    Some((f, scale)) => {
                        let cx = ((x0 as usize + t / 2) / scale).min(f.tx * t - 1);
                        let cy = ((y0 as usize + t / 2) / scale).min(f.ty * t - 1);
                        let (ox, oy) = f.at_pixel(cx, cy);
                        (ox * *scale as i32, oy * *scale as i32)
                    }
                    None => (0, 0),
                };
                let mut best = (f32::MAX, gx, gy);
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let (ox, oy) = (gx + dx, gy + dy);
                        let mut e = 0.0f32;
                        for yy in 0..t as isize {
                            for xx in 0..t as isize {
                                let d = r.at(x0 + xx, y0 + yy) - a.at(x0 + xx + ox as isize, y0 + yy + oy as isize);
                                e += if l1 { d.abs() } else { d * d };
                            }
                        }
                        if e < best.0 {
                            best = (e, ox, oy);
                        }
                    }
                }
                (best.1, best.2)
            })
            .collect();
        let scale = if l > 0 { PYR[l - 1] } else { 1 };
        prev = Some((Flow { tx, ty, off }, scale));
    }
    prev.unwrap().0
}

/// Streaming HDR+ merge of a RAW burst.
pub struct BurstMerger {
    meta: FrameMeta,
    width: usize,
    height: usize,
    noise: NoiseModel,
    reference: [Plane; 4],
    ref_pyr: Vec<Plane>,
    acc: [Vec<f32>; 4],
    wsum: Vec<f32>,
    frames: usize,
}

impl BurstMerger {
    /// Starts a burst with its reference frame (the first, sharpest-by-convention frame).
    pub fn new(first: &RawImage, meta: &FrameMeta) -> Result<BurstMerger, Error> {
        meta.validate()?;
        if first.width < 64 || first.height < 64 {
            return Err(Error::InvalidInput(format!("burst frame {}x{}", first.width, first.height)));
        }
        let m = apply_levels(first, meta);
        let noise = estimate_noise(&m, meta.cfa).unwrap_or(NoiseModel { s: 1e-3, o: 1e-5 });
        let reference = planes(&m);
        let ref_pyr = pyramid(&luma(&reference));
        let n = reference[0].v.len();
        let mut me = BurstMerger {
            meta: meta.clone(),
            width: first.width,
            height: first.height,
            noise,
            reference,
            ref_pyr,
            acc: std::array::from_fn(|_| vec![0.0; n]),
            wsum: vec![0.0; n],
            frames: 0,
        };
        // The reference contributes T_0 to every tile (its own D is zero).
        let refp = me.reference.clone();
        me.accumulate(&refp, None);
        Ok(me)
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Aligns `frame` to the reference and merges it.
    pub fn add(&mut self, frame: &RawImage) -> Result<(), Error> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(Error::InvalidInput("burst frame size changed".into()));
        }
        let m = apply_levels(frame, &self.meta);
        let alt = planes(&m);
        let flow = align(&self.ref_pyr, &pyramid(&luma(&alt)));
        self.accumulate(&alt, Some(&flow));
        Ok(())
    }

    /// Adds one frame's tile contributions T_z + A_z (T_0 - T_z) (A = 0 for the reference).
    fn accumulate(&mut self, frame: &[Plane; 4], flow: Option<&Flow>) {
        use rayon::prelude::*;
        let (w, h) = (self.reference[0].w, self.reference[0].h);
        let kaiser = crate::denoise::kaiser8(2.0);
        let starts = |len: usize| {
            let mut s: Vec<usize> = (0..=len.saturating_sub(MERGE_TILE)).step_by(MERGE_STEP).collect();
            if s.last() != Some(&(len - MERGE_TILE)) {
                s.push(len - MERGE_TILE);
            }
            s
        };
        let (xs, ys) = (starts(w), starts(h));
        let nm = self.noise;
        let reference = &self.reference;
        for batch in ys.chunks(32) {
            // Per block row: for every tile, the 4 merged plane tiles (8x8 each).
            let rows: Vec<Vec<[[[f32; 8]; 8]; 4]>> = batch
                .par_iter()
                .map(|&y0| {
                    xs.iter()
                        .map(|&x0| {
                            let (ox, oy) = flow.map_or((0, 0), |f| f.at_pixel(x0 + 4, y0 + 4));
                            std::array::from_fn(|p| {
                                let rp = &reference[p];
                                let fp = &frame[p];
                                let mut t0: [[f32; 8]; 8] = std::array::from_fn(|r| std::array::from_fn(|c| rp.v[(y0 + r) * w + x0 + c]));
                                let mut tz: [[f32; 8]; 8] = std::array::from_fn(|r| {
                                    std::array::from_fn(|c| fp.at((x0 + c) as isize + ox as isize, (y0 + r) as isize + oy as isize))
                                });
                                if flow.is_none() {
                                    return tz;
                                }
                                // Tile noise variance from the model at the reference tile mean.
                                let mean = t0.iter().flatten().sum::<f32>() / 64.0;
                                let s2 = nm.var(mean);
                                crate::denoise::wht8x8(&mut t0);
                                crate::denoise::wht8x8(&mut tz);
                                for r in 0..8 {
                                    for c in 0..8 {
                                        let d = t0[r][c] - tz[r][c];
                                        let a = d * d / (d * d + MERGE_C * s2);
                                        tz[r][c] += a * d;
                                    }
                                }
                                crate::denoise::wht8x8(&mut tz);
                                tz
                            })
                        })
                        .collect()
                })
                .collect();
            for (&y0, row) in batch.iter().zip(rows) {
                for (&x0, tiles) in xs.iter().zip(row) {
                    for r in 0..8 {
                        for c in 0..8 {
                            let i = (y0 + r) * w + x0 + c;
                            let k = kaiser[r] * kaiser[c];
                            for p in 0..4 {
                                self.acc[p][i] += k * tiles[p][r][c];
                            }
                            if flow.is_none() {
                                self.wsum[i] += k;
                            }
                        }
                    }
                }
            }
        }
        self.frames += 1;
        let _ = h;
    }

    /// Merged mosaic in normalized units (levels applied), ready for the rest of the pipeline.
    pub fn finish(self) -> (Mosaic, FrameMeta) {
        let (w, h) = (self.reference[0].w, self.reference[0].h);
        let n = self.frames as f32;
        let mut out = Mosaic { width: self.width, height: self.height, data: vec![0.0; self.width * self.height] };
        for p in 0..4 {
            let (dx, dy) = (p & 1, p >> 1);
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    out.data[(2 * y + dy) * self.width + 2 * x + dx] = self.acc[p][i] / (n * self.wsum[i]);
                }
            }
        }
        // Odd trailing row/column (if any) keep the reference frame's values.
        let _ = CfaPattern::Rggb;
        (out, self.meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::meta_rggb;

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

    /// Scene with texture, shifted by (sx, sy) RAW pixels (even), Poisson-Gaussian noise.
    fn frame(rng: &mut Rng, w: usize, h: usize, sx: isize, sy: isize, meta: &FrameMeta) -> (RawImage, Vec<f32>) {
        let scene = |x: isize, y: isize| 0.15 + 0.1 * ((x as f32 * 0.05).sin() * (y as f32 * 0.07).cos()) + if (x / 40 + y / 40) % 2 == 0 { 0.1 } else { 0.0 };
        let wl = meta.white_level;
        let mut clean = Vec::with_capacity(w * h);
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as isize, (i / w) as isize);
                let v = scene(x + sx, y + sy);
                clean.push(v);
                let noisy = v + (2e-3 * v + 1e-5).sqrt() * rng.n();
                (noisy * wl).clamp(0.0, wl) as u16
            })
            .collect();
        (RawImage { width: w, height: h, data }, clean)
    }

    #[test]
    fn merge_reduces_noise_and_aligns_shifts() {
        let mut meta = meta_rggb();
        meta.white_level = 16383.0;
        let (w, h) = (320, 256);
        let mut rng = Rng(42);
        let (r0, clean) = frame(&mut rng, w, h, 0, 0, &meta);
        let mut merger = BurstMerger::new(&r0, &meta).unwrap();
        for k in 1..8 {
            // Hand shake: a few pixels, even shifts.
            let (f, _) = frame(&mut rng, w, h, 2 * (k % 3) as isize, -2 * (k % 2) as isize, &meta);
            merger.add(&f).unwrap();
        }
        assert_eq!(merger.frames(), 8);
        let (merged, _) = merger.finish();
        let single = apply_levels(&r0, &meta);
        let rmse = |m: &Mosaic| {
            let mut s = 0.0f64;
            let mut n = 0;
            for y in 16..h - 16 {
                for x in 16..w - 16 {
                    let d = (m.data[y * w + x] - clean[y * w + x]) as f64;
                    s += d * d;
                    n += 1;
                }
            }
            (s / n as f64).sqrt()
        };
        let (e1, e8) = (rmse(&single), rmse(&merged));
        assert!(e8 < 0.55 * e1, "rmse single {e1} merged {e8}");
    }

    #[test]
    fn rejects_moving_object() {
        let mut meta = meta_rggb();
        meta.white_level = 16383.0;
        let (w, h) = (256, 192);
        let mut rng = Rng(7);
        let (r0, _) = frame(&mut rng, w, h, 0, 0, &meta);
        let mut merger = BurstMerger::new(&r0, &meta).unwrap();
        for _ in 1..6 {
            let (mut f, _) = frame(&mut rng, w, h, 0, 0, &meta);
            // A bright square appears only in the alternates: must not ghost into the result.
            for y in 80..112 {
                for x in 80..112 {
                    f.data[y * w + x] = 15000;
                }
            }
            merger.add(&f).unwrap();
        }
        let (merged, _) = merger.finish();
        let mean = (80..112).flat_map(|y| (80..112).map(move |x| (x, y))).map(|(x, y)| merged.data[y * w + x]).sum::<f32>() / 1024.0;
        assert!(mean < 0.35, "ghost: mean {mean}");
    }
}
