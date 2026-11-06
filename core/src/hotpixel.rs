//! Step 4: hot pixel suppression, only when the HAL did not already do it.

use crate::image::Mosaic;

/// A photosite is hot when it exceeds the brightest of its 8 same-color neighbors
/// (offsets of 2 in a Bayer mosaic) both by `HOT_FACTOR` times and by `HOT_MARGIN`
/// absolute. Such a pixel is replaced by the median of those neighbors; everything
/// else is left untouched, so this is not a denoiser. A real point light covers
/// several photosites after the lens PSF, so neighbors of the same color rise too.
pub const HOT_FACTOR: f32 = 2.0;
pub const HOT_MARGIN: f32 = 0.05;

pub fn fix_hot_pixels(m: &Mosaic) -> Mosaic {
    let (w, h) = (m.width, m.height);
    let mut out = m.clone();
    if w < 5 || h < 5 {
        return out;
    }
    let d = &m.data;
    for y in 2..h - 2 {
        for x in 2..w - 2 {
            let i = y * w + x;
            let v = d[i];
            let mut n = [
                d[i - 2 * w - 2], d[i - 2 * w], d[i - 2 * w + 2],
                d[i - 2], d[i + 2],
                d[i + 2 * w - 2], d[i + 2 * w], d[i + 2 * w + 2],
            ];
            let max = n.iter().copied().fold(f32::MIN, f32::max);
            if v > max * HOT_FACTOR && v - max > HOT_MARGIN {
                n.sort_unstable_by(f32::total_cmp);
                out.data[i] = 0.5 * (n[3] + n[4]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_isolated_spike_only() {
        let (w, h) = (9, 9);
        let mut data: Vec<f32> = (0..w * h).map(|i| 0.1 + 0.001 * (i % 7) as f32).collect();
        data[4 * w + 4] = 0.9; // hot
        data[4 * w + 6] = 0.15; // mild bump, keep
        let m = Mosaic { width: w, height: h, data };
        let out = fix_hot_pixels(&m);
        assert!(out.data[4 * w + 4] < 0.11);
        assert_eq!(out.data[4 * w + 6], 0.15);
        let changed = out.data.iter().zip(&m.data).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 1);
    }
}
