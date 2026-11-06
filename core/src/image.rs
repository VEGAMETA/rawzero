//! Plain image buffers passed between pipeline steps.

use crate::Error;

/// Bayer mosaic straight from the sensor, one u16 sample per photosite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u16>,
}

/// Bayer mosaic in normalized linear units (0 = black level, 1 = white level).
/// Values may be slightly negative (noise around black) until the tone step.
#[derive(Clone, Debug, PartialEq)]
pub struct Mosaic {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

/// Linear RGB, interleaved per pixel.
#[derive(Clone, Debug, PartialEq)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 3]>,
}

/// Display-referred 8-bit sRGB, interleaved RGBRGB...
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb8Image {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// Pixel rectangle: `x`, `y` is the top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl RawImage {
    pub fn new(width: usize, height: usize, data: Vec<u16>) -> Result<Self, Error> {
        if width == 0 || height == 0 || data.len() != width * height {
            return Err(Error::InvalidInput(format!(
                "raw buffer of {} samples does not match {width}x{height}",
                data.len()
            )));
        }
        Ok(Self { width, height, data })
    }
}

/// Crops an RGB image. The rectangle must lie inside the image.
pub fn crop(img: &RgbImage, r: Rect) -> Result<RgbImage, Error> {
    if r.width == 0 || r.height == 0 || r.x + r.width > img.width || r.y + r.height > img.height {
        return Err(Error::InvalidInput(format!(
            "crop {r:?} outside {}x{}",
            img.width, img.height
        )));
    }
    let mut data = Vec::with_capacity(r.width * r.height);
    for y in r.y..r.y + r.height {
        let row = y * img.width;
        data.extend_from_slice(&img.data[row + r.x..row + r.x + r.width]);
    }
    Ok(RgbImage { width: r.width, height: r.height, data })
}

/// Mirror axis in sensor (stored) pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mirror {
    None,
    /// Reverse each row (left <-> right in stored pixels).
    Columns,
    /// Reverse row order (top <-> bottom in stored pixels).
    Rows,
}

/// Selfie mirror: front-camera photos keep the left-right flip the preview showed.
/// Which stored axis that is depends on the EXIF rotation, so the caller picks it.
pub fn mirror(img: &Rgb8Image, m: Mirror) -> Rgb8Image {
    let (w, h) = (img.width, img.height);
    let mut data = Vec::with_capacity(img.data.len());
    match m {
        Mirror::None => data.extend_from_slice(&img.data),
        Mirror::Columns => {
            for row in img.data.chunks_exact(w * 3) {
                data.extend(row.chunks_exact(3).rev().flatten());
            }
        }
        Mirror::Rows => {
            for y in (0..h).rev() {
                data.extend_from_slice(&img.data[y * w * 3..(y + 1) * w * 3]);
            }
        }
    }
    Rgb8Image { width: w, height: h, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_axes() {
        // 2x2 pixels a b / c d, one byte per channel value.
        let px = |v: u8| [v, v, v];
        let img = Rgb8Image { width: 2, height: 2, data: [px(1), px(2), px(3), px(4)].concat() };
        let first = |i: &Rgb8Image| i.data.chunks(3).map(|p| p[0]).collect::<Vec<_>>();
        assert_eq!(first(&mirror(&img, Mirror::Columns)), vec![2, 1, 4, 3]);
        assert_eq!(first(&mirror(&img, Mirror::Rows)), vec![3, 4, 1, 2]);
        assert_eq!(first(&mirror(&img, Mirror::None)), vec![1, 2, 3, 4]);
    }

    #[test]
    fn crop_takes_inner_block() {
        let img = RgbImage {
            width: 4,
            height: 3,
            data: (0..12).map(|i| [i as f32; 3]).collect(),
        };
        let c = crop(&img, Rect { x: 1, y: 1, width: 2, height: 2 }).unwrap();
        let v: Vec<f32> = c.data.iter().map(|p| p[0]).collect();
        assert_eq!(v, vec![5.0, 6.0, 9.0, 10.0]);
        assert!(crop(&img, Rect { x: 3, y: 0, width: 2, height: 1 }).is_err());
    }
}
