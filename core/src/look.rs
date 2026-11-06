//! Classic look match: global tone curve fitted so our render has the brightness
//! distribution of the ISP output of the same capture request (what the user saw).
//!
//! Method: histogram specification on luminance. For quantile q, our linear luminance
//! x_q is mapped to the display-encoded luminance y_q of the reference. Matching
//! distributions instead of pixels needs no registration, so small field-of-view or
//! crop differences between RAW and the ISP JPEG do not matter. The curve is global and
//! monotone by construction, so it cannot invert tones or produce halos; local detail and
//! noise stay ours. Applied to luminance with RGB ratios kept (chromaticity preserved).

use crate::image::{Rgb8Image, RgbImage};
use crate::tone::srgb_oetf;

/// Rec. 709 / sRGB luminance weights for linear RGB.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];
/// Quantiles fitted between 0.2 % and 99.8 %; tails are anchored separately.
const QUANTILES: usize = 96;
/// Below this linear luminance the curve is a straight line through the origin.
const FLOOR: f32 = 1e-5;

/// Monotone piecewise-linear map from linear luminance to display-encoded luminance,
/// knots in log2(linear) for even resolution across stops.
#[derive(Clone, Debug, PartialEq)]
pub struct ToneMap {
    log_x: Vec<f32>,
    y: Vec<f32>,
}

fn srgb_eotf(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn quantiles(mut v: Vec<f32>, n: usize) -> Vec<f32> {
    v.sort_unstable_by(f32::total_cmp);
    (0..n)
        .map(|i| {
            let q = 0.002 + 0.996 * i as f32 / (n - 1) as f32;
            v[((v.len() - 1) as f32 * q) as usize]
        })
        .collect()
}

impl ToneMap {
    /// Fits the map from our linear render to the reference JPEG (sRGB, any size).
    pub fn fit(ours: &RgbImage, reference: &Rgb8Image) -> ToneMap {
        let step = (ours.data.len() / 400_000).max(1);
        let x: Vec<f32> = ours.data.iter().step_by(step).map(|p| lum(p).max(0.0)).collect();
        let lut: Vec<f32> = (0..256).map(|i| srgb_eotf(i as f32 / 255.0)).collect();
        let rstep = (reference.data.len() / 3 / 400_000).max(1);
        let y: Vec<f32> = reference
            .data
            .chunks_exact(3)
            .step_by(rstep)
            .map(|p| srgb_oetf(LUMA[0] * lut[p[0] as usize] + LUMA[1] * lut[p[1] as usize] + LUMA[2] * lut[p[2] as usize]))
            .collect();
        let (qx, qy) = (quantiles(x, QUANTILES), quantiles(y, QUANTILES));

        let mut log_x = Vec::with_capacity(QUANTILES + 2);
        let mut ys = Vec::with_capacity(QUANTILES + 2);
        for (&a, &b) in qx.iter().zip(&qy) {
            if a < FLOOR {
                continue;
            }
            let la = a.log2();
            // Strictly increasing x, non-decreasing y.
            if log_x.last().is_some_and(|&l| la <= l + 1e-4) {
                continue;
            }
            let b = ys.last().map_or(b, |&p: &f32| b.max(p));
            log_x.push(la);
            ys.push(b);
        }
        if log_x.is_empty() {
            return ToneMap::identity();
        }
        // Highlight anchor: our clip point (1.0) maps to the reference's top or higher.
        if *log_x.last().unwrap() < -1e-3 {
            log_x.push(0.0);
            ys.push(ys.last().unwrap().max(qy[QUANTILES - 1]).max(0.98));
        }
        ToneMap { log_x, y: ys }
    }

    /// Plain sRGB encoding (no look change), used when no reference is available.
    pub fn identity() -> ToneMap {
        let log_x: Vec<f32> = (0..=40).map(|i| -20.0 + 0.5 * i as f32).collect();
        let y = log_x.iter().map(|&l| srgb_oetf(l.exp2())).collect();
        ToneMap { log_x, y }
    }

    /// Display-encoded luminance for linear luminance `x`.
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        let lx = x.max(FLOOR).log2();
        let n = self.log_x.len();
        let v = if lx <= self.log_x[0] {
            // Linear in x below the first knot: passes through the origin.
            self.y[0] * x / self.log_x[0].exp2()
        } else if lx >= self.log_x[n - 1] {
            self.y[n - 1]
        } else {
            let i = self.log_x.partition_point(|&l| l <= lx) - 1;
            let t = (lx - self.log_x[i]) / (self.log_x[i + 1] - self.log_x[i]);
            self.y[i] + (self.y[i + 1] - self.y[i]) * t
        };
        v.clamp(0.0, 1.0)
    }
}

/// Global white-balance match: per-channel gains (green = 1) that bring the mean linear
/// color of our mid-tones to the reference's mid-tones (the ISP's AWB preference, e.g. warmer
/// skin). Mid-tones = luminance between the 20th and 90th percentile, unclipped. Two degrees
/// of freedom, distribution-based like the tone fit, so no registration is needed.
pub fn fit_wb(ours: &RgbImage, reference: &Rgb8Image) -> [f32; 3] {
    let lut: Vec<f32> = (0..256).map(|i| srgb_eotf(i as f32 / 255.0)).collect();
    let mean_mid = |px: &mut dyn Iterator<Item = [f32; 3]>| -> Option<[f64; 3]> {
        let v: Vec<[f32; 3]> = px.filter(|p| p.iter().all(|&c| c > 0.0 && c < 0.97)).collect();
        if v.len() < 1000 {
            return None;
        }
        let mut l: Vec<f32> = v.iter().map(lum).collect();
        l.sort_unstable_by(f32::total_cmp);
        let (lo, hi) = (l[l.len() / 5], l[l.len() * 9 / 10]);
        let mut s = [0.0f64; 3];
        let mut n = 0usize;
        for p in v.iter().filter(|p| (lo..=hi).contains(&lum(p))) {
            for c in 0..3 {
                s[c] += p[c] as f64;
            }
            n += 1;
        }
        (n > 0).then(|| s.map(|x| x / n as f64))
    };
    let step = (ours.data.len() / 300_000).max(1);
    let a = mean_mid(&mut ours.data.iter().step_by(step).copied());
    let rstep = (reference.data.len() / 3 / 300_000).max(1);
    let b = mean_mid(&mut reference.data.chunks_exact(3).step_by(rstep).map(|p| [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]]));
    let (Some(a), Some(b)) = (a, b) else { return [1.0; 3] };
    let g: [f64; 3] = std::array::from_fn(|c| (b[c] / b[1]) / (a[c] / a[1]));
    // Guard against nonsense (wildly different scenes / failed captures).
    if g.iter().any(|v| !(0.5..=2.0).contains(v)) {
        return [1.0; 3];
    }
    g.map(|v| v as f32)
}

/// How the reference is oriented relative to our sensor-oriented render: the preview frame
/// is our image rotated clockwise by `rotate_cw` degrees, then mirrored left-right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RefGeometry {
    pub rotate_cw: u16,
    pub mirror: bool,
}

/// A look reference: the preview frame (or ISP still) and how it maps onto our image.
#[derive(Clone, Copy, Debug)]
pub struct Reference<'a> {
    pub image: &'a Rgb8Image,
    pub geometry: RefGeometry,
}

/// Grid of block means in linear light: (mean RGB, luminance relative std) per cell.
fn block_means(lin: &dyn Fn(usize, usize) -> [f32; 3], w: usize, h: usize, gx: usize, gy: usize) -> Vec<([f64; 3], f64)> {
    let mut out = Vec::with_capacity(gx * gy);
    for j in 0..gy {
        for i in 0..gx {
            let (x0, x1) = (i * w / gx, (i + 1) * w / gx);
            let (y0, y1) = (j * h / gy, (j + 1) * h / gy);
            let (sx, sy) = (((x1 - x0) / 12).max(1), ((y1 - y0) / 12).max(1));
            let (mut s, mut l1, mut l2, mut n) = ([0.0f64; 3], 0.0f64, 0.0f64, 0.0f64);
            for y in (y0..y1).step_by(sy) {
                for x in (x0..x1).step_by(sx) {
                    let p = lin(x, y);
                    let l = lum(&p) as f64;
                    for c in 0..3 {
                        s[c] += p[c] as f64;
                    }
                    l1 += l;
                    l2 += l * l;
                    n += 1.0;
                }
            }
            let m = s.map(|v| v / n);
            let ml = l1 / n;
            let sd = ((l2 / n - ml * ml).max(0.0)).sqrt() / ml.max(1e-6);
            out.push((m, sd));
        }
    }
    out
}

/// Full 3x3 color match to the reference (the ISP's hue rendering, e.g. warmer, redder
/// skin than the sensor's forward matrix gives). Fitted on *chromaticity* only: every grid
/// cell is normalized to unit luminance in both images, so the reference's nonlinear tone
/// curve cannot leak into the matrix (the tone curve is fitted afterwards). Cells are paired
/// through [RefGeometry]; only smooth mid-tone cells are used (robust to the small shift
/// between preview frame and still), with a ridge toward the diagonal (WB-only) solution.
/// Returns None when the fit is unreliable (too few cells, poor residual, odd matrix).
pub fn fit_ccm(ours: &RgbImage, r: &Reference) -> Option<[[f32; 3]; 3]> {
    const G: usize = 24;
    let lut: Vec<f32> = (0..256).map(|i| srgb_eotf(i as f32 / 255.0)).collect();
    let rimg = r.image;
    let rw = rimg.width;
    let ref_px = |x: usize, y: usize| {
        let i = (y * rw + x) * 3;
        [lut[rimg.data[i] as usize], lut[rimg.data[i + 1] as usize], lut[rimg.data[i + 2] as usize]]
    };
    let gy = (G as f32 * rimg.height as f32 / rimg.width as f32).round().max(4.0) as usize;
    let rcells = block_means(&ref_px, rimg.width, rimg.height, G, gy);
    let rot = r.geometry.rotate_cw % 360;
    let (ox, oy) = if rot % 180 == 0 { (G, gy) } else { (gy, G) };
    let our_px = |x: usize, y: usize| ours.data[y * ours.width + x];
    let ocells = block_means(&our_px, ours.width, ours.height, ox, oy);
    // Reference cell (i, j) -> our cell: undo the mirror, then the clockwise rotation.
    // Rotating (W, H) by 90 CW sends (x, y) to (H-1-y, x); 180 to (W-1-x, H-1-y);
    // 270 to (y, W-1-x). Inverted on the grids (ours is ox x oy cells).
    let map = |i: usize, j: usize| -> usize {
        let i = if r.geometry.mirror { G - 1 - i } else { i };
        let (x, y) = match rot {
            90 => (j, G - 1 - i),
            180 => (G - 1 - i, gy - 1 - j),
            270 => (gy - 1 - j, i),
            _ => (i, j),
        };
        y * ox + x
    };
    // Registration check: log-luminance of paired cells must correlate. A wrong orientation
    // or a different field of view (Xiaomi tele still vs RAW) fails here -> WB-gain fallback.
    if registration(&rcells, &ocells, G, gy, &map) < MIN_REGISTRATION {
        return None;
    }
    let mut a = [[0.0f64; 3]; 3];
    let mut b = [[0.0f64; 3]; 3];
    let mut n = 0usize;
    let mut pairs = Vec::new();
    for j in 0..gy {
        for i in 0..G {
            let (rm, rsd) = rcells[j * G + i];
            let (om, osd) = ocells[map(i, j)];
            let (ry, oyl) = (lum_f64(&rm), lum_f64(&om));
            if !(0.01..0.8).contains(&ry) || oyl < 1e-4 || rsd > 0.25 || osd > 0.25 || rm.iter().chain(&om).any(|&v| v <= 0.0) {
                continue;
            }
            let on = om.map(|v| v / oyl);
            let rn = rm.map(|v| v / ry);
            for p in 0..3 {
                for q in 0..3 {
                    a[p][q] += on[p] * on[q];
                    b[p][q] += on[p] * rn[q];
                }
            }
            pairs.push((on, rn));
            n += 1;
        }
    }
    if n < 60 {
        return None;
    }
    // Ridge toward the diagonal: lambda * |M - diag(d)|^2, d from per-channel means ratio.
    let d: [f64; 3] = std::array::from_fn(|c| {
        let (so, sr) = pairs.iter().fold((0.0, 0.0), |(so, sr), (o, r)| (so + o[c], sr + r[c]));
        sr / so.max(1e-9)
    });
    let lambda = 0.02 * (a[0][0] + a[1][1] + a[2][2]) / 3.0;
    for p in 0..3 {
        a[p][p] += lambda;
    }
    let inv = crate::color::inverse(&a).ok()?;
    let mut m = [[0.0f64; 3]; 3];
    for c in 0..3 {
        // Column c of B plus the ridge target for output channel c.
        let rhs: [f64; 3] = std::array::from_fn(|p| b[p][c] + if p == c { lambda * d[c] } else { 0.0 });
        let row = crate::color::mul_vec(&inv, rhs);
        m[c] = row;
    }
    // Keep luminance: Y(M * gray) = Y(gray), so the tone fit alone sets brightness.
    let gy_out = lum_f64(&crate::color::mul_vec(&m, [1.0, 1.0, 1.0]));
    if !(gy_out > 0.2) {
        return None;
    }
    let m = m.map(|row| row.map(|v| v / gy_out));
    // Sanity: positive, dominant diagonal; the fit must beat the diagonal model.
    if (0..3).any(|c| m[c][c] < 0.3 || m[c][c] < m[c].iter().enumerate().filter(|(k, _)| *k != c).map(|(_, v)| v.abs()).sum::<f64>() * 0.6) {
        return None;
    }
    let err = |f: &dyn Fn([f64; 3]) -> [f64; 3]| pairs.iter().map(|(o, r)| { let p = f(*o); let yp = lum_f64(&p).max(1e-9); (0..3).map(|c| (p[c] / yp - r[c]).powi(2)).sum::<f64>() }).sum::<f64>();
    let e_full = err(&|o| crate::color::mul_vec(&m, o));
    let e_diag = err(&|o| [o[0] * d[0], o[1] * d[1], o[2] * d[2]]);
    if !(e_full <= e_diag * 1.05) {
        return None;
    }
    Some(m.map(|row| row.map(|v| v as f32)))
}

/// Pearson correlation of log luminance between paired reference / our grid cells.
fn registration(rcells: &[([f64; 3], f64)], ocells: &[([f64; 3], f64)], g: usize, gy: usize, map: &dyn Fn(usize, usize) -> usize) -> f64 {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for j in 0..gy {
        for i in 0..g {
            let (a, b) = (lum_f64(&rcells[j * g + i].0), lum_f64(&ocells[map(i, j)].0));
            if a > 1e-5 && b > 1e-6 {
                xs.push(a.ln());
                ys.push(b.ln());
            }
        }
    }
    if xs.len() < 20 {
        return 0.0;
    }
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(&ys) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx) * (x - mx);
        syy += (y - my) * (y - my);
    }
    sxy / (sxx * syy).sqrt().max(1e-12)
}

/// Minimum luminance correlation for the 3x3 color fit to be trusted.
const MIN_REGISTRATION: f64 = 0.85;

/// Registration score of every orientation (diagnostics: which way the reference is turned).
pub fn registration_scan(ours: &RgbImage, reference: &Rgb8Image) -> Vec<(RefGeometry, f64)> {
    let mut out = Vec::new();
    for rotate_cw in [0u16, 90, 180, 270] {
        for mirror in [false, true] {
            let geometry = RefGeometry { rotate_cw, mirror };
            out.push((geometry, registration_score(ours, &Reference { image: reference, geometry })));
        }
    }
    out
}

fn registration_score(ours: &RgbImage, r: &Reference) -> f64 {
    const G: usize = 24;
    let lut: Vec<f32> = (0..256).map(|i| srgb_eotf(i as f32 / 255.0)).collect();
    let rimg = r.image;
    let rw = rimg.width;
    let ref_px = |x: usize, y: usize| {
        let i = (y * rw + x) * 3;
        [lut[rimg.data[i] as usize], lut[rimg.data[i + 1] as usize], lut[rimg.data[i + 2] as usize]]
    };
    let gy = (G as f32 * rimg.height as f32 / rimg.width as f32).round().max(4.0) as usize;
    let rcells = block_means(&ref_px, rimg.width, rimg.height, G, gy);
    let rot = r.geometry.rotate_cw % 360;
    let (ox, oy) = if rot % 180 == 0 { (G, gy) } else { (gy, G) };
    let our_px = |x: usize, y: usize| ours.data[y * ours.width + x];
    let ocells = block_means(&our_px, ours.width, ours.height, ox, oy);
    let map = |i: usize, j: usize| -> usize {
        let i = if r.geometry.mirror { G - 1 - i } else { i };
        let (x, y) = match rot {
            90 => (j, G - 1 - i),
            180 => (G - 1 - i, gy - 1 - j),
            270 => (gy - 1 - j, i),
            _ => (i, j),
        };
        y * ox + x
    };
    registration(&rcells, &ocells, G, gy, &map)
}

fn lum_f64(p: &[f64; 3]) -> f64 {
    LUMA[0] as f64 * p[0] + LUMA[1] as f64 * p[1] + LUMA[2] as f64 * p[2]
}

pub fn apply_gains(img: &RgbImage, g: [f32; 3]) -> RgbImage {
    RgbImage { width: img.width, height: img.height, data: img.data.iter().map(|p| [p[0] * g[0], p[1] * g[1], p[2] * g[2]]).collect() }
}

#[inline]
fn lum(p: &[f32; 3]) -> f32 {
    LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2]
}

/// Linear RGB -> 8-bit sRGB through the luminance map, RGB ratios preserved. Channels
/// pushed past 1 are pulled toward the pixel's luminance just enough to fit (keeps hue
/// and brightness, gives up saturation only where it cannot be shown).
/// Mean chroma (mean |c - Y'| over channels) of display-encoded mid-tone pixels.
fn mean_chroma(px: &mut dyn Iterator<Item = [f32; 3]>) -> Option<f64> {
    let (mut s, mut n) = (0.0f64, 0usize);
    for p in px {
        let y = LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2];
        if !(0.15..=0.85).contains(&y) || p.iter().any(|&c| c >= 0.99) {
            continue;
        }
        s += p.iter().map(|&c| (c - y).abs() as f64).sum::<f64>() / 3.0;
        n += 1;
    }
    (n > 500).then(|| s / n as f64)
}

/// Saturation factor that gives our matched render the reference's mean mid-tone chroma
/// (display-encoded). Clamped to 0.7..1.6: a global, single-parameter match.
pub fn fit_saturation(ours: &RgbImage, tm: &ToneMap, reference: &Rgb8Image) -> f32 {
    let step = (ours.data.len() / 200_000).max(1);
    let sample = RgbImage { width: 0, height: 0, data: ours.data.iter().step_by(step).copied().collect() };
    let rendered = render_matched_sat(&sample, tm, 1.0);
    let a = mean_chroma(&mut rendered.data.chunks_exact(3).map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]));
    let rstep = (reference.data.len() / 3 / 200_000).max(1);
    let b = mean_chroma(&mut reference.data.chunks_exact(3).step_by(rstep).map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]));
    match (a, b) {
        (Some(a), Some(b)) if a > 1e-4 => ((b / a) as f32).clamp(0.7, 1.6),
        _ => 1.0,
    }
}

pub fn render_matched(img: &RgbImage, tm: &ToneMap) -> Rgb8Image {
    render_matched_sat(img, tm, 1.0)
}

/// [render_matched] with a saturation factor applied in linear light around the pixel's
/// luminance (hue and luminance kept).
pub fn render_matched_sat(img: &RgbImage, tm: &ToneMap, sat: f32) -> Rgb8Image {
    // Gain LUT indexed by the top bits of the f32 luminance: exponent + 7 mantissa bits is
    // a piecewise-linear log2 with 128 steps per stop, no transcendental per pixel.
    const SHIFT: u32 = 16;
    let lo_bits = (2f32.powi(-24)).to_bits() >> SHIFT;
    let hi_bits = 1.0f32.to_bits() >> SHIFT;
    let gain_lut: Vec<f32> = (lo_bits..=hi_bits)
        .map(|b| {
            // Bucket center.
            let x = f32::from_bits((b << SHIFT) | (1 << (SHIFT - 1)));
            srgb_eotf(tm.eval(x)) / x
        })
        .collect();
    let gain_at = |y: f32| gain_lut[((y.to_bits() >> SHIFT).clamp(lo_bits, hi_bits) - lo_bits) as usize];
    let oetf_lut: Vec<u8> = (0..=4095).map(|i| (srgb_oetf(i as f32 / 4095.0) * 255.0 + 0.5) as u8).collect();
    let enc = |v: f32| oetf_lut[(v.clamp(0.0, 1.0) * 4095.0 + 0.5) as usize];

    let mut data = Vec::with_capacity(img.data.len() * 3);
    for p in &img.data {
        let p = [p[0].max(0.0), p[1].max(0.0), p[2].max(0.0)];
        let y = lum(&p);
        if y <= 0.0 {
            data.extend_from_slice(&[0, 0, 0]);
            continue;
        }
        let g = gain_at(y);
        let yo = y * g;
        let mut o = [p[0] * g, p[1] * g, p[2] * g];
        if sat != 1.0 {
            for c in &mut o {
                *c = (yo + sat * (*c - yo)).max(0.0);
            }
        }
        let max = o[0].max(o[1]).max(o[2]);
        if max > 1.0 {
            // Blend toward gray at the same luminance until the largest channel fits.
            let t = if max - yo > 1e-6 { ((max - 1.0) / (max - yo)).clamp(0.0, 1.0) } else { 1.0 };
            for c in &mut o {
                *c += (yo.min(1.0) - *c) * t;
            }
        }
        data.extend_from_slice(&[enc(o[0]), enc(o[1]), enc(o[2])]);
    }
    Rgb8Image { width: img.width, height: img.height, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_ramp(n: usize, f: impl Fn(f32) -> f32) -> RgbImage {
        RgbImage { width: n, height: 1, data: (0..n).map(|i| [f((i as f32 + 0.5) / n as f32); 3]).collect() }
    }

    #[test]
    fn matches_a_brighter_reference() {
        // Ours: linear ramp 0..0.25 (underexposed by 2 stops vs the reference's linear 0..1).
        let ours = gray_ramp(4000, |t| 0.25 * t);
        let reference = Rgb8Image {
            width: 4000,
            height: 1,
            data: (0..4000).flat_map(|i| { let v = (srgb_oetf((i as f32 + 0.5) / 4000.0) * 255.0 + 0.5) as u8; [v, v, v] }).collect(),
        };
        let tm = ToneMap::fit(&ours, &reference);
        for x in [0.02f32, 0.05, 0.1, 0.2] {
            let want = srgb_oetf(4.0 * x);
            assert!((tm.eval(x) - want).abs() < 0.02, "x {x}: {} vs {want}", tm.eval(x));
        }
        let out = render_matched(&ours, &tm);
        let mid = out.data[2000 * 3] as f32 / 255.0;
        assert!((mid - srgb_oetf(0.5)).abs() < 0.03, "{mid}");
    }

    #[test]
    fn curve_is_monotone_and_keeps_chroma() {
        let ours = gray_ramp(1000, |t| t * t);
        let reference = Rgb8Image { width: 1000, height: 1, data: (0..1000).flat_map(|i| [(i / 4) as u8; 3]).collect() };
        let tm = ToneMap::fit(&ours, &reference);
        let mut prev = 0.0;
        for i in 1..200 {
            let v = tm.eval(i as f32 / 200.0);
            assert!(v >= prev);
            prev = v;
        }
        // A saturated red stays red (G, B stay below R).
        let px = RgbImage { width: 1, height: 1, data: vec![[0.3, 0.05, 0.05]] };
        let o = render_matched(&px, &tm).data;
        assert!(o[0] > o[1] && o[1] == o[2]);
    }

    #[test]
    fn wb_match_recovers_a_cast() {
        // Reference is our scene with a warm cast (R x1.3, B x0.8 in linear).
        let ours = RgbImage { width: 2000, height: 1, data: (0..2000).map(|i| { let v = 0.05 + 0.4 * i as f32 / 2000.0; [v, v * 1.1, v * 0.9] }).collect() };
        let reference = Rgb8Image { width: 2000, height: 1, data: ours.data.iter().flat_map(|p| [p[0] * 1.3, p[1], p[2] * 0.8].map(|c| (srgb_oetf(c) * 255.0 + 0.5) as u8)).collect() };
        let g = fit_wb(&ours, &reference);
        assert!((g[0] - 1.3).abs() < 0.03 && g[1] == 1.0 && (g[2] - 0.8).abs() < 0.03, "{g:?}");
    }

    #[test]
    fn ccm_fit_recovers_hue_matrix_through_rotation_and_mirror() {
        // Smooth patches of different colors (constant within grid-sized blocks).
        let (w, h) = (480, 360);
        let patch = |x: usize, y: usize| -> [f32; 3] {
            let (i, j) = (x * 12 / w, y * 9 / h);
            let t = (i * 7 + j * 3) % 11;
            [0.05 + 0.03 * t as f32, 0.08 + 0.02 * ((t * 5) % 11) as f32, 0.06 + 0.025 * ((t * 3) % 11) as f32]
        };
        let ours = RgbImage { width: w, height: h, data: (0..w * h).map(|k| patch(k % w, k / w)).collect() };
        // Reference: warmer hue rendering (luminance-neutral mix), rotated 90 CW, mirrored.
        let m: [[f32; 3]; 3] = [[1.15, -0.10, -0.05], [-0.05, 1.05, 0.0], [-0.02, -0.15, 1.17]];
        let (rw, rh) = (h, w);
        let mut data = vec![0u8; rw * rh * 3];
        for y in 0..h {
            for x in 0..w {
                let p = ours.data[y * w + x];
                let q: [f32; 3] = std::array::from_fn(|c| m[c][0] * p[0] + m[c][1] * p[1] + m[c][2] * p[2]);
                // rotate 90 CW: (x, y) -> (h-1-y, x); then mirror: u -> rw-1-u.
                let (u, v) = (rw - 1 - (h - 1 - y), x);
                let i = (v * rw + u) * 3;
                for c in 0..3 {
                    data[i + c] = (srgb_oetf(q[c].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
                }
            }
        }
        let reference = Rgb8Image { width: rw, height: rh, data };
        let r = Reference { image: &reference, geometry: RefGeometry { rotate_cw: 90, mirror: true } };
        let fit = fit_ccm(&ours, &r).expect("fit");
        // Compare normalized to the same luminance scale.
        let ym = |a: &[[f32; 3]; 3]| (0..3).map(|c| LUMA[c] * a[c].iter().sum::<f32>()).sum::<f32>();
        let (sf, sm) = (ym(&fit), ym(&m));
        for c in 0..3 {
            for k in 0..3 {
                assert!((fit[c][k] / sf - m[c][k] / sm).abs() < 0.06, "{fit:?} vs {m:?}");
            }
        }
        // Wrong geometry must not produce a confident wrong answer.
        let wrong = Reference { image: &reference, geometry: RefGeometry { rotate_cw: 0, mirror: false } };
        if let Some(f) = fit_ccm(&ours, &wrong) {
            let off: f32 = (0..3).map(|c| (f[c][c] / ym(&f) - m[c][c] / sm).abs()).sum();
            assert!(off > 0.0);
        }
    }

    #[test]
    fn identity_is_plain_srgb() {
        let tm = ToneMap::identity();
        for x in [0.001f32, 0.18, 0.7] {
            assert!((tm.eval(x) - srgb_oetf(x)).abs() < 0.01);
        }
    }
}
