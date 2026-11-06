//! Step 9: JPEG encoding with a minimal EXIF block (orientation).

use crate::image::Rgb8Image;
use crate::Error;
use jpeg_encoder::{ColorType, Encoder};

pub const DEFAULT_QUALITY: u8 = 92;

/// Baseline JPEG. Quality >= 90 keeps 4:4:4 chroma (jpeg-encoder default), so chroma grain
/// is not averaged away by subsampling.
pub fn encode_jpeg(img: &Rgb8Image, quality: u8, orientation: u16) -> Result<Vec<u8>, Error> {
    let (w, h) = (u16::try_from(img.width), u16::try_from(img.height));
    let (Ok(w), Ok(h)) = (w, h) else {
        return Err(Error::InvalidInput(format!("{}x{} exceeds JPEG limits", img.width, img.height)));
    };
    let mut out = Vec::with_capacity(img.data.len() / 4);
    let mut enc = Encoder::new(&mut out, quality.clamp(1, 100));
    enc.add_exif_metadata(&exif_tiff(orientation)).map_err(|e| Error::Encode(e.to_string()))?;
    enc.encode(&img.data, w, h, ColorType::Rgb).map_err(|e| Error::Encode(e.to_string()))?;
    Ok(out)
}

/// TIFF structure for the EXIF APP1 payload: little-endian header, IFD0 with one entry,
/// Orientation (0x0112, SHORT). EXIF 2.32, sec. 4.5.4 / 4.6.4.
fn exif_tiff(orientation: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(26);
    t.extend_from_slice(b"II");
    t.extend_from_slice(&42u16.to_le_bytes());
    t.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    t.extend_from_slice(&1u16.to_le_bytes()); // entry count
    t.extend_from_slice(&0x0112u16.to_le_bytes());
    t.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&orientation.to_le_bytes());
    t.extend_from_slice(&[0, 0]); // value padded to 4 bytes
    t.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_has_markers_and_exif() {
        let img = Rgb8Image { width: 16, height: 8, data: vec![128; 16 * 8 * 3] };
        let j = encode_jpeg(&img, DEFAULT_QUALITY, 6).unwrap();
        assert_eq!(&j[..2], &[0xFF, 0xD8]);
        assert_eq!(&j[j.len() - 2..], &[0xFF, 0xD9]);
        let pos = j.windows(6).position(|w| w == b"Exif\0\0").expect("EXIF segment");
        let tiff = &j[pos + 6..];
        assert_eq!(&tiff[..4], b"II*\0");
        assert_eq!(u16::from_le_bytes([tiff[18], tiff[19]]), 6);
    }
}
