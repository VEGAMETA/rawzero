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
    fn identity_is_plain_srgb() {
        let tm = ToneMap::identity();
        for x in [0.001f32, 0.18, 0.7] {
            assert!((tm.eval(x) - srgb_oetf(x)).abs() < 0.01);
        }
    }
}
