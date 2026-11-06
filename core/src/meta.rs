//! Frame metadata. On Android it is filled from `CameraCharacteristics` and
//! `CaptureResult`; on desktop from DNG tags. Field docs name both sources.

use crate::color::Mat3;
use crate::image::Rect;
use crate::Error;

/// 2x2 Bayer layout, named by the colors of the top-left 2x2 cell in row-major order.
/// `SENSOR_INFO_COLOR_FILTER_ARRANGEMENT` values 0..=3; DNG `CFAPattern` (33422).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfaPattern {
    Rggb,
    Grbg,
    Gbrg,
    Bggr,
}

/// Color index used inside the pipeline.
pub const R: usize = 0;
pub const G: usize = 1;
pub const B: usize = 2;

impl CfaPattern {
    pub fn from_camera2(v: i32) -> Result<Self, Error> {
        match v {
            0 => Ok(Self::Rggb),
            1 => Ok(Self::Grbg),
            2 => Ok(Self::Gbrg),
            3 => Ok(Self::Bggr),
            _ => Err(Error::Unsupported(format!(
                "SENSOR_INFO_COLOR_FILTER_ARRANGEMENT = {v} (only 2x2 Bayer is supported)"
            ))),
        }
    }

    /// From a 2x2 DNG `CFAPattern` (0 = red, 1 = green, 2 = blue), row-major.
    pub fn from_dng(p: [u8; 4]) -> Result<Self, Error> {
        match p {
            [0, 1, 1, 2] => Ok(Self::Rggb),
            [1, 0, 2, 1] => Ok(Self::Grbg),
            [1, 2, 0, 1] => Ok(Self::Gbrg),
            [2, 1, 1, 0] => Ok(Self::Bggr),
            _ => Err(Error::Unsupported(format!("CFA pattern {p:?}"))),
        }
    }

    /// Colors of the 2x2 cell, row-major: index = (y & 1) * 2 + (x & 1).
    pub fn layout(self) -> [usize; 4] {
        match self {
            Self::Rggb => [R, G, G, B],
            Self::Grbg => [G, R, B, G],
            Self::Gbrg => [G, B, R, G],
            Self::Bggr => [B, G, G, R],
        }
    }

    #[inline]
    pub fn color_at(self, x: usize, y: usize) -> usize {
        self.layout()[cell(x, y)]
    }

    /// Index into Camera2 per-channel arrays ordered `[R, G_even, G_odd, B]`
    /// (`COLOR_CORRECTION_GAINS`, `LensShadingMap`) for a 2x2 cell position.
    /// G_even is the green on the even row of the cell, G_odd on the odd row
    /// (same convention as AOSP DngCreator `addGainMapsForMetadata`).
    pub fn camera2_channel(self, pos: usize) -> usize {
        match self.layout()[pos] {
            R => 0,
            B => 3,
            _ if pos < 2 => 1,
            _ => 2,
        }
    }
}

/// Position inside the 2x2 CFA cell, row-major.
#[inline]
pub fn cell(x: usize, y: usize) -> usize {
    ((y & 1) << 1) | (x & 1)
}

/// `STATISTICS_LENS_SHADING_CORRECTION_MAP`: gain factors on a `rows` x `cols` grid
/// whose corner samples sit on the corners of the frame; bilinear in between.
#[derive(Clone, Debug, PartialEq)]
pub struct LensShadingMap {
    pub rows: usize,
    pub cols: usize,
    /// Row-major grid, each sample `[R, G_even, G_odd, B]`.
    pub gains: Vec<[f32; 4]>,
}

/// Whether the RAW must be shading-corrected with the delivered map.
///
/// Camera2 says to apply `STATISTICS_LENS_SHADING_CORRECTION_MAP` only when
/// `SENSOR_INFO_LENS_SHADING_APPLIED == false`; AOSP DngCreator writes the map into
/// OpcodeList2 regardless of the flag. Measured on white-wall flat fields
/// (`tools/flatfield.py`, 2026-11-06): Samsung S21U reports `true` and its RAW is indeed
/// corrected (applying the map over-brightens corners 1.2-1.5x); Xiaomi 14 reports `true`
/// but its RAW has the full lens falloff (corners 0.12-0.2 of center), so the flag is
/// ignored for Xiaomi.
pub fn needs_lens_shading(make: &str, hal_says_applied: bool) -> bool {
    !hal_says_applied || make.eq_ignore_ascii_case("xiaomi")
}

/// One calibration illuminant of the DNG / Camera2 color model.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    /// EXIF LightSource code: `SENSOR_REFERENCE_ILLUMINANT1/2`, DNG `CalibrationIlluminant1/2`.
    pub illuminant: u16,
    /// XYZ -> reference camera space: `SENSOR_COLOR_TRANSFORM1/2`, DNG `ColorMatrix1/2`.
    pub color_matrix: Mat3,
    /// White-balanced camera -> XYZ(D50): `SENSOR_FORWARD_MATRIX1/2`, DNG `ForwardMatrix1/2`.
    pub forward_matrix: Option<Mat3>,
    /// Reference -> individual camera: `SENSOR_CALIBRATION_TRANSFORM1/2`, DNG `CameraCalibration1/2`.
    pub camera_calibration: Mat3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColorProfile {
    pub cal1: Calibration,
    pub cal2: Option<Calibration>,
    /// DNG `AnalogBalance`; identity on Android.
    pub analog_balance: [f64; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct FrameMeta {
    pub cfa: CfaPattern,
    /// Per 2x2 cell position, row-major. `SENSOR_DYNAMIC_BLACK_LEVEL`, else
    /// `SENSOR_BLACK_LEVEL_PATTERN` (both are in CFA layout order); DNG `BlackLevel`.
    pub black_level: [f32; 4],
    /// `SENSOR_DYNAMIC_WHITE_LEVEL`, else `SENSOR_INFO_WHITE_LEVEL`; DNG `WhiteLevel`.
    pub white_level: f32,
    /// `None` when `SENSOR_INFO_LENS_SHADING_APPLIED == true` or no map was delivered.
    pub lens_shading: Option<LensShadingMap>,
    /// True when the HAL already corrected hot pixels on the RAW stream (`HOT_PIXEL_MODE`).
    pub hot_pixels_corrected: bool,
    /// `COLOR_CORRECTION_GAINS` `[R, G_even, G_odd, B]`; DNG: 1 / `AsShotNeutral`.
    pub wb_gains: [f32; 4],
    pub color: ColorProfile,
    /// EXIF orientation 1..=8, written to the output file; pixels are not rotated.
    pub orientation: u16,
    /// Output crop in sensor coordinates (DNG `DefaultCrop*`); `None` on Android.
    pub crop: Option<Rect>,
}

impl FrameMeta {
    pub fn validate(&self) -> Result<(), Error> {
        for (i, &b) in self.black_level.iter().enumerate() {
            if !(b.is_finite() && b >= 0.0 && b < self.white_level) {
                return Err(Error::InvalidInput(format!(
                    "black level[{i}] = {b} vs white level {}",
                    self.white_level
                )));
            }
        }
        if self.wb_gains.iter().any(|g| !(g.is_finite() && *g > 0.0)) {
            return Err(Error::InvalidInput(format!("WB gains {:?}", self.wb_gains)));
        }
        if !(1..=8).contains(&self.orientation) {
            return Err(Error::InvalidInput(format!("orientation {}", self.orientation)));
        }
        if let Some(m) = &self.lens_shading {
            if m.rows == 0 || m.cols == 0 || m.gains.len() != m.rows * m.cols {
                return Err(Error::InvalidInput(format!(
                    "lens shading map {}x{} with {} samples",
                    m.cols,
                    m.rows,
                    m.gains.len()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera2_channels_follow_cfa() {
        // RGGB: R at (0,0), G_even at (1,0), G_odd at (0,1), B at (1,1).
        let p = CfaPattern::Rggb;
        assert_eq!((0..4).map(|i| p.camera2_channel(i)).collect::<Vec<_>>(), [0, 1, 2, 3]);
        let p = CfaPattern::Grbg;
        assert_eq!((0..4).map(|i| p.camera2_channel(i)).collect::<Vec<_>>(), [1, 0, 3, 2]);
        let p = CfaPattern::Bggr;
        assert_eq!((0..4).map(|i| p.camera2_channel(i)).collect::<Vec<_>>(), [3, 1, 2, 0]);
    }

    #[test]
    fn shading_flag_policy() {
        assert!(needs_lens_shading("samsung", false));
        assert!(!needs_lens_shading("samsung", true));
        assert!(needs_lens_shading("Xiaomi", true));
    }

    #[test]
    fn dng_and_camera2_agree() {
        assert_eq!(CfaPattern::from_dng([1, 2, 0, 1]).unwrap(), CfaPattern::from_camera2(2).unwrap());
        assert!(CfaPattern::from_camera2(4).is_err());
    }
}
