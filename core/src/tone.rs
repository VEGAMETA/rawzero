//! Step 8: one fixed global tone curve, then the sRGB OETF, quantized to 8 bit.

use crate::image::{Rgb8Image, RgbImage};

/// Log-log contrast at middle gray. 1.0 = no curve. Gentle by design; the look is the
/// owner's call, change only this constant.
pub const CONTRAST: f32 = 1.15;
/// Scene middle gray, kept fixed by the curve.
pub const MID_GRAY: f32 = 0.18;

/// `f(x) = x^c / (x^c + k (1 - x)^c)` with `k = (g / (1 - g))^(c - 1)`.
///
/// Geometry: in log-log space the slope at middle gray `g` is exactly `c`, `f(g) = g`,
/// `f(0) = 0`, `f(1) = 1`, and `f'(1) = 0` for `c > 1`, so white rolls off smoothly
/// instead of hitting a hard knee. Monotonic for `c > 0`. Applied per channel.
pub fn tone_curve(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if CONTRAST == 1.0 || x == 0.0 || x == 1.0 {
        return x;
    }
    let k = (MID_GRAY / (1.0 - MID_GRAY)).powf(CONTRAST - 1.0);
    let a = x.powf(CONTRAST);
    a / (a + k * (1.0 - x).powf(CONTRAST))
}

/// IEC 61966-2-1 sRGB transfer function, linear -> encoded.
pub fn srgb_oetf(x: f32) -> f32 {
    if x <= 0.003_130_8 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 }
}

/// Lookup resolution over linear [0, 1]. 2^16 entries: one step is 1.5e-5 linear, which in
/// the deepest shadows (OETF slope 12.92) is 0.05 of an 8-bit code, so the LUT adds no
/// visible quantization while replacing two `powf` per sample.
const LUT_BITS: u32 = 16;

pub fn build_lut() -> Vec<u8> {
    let n = 1usize << LUT_BITS;
    (0..n)
        .map(|i| {
            let x = i as f32 / (n - 1) as f32;
            (srgb_oetf(tone_curve(x)) * 255.0 + 0.5).clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// Clips to [0, 1] per channel (out-of-gamut and sub-black values), applies the curve
/// and the OETF, rounds to 8 bit.
pub fn render_8bit(img: &RgbImage, lut: &[u8]) -> Rgb8Image {
    let scale = (lut.len() - 1) as f32;
    let mut data = Vec::with_capacity(img.data.len() * 3);
    for p in &img.data {
        for &v in p {
            data.push(lut[(v.clamp(0.0, 1.0) * scale + 0.5) as usize]);
        }
    }
    Rgb8Image { width: img.width, height: img.height, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_fixed_points_and_monotonic() {
        assert_eq!(tone_curve(0.0), 0.0);
        assert_eq!(tone_curve(1.0), 1.0);
        assert!((tone_curve(MID_GRAY) - MID_GRAY).abs() < 1e-6);
        let mut prev = 0.0;
        for i in 1..=1000 {
            let y = tone_curve(i as f32 / 1000.0);
            assert!(y >= prev);
            prev = y;
        }
        // Log-log slope at gray equals CONTRAST.
        let (a, b) = (MID_GRAY * 0.99, MID_GRAY * 1.01);
        let slope = (tone_curve(b).ln() - tone_curve(a).ln()) / (b.ln() - a.ln());
        assert!((slope - CONTRAST).abs() < 1e-3, "{slope}");
    }

    #[test]
    fn oetf_reference_values() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_oetf(0.18) - 0.4613).abs() < 1e-3);
    }

    #[test]
    fn render_clips() {
        let lut = build_lut();
        let img = RgbImage { width: 1, height: 1, data: vec![[-0.5, 1.7, MID_GRAY]] };
        let out = render_8bit(&img, &lut);
        assert_eq!(&out.data[..2], &[0, 255]);
        assert_eq!(out.data[2], 118); // 0.4613 * 255
    }
}
