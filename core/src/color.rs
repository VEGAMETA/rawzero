//! Step 7: camera RGB -> XYZ(D50) -> linear sRGB(D65).
//!
//! Follows DNG spec 1.6, ch. 6 "Mapping Camera Color Space to CIE XYZ Space". The pipeline
//! has already multiplied the data by `gain = 1 / neutral` (wb.rs), so the matrix built
//! here is `CameraToXYZ_D50 * diag(neutral)`, which maps white-balanced camera RGB to XYZ.
//! Matrix math runs in f64 once per frame; only the final 3x3 is applied per pixel in f32.

use crate::image::RgbImage;
use crate::meta::{Calibration, ColorProfile};
use crate::Error;

/// Row-major 3x3.
pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// CIE 1931 chromaticities. D50 as used by the DNG spec and ICC PCS; D65 per IEC 61966-2-1.
pub const D50_XY: [f64; 2] = [0.3457, 0.3585];
pub const D65_XY: [f64; 2] = [0.3127, 0.3290];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

pub fn mul_vec(a: &Mat3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|k| a[i][k] * v[k]).sum())
}

pub fn diag(v: [f64; 3]) -> Mat3 {
    [[v[0], 0.0, 0.0], [0.0, v[1], 0.0], [0.0, 0.0, v[2]]]
}

pub fn inverse(m: &Mat3) -> Result<Mat3, Error> {
    let c = |r: usize, s: usize| {
        let (r0, r1, s0, s1) = ((r + 1) % 3, (r + 2) % 3, (s + 1) % 3, (s + 2) % 3);
        m[r0][s0] * m[r1][s1] - m[r0][s1] * m[r1][s0]
    };
    let det = (0..3).map(|j| m[0][j] * c(0, j)).sum::<f64>();
    if !(det.abs() > 1e-12) {
        return Err(Error::InvalidInput(format!("singular color matrix {m:?}")));
    }
    // Inverse = adjugate / det; adjugate is the transposed cofactor matrix.
    Ok(std::array::from_fn(|i| std::array::from_fn(|j| c(j, i) / det)))
}

fn lerp_mat(a: &Mat3, b: &Mat3, wa: f64) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| wa * a[i][j] + (1.0 - wa) * b[i][j]))
}

pub fn xy_to_xyz(xy: [f64; 2]) -> [f64; 3] {
    [xy[0] / xy[1], 1.0, (1.0 - xy[0] - xy[1]) / xy[1]]
}

pub fn xyz_to_xy(v: [f64; 3]) -> [f64; 2] {
    let s = v[0] + v[1] + v[2];
    if s > 0.0 { [v[0] / s, v[1] / s] } else { D50_XY }
}

/// Correlated color temperature from xy. C. S. McCamy, "Correlated color temperature
/// as an explicit function of chromaticity coordinates", Color Res. Appl. 17 (1992).
/// Error < 2 K over 2856..6504 K, which is far below what the matrix weighting resolves.
pub fn cct_mccamy(xy: [f64; 2]) -> f64 {
    let n = (xy[0] - 0.3320) / (0.1858 - xy[1]);
    449.0 * n.powi(3) + 3525.0 * n.powi(2) + 6823.3 * n + 5520.33
}

/// Temperature of an EXIF LightSource code, same table as Adobe DNG SDK
/// `dng_camera_profile::IlluminantToTemperature`. Unknown codes return `None`.
pub fn illuminant_temperature(code: u16) -> Option<f64> {
    Some(match code {
        17 | 3 => 2850.0,              // Standard light A, Tungsten
        24 => 3200.0,                  // ISO studio tungsten
        23 => 5000.0,                  // D50
        1 | 4 | 9 | 18 | 20 => 5500.0, // Daylight, Flash, Fine weather, Std B, D55
        10 | 19 | 21 => 6500.0,        // Cloudy, Std C, D65
        11 | 22 => 7500.0,             // Shade, D75
        12 => 6400.0,                  // Daylight fluorescent (5700..7100)
        13 => 5050.0,                  // Day white fluorescent (4600..5500)
        2 | 14 => 4150.0,              // Fluorescent, Cool white fluorescent (3800..4500)
        15 => 3525.0,                  // White fluorescent (3250..3800)
        16 => 2925.0,                  // Warm white fluorescent (2600..3250)
        _ => return None,
    })
}

/// Bradford chromatic adaptation from white `src` to white `dst` (XYZ).
/// K. M. Lam (1985); matrix as in Lindbloom, "Chromatic Adaptation".
pub fn bradford(src: [f64; 3], dst: [f64; 3]) -> Mat3 {
    const MB: Mat3 = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let (s, d) = (mul_vec(&MB, src), mul_vec(&MB, dst));
    let scale = diag([d[0] / s[0], d[1] / s[1], d[2] / s[2]]);
    mul(&inverse(&MB).expect("Bradford matrix is invertible"), &mul(&scale, &MB))
}

/// Linear sRGB -> XYZ(D65), derived from the IEC 61966-2-1 primaries and white so that
/// RGB (1,1,1) maps exactly to the D65 white (the 4-digit matrix in the standard does not).
pub fn srgb_to_xyz_d65() -> Mat3 {
    let prim = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]].map(xy_to_xyz);
    let p: Mat3 = std::array::from_fn(|i| std::array::from_fn(|j| prim[j][i]));
    let s = mul_vec(&inverse(&p).expect("sRGB primaries are independent"), xy_to_xyz(D65_XY));
    mul(&p, &diag(s))
}

/// XYZ(D50) -> linear sRGB: Bradford D50 -> D65, then inverse sRGB matrix.
pub fn xyz_d50_to_srgb() -> Mat3 {
    let to_d65 = bradford(xy_to_xyz(D50_XY), xy_to_xyz(D65_XY));
    mul(&inverse(&srgb_to_xyz_d65()).expect("invertible"), &to_d65)
}

/// Weight of calibration 1 for scene temperature `t`, interpolated in inverse temperature
/// (DNG spec ch. 6, "interpolated ... based on the inverse of the color temperature").
fn weight_cal1(t: f64, t1: f64, t2: f64) -> f64 {
    if (t1 - t2).abs() < 1.0 {
        return 1.0;
    }
    ((1.0 / t - 1.0 / t2) / (1.0 / t1 - 1.0 / t2)).clamp(0.0, 1.0)
}

/// Interpolated calibration for the scene white, plus the scene white chromaticity.
struct Interp {
    color_matrix: Mat3,
    forward_matrix: Option<Mat3>,
    camera_calibration: Mat3,
    white_xy: [f64; 2],
}

fn interpolate(p: &ColorProfile, neutral: [f64; 3]) -> Result<Interp, Error> {
    let ab = diag(p.analog_balance);
    let Some(c2) = &p.cal2 else {
        let c1 = &p.cal1;
        let xyz_to_cam = mul(&ab, &mul(&c1.camera_calibration, &c1.color_matrix));
        let white = mul_vec(&inverse(&xyz_to_cam)?, neutral);
        return Ok(Interp {
            color_matrix: c1.color_matrix,
            forward_matrix: c1.forward_matrix,
            camera_calibration: c1.camera_calibration,
            white_xy: xyz_to_xy(white),
        });
    };
    let c1 = &p.cal1;
    let temp = |c: &Calibration| {
        illuminant_temperature(c.illuminant)
            .ok_or_else(|| Error::Unsupported(format!("calibration illuminant {}", c.illuminant)))
    };
    let (t1, t2) = (temp(c1)?, temp(c2)?);
    let at = |w: f64| -> Result<(Mat3, Mat3, [f64; 2]), Error> {
        let cm = lerp_mat(&c1.color_matrix, &c2.color_matrix, w);
        let cc = lerp_mat(&c1.camera_calibration, &c2.camera_calibration, w);
        let xyz_to_cam = mul(&ab, &mul(&cc, &cm));
        Ok((cm, cc, xyz_to_xy(mul_vec(&inverse(&xyz_to_cam)?, neutral))))
    };
    // Self-consistency: the weight w must reproduce itself through
    // w -> matrices -> scene white xy -> temperature -> w (DNG SDK `NeutralToXY` iterates
    // this directly, which can oscillate). g(w) maps [0,1] into [0,1], so h(w) = g(w) - w
    // has h(0) >= 0 >= h(1) and bisection always brackets a fixed point.
    let g = |w: f64| -> Result<f64, Error> { Ok(weight_cal1(cct_mccamy(at(w)?.2), t1, t2)) };
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if g(mid)? > mid { lo = mid } else { hi = mid }
    }
    let w = 0.5 * (lo + hi);
    let (cm, cc, xy) = at(w)?;
    let fm = match (c1.forward_matrix, c2.forward_matrix) {
        (Some(a), Some(b)) => Some(lerp_mat(&a, &b, w)),
        (a, b) => a.or(b),
    };
    Ok(Interp { color_matrix: cm, forward_matrix: fm, camera_calibration: cc, white_xy: xy })
}

/// Scales the rows of a forward matrix so that camera (1,1,1) maps exactly to the D50
/// white, as DNG SDK `NormalizeForwardMatrix` does; Camera2 matrices are stored as
/// rationals and are not exactly normalized.
fn normalize_forward(fm: &Mat3) -> Mat3 {
    let got = mul_vec(fm, [1.0, 1.0, 1.0]);
    let want = xy_to_xyz(D50_XY);
    std::array::from_fn(|i| fm[i].map(|v| v * want[i] / got[i]))
}

/// White-balanced camera RGB -> XYZ(D50). `neutral` is the camera neutral with max 1.
pub fn camera_to_xyz_d50(p: &ColorProfile, neutral: [f64; 3]) -> Result<Mat3, Error> {
    let it = interpolate(p, neutral)?;
    let abcc_inv = inverse(&mul(&diag(p.analog_balance), &it.camera_calibration))?;
    let cam_to_xyz = match it.forward_matrix {
        // CameraToXYZ_D50 = FM * D * Inverse(AB * CC),
        // D = Invert(AsDiagonalMatrix(Inverse(AB * CC) * CameraNeutral)).
        Some(fm) => {
            let rn = mul_vec(&abcc_inv, neutral);
            mul(&normalize_forward(&fm), &mul(&diag(rn.map(|v| 1.0 / v)), &abcc_inv))
        }
        // CameraToXYZ_D50 = CA * Inverse(AB * CC * CM), CA adapts the scene white to D50.
        None => {
            let xyz_to_cam = mul(&diag(p.analog_balance), &mul(&it.camera_calibration, &it.color_matrix));
            let ca = bradford(xy_to_xyz(it.white_xy), xy_to_xyz(D50_XY));
            mul(&ca, &inverse(&xyz_to_cam)?)
        }
    };
    let m = mul(&cam_to_xyz, &diag(neutral));
    // Normalize so white-balanced (1,1,1), i.e. clipped highlights, lands on Y = 1.
    let y = mul_vec(&m, [1.0, 1.0, 1.0])[1];
    Ok(m.map(|r| r.map(|v| v / y)))
}

/// Full per-pixel matrix: white-balanced camera RGB -> linear sRGB.
pub fn camera_to_srgb(p: &ColorProfile, neutral: [f64; 3]) -> Result<[[f32; 3]; 3], Error> {
    let m = mul(&xyz_d50_to_srgb(), &camera_to_xyz_d50(p, neutral)?);
    Ok(m.map(|r| r.map(|v| v as f32)))
}

pub fn apply_matrix(img: &RgbImage, m: &[[f32; 3]; 3]) -> RgbImage {
    let data = img
        .data
        .iter()
        .map(|p| std::array::from_fn(|i| m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2]))
        .collect();
    RgbImage { width: img.width, height: img.height, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Mat3, b: &Mat3, tol: f64) -> bool {
        a.iter().flatten().zip(b.iter().flatten()).all(|(x, y)| (x - y).abs() < tol)
    }

    #[test]
    fn xyz_d50_to_srgb_matches_lindbloom() {
        // Lindbloom, "RGB/XYZ Matrices", sRGB with Bradford-adapted D50 reference white.
        let lind: Mat3 = [
            [3.1338561, -1.6168667, -0.4906146],
            [-0.9787684, 1.9161415, 0.0334540],
            [0.0719453, -0.2289914, 1.4052427],
        ];
        assert!(close(&xyz_d50_to_srgb(), &lind, 2e-3));
        let white = mul_vec(&xyz_d50_to_srgb(), xy_to_xyz(D50_XY));
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-9), "{white:?}");
    }

    #[test]
    fn inverse_roundtrip() {
        let m: Mat3 = [[2.0, 1.0, 0.5], [0.1, 3.0, -1.0], [0.0, 0.4, 1.5]];
        assert!(close(&mul(&m, &inverse(&m).unwrap()), &IDENTITY, 1e-12));
        assert!(inverse(&[[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]]).is_err());
    }

    #[test]
    fn mccamy_reference_points() {
        assert!((cct_mccamy(D65_XY) - 6504.0).abs() < 5.0);
        assert!((cct_mccamy([0.44757, 0.40745]) - 2856.0).abs() < 5.0);
    }

    fn profile(fm: bool) -> ColorProfile {
        // Synthetic camera: XYZ -> camera is a mild channel mix.
        let cm1: Mat3 = [[0.9, 0.2, -0.1], [-0.3, 1.2, 0.1], [0.0, -0.2, 1.1]];
        let cm2: Mat3 = [[0.8, 0.25, -0.05], [-0.35, 1.25, 0.1], [0.05, -0.15, 1.0]];
        let fm1 = mul(&inverse(&cm1).unwrap(), &IDENTITY);
        ColorProfile {
            cal1: Calibration { illuminant: 17, color_matrix: cm1, forward_matrix: fm.then_some(fm1), camera_calibration: IDENTITY },
            cal2: Some(Calibration { illuminant: 21, color_matrix: cm2, forward_matrix: fm.then_some(fm1), camera_calibration: IDENTITY }),
            analog_balance: [1.0; 3],
        }
    }

    #[test]
    fn neutral_maps_to_white() {
        for fm in [true, false] {
            let m = camera_to_srgb(&profile(fm), [0.55, 1.0, 0.7]).unwrap();
            let w: Vec<f32> = m.iter().map(|r| r.iter().sum()).collect();
            assert!(w.iter().all(|v| (v - 1.0).abs() < 1e-5), "fm={fm}: {w:?}");
        }
    }

    #[test]
    fn interpolation_recovers_scene_white() {
        // Neutral of a D65 scene under cal2 alone must be recognized as ~6500 K.
        let p = profile(false);
        let c2 = p.cal2.as_ref().unwrap();
        let mut n = mul_vec(&c2.color_matrix, xy_to_xyz(D65_XY));
        let mx = n.iter().copied().fold(0.0, f64::max);
        n = n.map(|v| v / mx);
        let it = interpolate(&p, n).unwrap();
        let t = cct_mccamy(it.white_xy);
        assert!((t - 6504.0).abs() < 10.0, "{t}");
    }

    #[test]
    fn interpolation_is_self_consistent_mid_range() {
        // Matrices from a real Mi 8 DNG (D55 / A) where plain fixed-point iteration oscillates.
        let cm1: Mat3 = [[0.6584, -0.3131, -0.1044], [-0.9956, 1.9269, 0.0401], [0.0562, -0.2168, 1.132]];
        let cm2: Mat3 = [[1.397, -0.6584, -0.2168], [-0.9956, 1.9269, 0.0401], [0.0401, -0.1525, 0.7868]];
        let cal = |illuminant, color_matrix| Calibration { illuminant, color_matrix, forward_matrix: None, camera_calibration: IDENTITY };
        let p = ColorProfile { cal1: cal(20, cm1), cal2: Some(cal(17, cm2)), analog_balance: [1.0; 3] };
        let it = interpolate(&p, [0.451847, 1.0, 0.715051]).unwrap();
        let w = weight_cal1(cct_mccamy(it.white_xy), 5500.0, 2850.0);
        let expect = lerp_mat(&cm1, &cm2, w);
        assert!(close(&it.color_matrix, &expect, 1e-6), "w = {w}");
    }
}
