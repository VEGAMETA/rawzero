//! Step 5: white balance on the mosaic, with highlight clipping.

use crate::image::Mosaic;
use crate::meta::{cell, FrameMeta};

/// Per 2x2 cell gains from `COLOR_CORRECTION_GAINS`, normalized so the smallest is 1.
/// With that normalization the camera neutral (max component 1, DNG SDK convention)
/// is `1 / gain`, and a fully clipped photosite stays at 1 in every channel.
pub fn cell_gains(meta: &FrameMeta) -> [f32; 4] {
    let min = meta.wb_gains.iter().copied().fold(f32::MAX, f32::min);
    std::array::from_fn(|p| meta.wb_gains[meta.cfa.camera2_channel(p)] / min)
}

/// Camera neutral `[R, G, B]` implied by the gains (greens averaged), max component 1.
pub fn camera_neutral(meta: &FrameMeta) -> [f64; 3] {
    let g = meta.wb_gains.map(|v| v as f64);
    let n = [1.0 / g[0], 2.0 / (g[1] + g[2]), 1.0 / g[3]];
    let max = n[0].max(n[1]).max(n[2]);
    n.map(|v| v / max)
}

/// `v * gain`, then clip at 1: above the white level the sensor carries no color
/// information, and clipping all channels at the same point keeps blown highlights
/// neutral instead of magenta (dcraw highlight mode 0).
pub fn apply_white_balance(m: &Mosaic, gains: [f32; 4]) -> Mosaic {
    let mut data = Vec::with_capacity(m.data.len());
    for (y, row) in m.data.chunks_exact(m.width).enumerate() {
        data.extend(row.iter().enumerate().map(|(x, &v)| (v * gains[cell(x, y)]).min(1.0)));
    }
    Mosaic { width: m.width, height: m.height, data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::meta_rggb;

    #[test]
    fn gains_normalized_and_clipped() {
        let mut meta = meta_rggb();
        meta.wb_gains = [4.0, 2.0, 2.0, 3.0];
        let g = cell_gains(&meta);
        assert_eq!(g, [2.0, 1.0, 1.0, 1.5]);
        let n = camera_neutral(&meta);
        assert!((n[0] - 0.5).abs() < 1e-12 && n[1] == 1.0 && (n[2] - 2.0 / 3.0).abs() < 1e-12);
        let m = Mosaic { width: 2, height: 2, data: vec![0.3, 1.0, 0.5, 0.9] };
        let out = apply_white_balance(&m, g);
        assert_eq!(out.data, vec![0.6, 1.0, 0.5, 1.0]);
    }
}
