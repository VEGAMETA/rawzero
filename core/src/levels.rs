//! Step 2: black / white levels -> normalized linear mosaic.

use crate::image::{Mosaic, RawImage};
use crate::meta::{cell, FrameMeta};

/// `v' = (v - black[cell]) / (white - black[cell])`, per 2x2 CFA position
/// (DNG spec 1.6, ch. 5 "Black Level / White Level"; Camera2 `SENSOR_BLACK_LEVEL_PATTERN`).
///
/// No clamping: noise below black stays negative so shadow means are not biased upward;
/// values above 1 are clipped later, after white balance.
pub fn apply_levels(raw: &RawImage, meta: &FrameMeta) -> Mosaic {
    let scale: [f32; 4] = std::array::from_fn(|i| 1.0 / (meta.white_level - meta.black_level[i]));
    let bl = meta.black_level;
    let mut data = Vec::with_capacity(raw.data.len());
    for (y, row) in raw.data.chunks_exact(raw.width).enumerate() {
        let (c0, c1) = (cell(0, y), cell(1, y));
        for pair in row.chunks(2) {
            data.push((pair[0] as f32 - bl[c0]) * scale[c0]);
            if let Some(&v) = pair.get(1) {
                data.push((v as f32 - bl[c1]) * scale[c1]);
            }
        }
    }
    Mosaic { width: raw.width, height: raw.height, data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::meta_rggb;

    #[test]
    fn per_channel_black_and_white() {
        let mut m = meta_rggb();
        m.black_level = [64.0, 60.0, 68.0, 64.0];
        m.white_level = 1023.0;
        let raw = RawImage::new(3, 2, vec![64, 60, 1023, 68, 64, 0]).unwrap();
        let out = apply_levels(&raw, &m);
        let exp = [0.0, 0.0, 1.0, 0.0, 0.0, -68.0 / 955.0];
        for (a, b) in out.data.iter().zip(exp) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }
}
