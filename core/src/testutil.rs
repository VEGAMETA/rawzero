use crate::color::IDENTITY;
use crate::meta::{Calibration, CfaPattern, ColorProfile, FrameMeta};

/// Neutral test metadata: RGGB, 10-bit, identity color (camera == XYZ D50 after WB).
pub fn meta_rggb() -> FrameMeta {
    let d50 = crate::color::xy_to_xyz(crate::color::D50_XY);
    let fm = crate::color::diag(d50);
    let cal = |illuminant| Calibration {
        illuminant,
        color_matrix: IDENTITY,
        forward_matrix: Some(fm),
        camera_calibration: IDENTITY,
    };
    FrameMeta {
        cfa: CfaPattern::Rggb,
        black_level: [0.0; 4],
        white_level: 1023.0,
        lens_shading: None,
        hot_pixels_corrected: true,
        wb_gains: [1.0; 4],
        color: ColorProfile { cal1: cal(17), cal2: Some(cal(21)), analog_balance: [1.0; 3] },
        orientation: 1,
        crop: None,
    }
}
