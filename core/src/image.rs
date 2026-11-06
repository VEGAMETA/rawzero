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

#[cfg(test)]
mod tests {
    use super::*;

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
