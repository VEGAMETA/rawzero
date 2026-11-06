//! Step 1: packed RAW10 / RAW12 -> u16. `RAW_SENSOR` is already u16 and skips this.

use crate::image::RawImage;
use crate::Error;

/// `ImageFormat.RAW10`: 4 pixels in 5 bytes. Bytes 0..4 hold bits 9..2 of pixels 0..3,
/// byte 4 holds bits 1..0 of pixel k at bit positions 2k+1..2k (Android `ImageFormat` docs).
pub fn unpack_raw10(src: &[u8], width: usize, height: usize, row_stride: usize) -> Result<RawImage, Error> {
    check(src, width, height, row_stride, 4, 5)?;
    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = &src[y * row_stride..y * row_stride + width / 4 * 5];
        for g in row.chunks_exact(5) {
            let lo = g[4] as u16;
            for k in 0..4 {
                data.push((g[k] as u16) << 2 | (lo >> (2 * k)) & 0b11);
            }
        }
    }
    RawImage::new(width, height, data)
}

/// `ImageFormat.RAW12`: 2 pixels in 3 bytes. Bytes 0, 1 hold bits 11..4 of pixels 0, 1;
/// byte 2 holds bits 3..0 of pixel 0 in its low nibble and of pixel 1 in its high nibble.
pub fn unpack_raw12(src: &[u8], width: usize, height: usize, row_stride: usize) -> Result<RawImage, Error> {
    check(src, width, height, row_stride, 2, 3)?;
    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = &src[y * row_stride..y * row_stride + width / 2 * 3];
        for g in row.chunks_exact(3) {
            data.push((g[0] as u16) << 4 | (g[2] & 0x0f) as u16);
            data.push((g[1] as u16) << 4 | (g[2] >> 4) as u16);
        }
    }
    RawImage::new(width, height, data)
}

/// `ImageFormat.RAW_SENSOR`: one little-endian u16 per pixel, rows `row_stride` bytes apart
/// (`Image.Plane.getRowStride()`).
pub fn unpack_raw16(src: &[u8], width: usize, height: usize, row_stride: usize) -> Result<RawImage, Error> {
    check(src, width, height, row_stride, 1, 2)?;
    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = &src[y * row_stride..y * row_stride + width * 2];
        data.extend(row.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])));
    }
    RawImage::new(width, height, data)
}

fn check(src: &[u8], w: usize, h: usize, stride: usize, px: usize, bytes: usize) -> Result<(), Error> {
    let row_bytes = w / px * bytes;
    if w == 0 || h == 0 || w % px != 0 || stride < row_bytes || src.len() < (h - 1) * stride + row_bytes {
        return Err(Error::InvalidInput(format!(
            "packed raw {w}x{h}, stride {stride}, {} bytes",
            src.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw10_bit_layout() {
        // Pixels 0x3FF, 0x000, 0x2AA, 0x155 -> high bytes FF 00 AA 55, low bits 11 00 10 01.
        let row = [0xFF, 0x00, 0xAA, 0x55, 0b01_10_00_11, 0xEE /* stride padding */];
        let img = unpack_raw10(&row, 4, 1, 6).unwrap();
        assert_eq!(img.data, vec![0x3FF, 0x000, 0x2AA, 0x155]);
    }

    #[test]
    fn raw12_bit_layout() {
        // 0xABC, 0x123 -> AB 12 3C
        let img = unpack_raw12(&[0xAB, 0x12, 0x3C, 0xAB, 0x12, 0x3C], 2, 2, 3).unwrap();
        assert_eq!(img.data, vec![0xABC, 0x123, 0xABC, 0x123]);
    }

    #[test]
    fn raw16_with_stride() {
        let src = [0x34, 0x12, 0xFF, 0x03, 0xAA, 0xAA, 0x01, 0x00, 0x02, 0x00];
        let img = unpack_raw16(&src, 2, 2, 6).unwrap();
        assert_eq!(img.data, vec![0x1234, 0x03FF, 0x0001, 0x0002]);
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(unpack_raw10(&[0; 9], 4, 2, 5).is_err());
        assert!(unpack_raw12(&[0; 3], 3, 1, 3).is_err());
    }
}
