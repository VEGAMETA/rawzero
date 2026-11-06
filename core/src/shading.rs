//! Step 3: lens shading (vignetting + color shading) correction.

use crate::image::Mosaic;
use crate::meta::{cell, CfaPattern, LensShadingMap};

/// Multiplies every photosite by the gain of its color channel, bilinearly interpolated
/// from the map. Grid corners coincide with frame corners (Camera2
/// `STATISTICS_LENS_SHADING_CORRECTION_MAP`; DNG `GainMap` opcode with origin 0 and
/// spacing 1/(n-1), as written by AOSP DngCreator).
pub fn apply_lens_shading(m: &Mosaic, map: &LensShadingMap, cfa: CfaPattern) -> Mosaic {
    let ch: [usize; 4] = std::array::from_fn(|p| cfa.camera2_channel(p));
    let cols: Vec<(usize, usize, f32)> = (0..m.width).map(|x| axis(x, m.width, map.cols)).collect();
    let mut data = Vec::with_capacity(m.data.len());
    for (y, row) in m.data.chunks_exact(m.width).enumerate() {
        let (r0, r1, fy) = axis(y, m.height, map.rows);
        let (g0, g1) = (&map.gains[r0 * map.cols..], &map.gains[r1 * map.cols..]);
        for (x, &v) in row.iter().enumerate() {
            let (c0, c1, fx) = cols[x];
            let c = ch[cell(x, y)];
            let top = g0[c0][c] + (g0[c1][c] - g0[c0][c]) * fx;
            let bot = g1[c0][c] + (g1[c1][c] - g1[c0][c]) * fx;
            data.push(v * (top + (bot - top) * fy));
        }
    }
    Mosaic { width: m.width, height: m.height, data }
}

/// Maps pixel index `i` of `n` onto a grid of `len` samples: (lower, upper, fraction).
fn axis(i: usize, n: usize, len: usize) -> (usize, usize, f32) {
    if len < 2 || n < 2 {
        return (0, 0, 0.0);
    }
    let t = i as f32 * (len - 1) as f32 / (n - 1) as f32;
    let i0 = (t as usize).min(len - 2);
    (i0, i0 + 1, t - i0 as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_picks_channel() {
        // 2x2 grid: gain 1 on the left edge, 3 on the right; red channel doubled.
        let g = |l: f32| [2.0 * l, l, l, l];
        let map = LensShadingMap { rows: 2, cols: 2, gains: vec![g(1.0), g(3.0), g(1.0), g(3.0)] };
        let m = Mosaic { width: 3, height: 2, data: vec![1.0; 6] };
        let out = apply_lens_shading(&m, &map, CfaPattern::Rggb);
        // Row 0: R G R at x = 0, 1, 2 -> gains 2*1, 2, 2*3.
        assert_eq!(out.data, vec![2.0, 2.0, 6.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn axis_endpoints() {
        assert_eq!(axis(0, 100, 17), (0, 1, 0.0));
        let (a, b, f) = axis(99, 100, 17);
        assert_eq!((a, b), (15, 16));
        assert!((f - 1.0).abs() < 1e-6);
    }
}
