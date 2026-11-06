//! JNI bridge for `dev.vegameta.rawzero.Native`. All `unsafe` of the project lives here.
//!
//! Metadata crosses JNI as flat primitive arrays with the fixed layout below (mirrored in
//! `Native.kt`); `build_meta` turns them into `FrameMeta` and is plain Rust, tested on desktop.

use jni::objects::{JByteBuffer, JClass, JDoubleArray, JFloatArray, JIntArray, JString};
use jni::sys::jint;
use jni::{Env, EnvUnowned};
use rawzero_core::color::Mat3;
use rawzero_core::{Calibration, CfaPattern, ColorProfile, FrameMeta, LensShadingMap, Rect};

/// `ints` layout.
pub mod ints {
    pub const CFA: usize = 0; // SENSOR_INFO_COLOR_FILTER_ARRANGEMENT
    pub const ORIENTATION: usize = 1; // EXIF 1..8
    pub const HOT_PIXELS_CORRECTED: usize = 2; // 0/1, from HOT_PIXEL_MODE
    pub const ILLUMINANT1: usize = 3; // SENSOR_REFERENCE_ILLUMINANT1
    pub const ILLUMINANT2: usize = 4; // SENSOR_REFERENCE_ILLUMINANT2, 0 = absent
    pub const HAL_SHADING_APPLIED: usize = 5; // SENSOR_INFO_LENS_SHADING_APPLIED 0/1
    pub const SHADING_ROWS: usize = 6;
    pub const SHADING_COLS: usize = 7;
    pub const JPEG_QUALITY: usize = 8;
    pub const CROP_X: usize = 9; // crop in RAW pixels, width 0 = no crop
    pub const CROP_Y: usize = 10;
    pub const CROP_W: usize = 11;
    pub const CROP_H: usize = 12;
    pub const MIRROR: usize = 13; // 0 none, 1 reverse columns, 2 reverse rows (stored pixels)
    pub const MODE: usize = 14; // 0 Process Zero, 1 Classic (noise reduction + tone match)
    pub const LEN: usize = 15;
}

/// `floats` layout: black level [0..4] (2x2 CFA, row-major), white level [4],
/// COLOR_CORRECTION_GAINS [5..9] as [R, G_even, G_odd, B].
pub const FLOATS_LEN: usize = 9;

/// `doubles` layout: six row-major 3x3 matrices, in order COLOR_TRANSFORM1/2,
/// FORWARD_MATRIX1/2, CALIBRATION_TRANSFORM1/2. A matrix whose first element is NaN is absent.
pub const DOUBLES_LEN: usize = 54;

#[derive(Debug)]
pub enum BridgeError {
    Jni(jni::errors::Error),
    Core(rawzero_core::Error),
    Input(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Jni(e) => write!(f, "jni: {e}"),
            BridgeError::Core(e) => write!(f, "{e}"),
            BridgeError::Input(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<jni::errors::Error> for BridgeError {
    fn from(e: jni::errors::Error) -> Self {
        BridgeError::Jni(e)
    }
}

impl From<rawzero_core::Error> for BridgeError {
    fn from(e: rawzero_core::Error) -> Self {
        BridgeError::Core(e)
    }
}

fn mat(d: &[f64], i: usize) -> Option<Mat3> {
    let m = &d[9 * i..9 * i + 9];
    (!m[0].is_nan()).then(|| std::array::from_fn(|r| std::array::from_fn(|c| m[3 * r + c])))
}

/// Flat arrays -> `FrameMeta`. `shading` is `rows * cols * 4` gains `[R, G_even, G_odd, B]`.
pub fn build_meta(i: &[i32], f: &[f32], d: &[f64], shading: Option<&[f32]>, make: &str) -> Result<FrameMeta, BridgeError> {
    if i.len() < ints::LEN || f.len() < FLOATS_LEN || d.len() < DOUBLES_LEN {
        return Err(BridgeError::Input(format!("array lengths {} / {} / {}", i.len(), f.len(), d.len())));
    }
    let cfa = CfaPattern::from_camera2(i[ints::CFA])?;
    let color = color_profile(i, d)?;
    let hal_applied = i[ints::HAL_SHADING_APPLIED] != 0;
    let lens_shading = match shading {
        Some(s) if rawzero_core::meta::needs_lens_shading(make, hal_applied) => {
            let (rows, cols) = (i[ints::SHADING_ROWS] as usize, i[ints::SHADING_COLS] as usize);
            if s.len() != rows * cols * 4 {
                return Err(BridgeError::Input(format!("shading map {cols}x{rows} with {} values", s.len())));
            }
            Some(LensShadingMap { rows, cols, gains: s.chunks_exact(4).map(|g| [g[0], g[1], g[2], g[3]]).collect() })
        }
        _ => None,
    };
    let crop = (i[ints::CROP_W] > 0 && i[ints::CROP_H] > 0).then(|| Rect {
        x: i[ints::CROP_X] as usize,
        y: i[ints::CROP_Y] as usize,
        width: i[ints::CROP_W] as usize,
        height: i[ints::CROP_H] as usize,
    });
    let meta = FrameMeta {
        cfa,
        black_level: [f[0], f[1], f[2], f[3]],
        white_level: f[4],
        lens_shading,
        hot_pixels_corrected: i[ints::HOT_PIXELS_CORRECTED] != 0,
        wb_gains: [f[5], f[6], f[7], f[8]],
        color,
        orientation: i[ints::ORIENTATION] as u16,
        crop,
    };
    meta.validate()?;
    Ok(meta)
}

/// Color calibration part of the arrays (illuminants from `ints`, matrices from `doubles`).
pub fn color_profile(i: &[i32], d: &[f64]) -> Result<ColorProfile, BridgeError> {
    if i.len() < ints::LEN || d.len() < DOUBLES_LEN {
        return Err(BridgeError::Input(format!("array lengths {} / {}", i.len(), d.len())));
    }
    let cm1 = mat(d, 0).ok_or_else(|| BridgeError::Input("SENSOR_COLOR_TRANSFORM1 missing".into()))?;
    let ident = rawzero_core::color::IDENTITY;
    let cal1 = Calibration {
        illuminant: i[ints::ILLUMINANT1] as u16,
        color_matrix: cm1,
        forward_matrix: mat(d, 2),
        camera_calibration: mat(d, 4).unwrap_or(ident),
    };
    let cal2 = match (i[ints::ILLUMINANT2], mat(d, 1)) {
        (il, Some(cm)) if il > 0 => Some(Calibration {
            illuminant: il as u16,
            color_matrix: cm,
            forward_matrix: mat(d, 3),
            camera_calibration: mat(d, 5).unwrap_or(ident),
        }),
        _ => None,
    };
    Ok(ColorProfile { cal1, cal2, analog_balance: [1.0; 3] })
}

/// COLOR_CORRECTION_GAINS `[R, G_even, G_odd, B]` (green = 1) for a Planckian white at `kelvin`.
pub fn wb_gains(i: &[i32], d: &[f64], kelvin: f64) -> Result<[f32; 4], BridgeError> {
    let n = rawzero_core::color::neutral_for_temperature(&color_profile(i, d)?, kelvin)?;
    let g = [n[1] / n[0], 1.0, 1.0, n[1] / n[2]];
    Ok(g.map(|v| v as f32))
}

fn read_ints(env: &Env, a: &JIntArray) -> Result<Vec<i32>, BridgeError> {
    let mut v = vec![0; a.len(env)?];
    a.get_region(env, 0, &mut v)?;
    Ok(v)
}

fn read_floats(env: &Env, a: &JFloatArray) -> Result<Vec<f32>, BridgeError> {
    let mut v = vec![0.0; a.len(env)?];
    a.get_region(env, 0, &mut v)?;
    Ok(v)
}

fn read_doubles(env: &Env, a: &JDoubleArray) -> Result<Vec<f64>, BridgeError> {
    let mut v = vec![0.0; a.len(env)?];
    a.get_region(env, 0, &mut v)?;
    Ok(v)
}

/// `Native.processRaw16(raw, width, height, rowStride, ints, floats, doubles, shading, make,
/// reference, refWidth, refHeight): ByteArray`
/// RAW_SENSOR plane -> pipeline -> JPEG bytes. `reference` (RGB8, may be null) selects the
/// Classic look matched to the ISP output; null gives Process Zero. Throws RuntimeException.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_vegameta_rawzero_Native_processRaw16<'caller>(
    mut unowned: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    raw: JByteBuffer<'caller>,
    width: jint,
    height: jint,
    row_stride: jint,
    ints_arr: JIntArray<'caller>,
    floats_arr: JFloatArray<'caller>,
    doubles_arr: JDoubleArray<'caller>,
    shading_arr: JFloatArray<'caller>,
    make: JString<'caller>,
    reference_arr: jni::objects::JByteArray<'caller>,
    ref_width: jint,
    ref_height: jint,
) -> jni::objects::JByteArray<'caller> {
    unowned
        .with_env(|env| -> Result<_, BridgeError> {
            let (w, h, stride) = (width as usize, height as usize, row_stride as usize);
            let addr = env.get_direct_buffer_address(&raw)?;
            let cap = env.get_direct_buffer_capacity(&raw)?;
            if addr.is_null() || cap < (h.saturating_sub(1)) * stride + w * 2 {
                return Err(BridgeError::Input(format!("raw buffer {cap} bytes for {w}x{h} stride {stride}")));
            }
            // SAFETY: the direct buffer belongs to the android.media.Image the caller keeps open
            // for the duration of this call; `cap` bytes are readable.
            let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, cap) };
            let shading = if shading_arr.is_null() { None } else { Some(read_floats(env, &shading_arr)?) };
            let meta = build_meta(
                &read_ints(env, &ints_arr)?,
                &read_floats(env, &floats_arr)?,
                &read_doubles(env, &doubles_arr)?,
                shading.as_deref(),
                &make.try_to_string(env)?,
            )?;
            let quality = read_ints(env, &ints_arr)?[ints::JPEG_QUALITY].clamp(1, 100) as u8;
            let raw = rawzero_core::unpack::unpack_raw16(bytes, w, h, stride)?;
            let reference = if reference_arr.is_null() {
                None
            } else {
                let mut v = vec![0i8; reference_arr.len(env)?];
                reference_arr.get_region(env, 0, &mut v)?;
                let (rw, rh) = (ref_width as usize, ref_height as usize);
                if v.len() != rw * rh * 3 {
                    return Err(BridgeError::Input(format!("reference {} bytes for {rw}x{rh}", v.len())));
                }
                Some(rawzero_core::Rgb8Image { width: rw, height: rh, data: v.into_iter().map(|b| b as u8).collect() })
            };
            let look = if read_ints(env, &ints_arr)?[ints::MODE] == 1 {
                rawzero_core::Look::Classic(reference.as_ref())
            } else {
                rawzero_core::Look::Fixed
            };
            let (img, _t) = rawzero_core::process_look(&raw, &meta, look)?;
            let img = match read_ints(env, &ints_arr)?[ints::MIRROR] {
                1 => rawzero_core::image::mirror(&img, rawzero_core::image::Mirror::Columns),
                2 => rawzero_core::image::mirror(&img, rawzero_core::image::Mirror::Rows),
                _ => img,
            };
            let jpeg = rawzero_core::encode::encode_jpeg(&img, quality, meta.orientation)?;
            Ok(env.byte_array_from_slice(&jpeg)?)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// `Native.wbGains(ints, doubles, kelvin): FloatArray` -> `[R, G_even, G_odd, B]`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_vegameta_rawzero_Native_wbGains<'caller>(
    mut unowned: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ints_arr: JIntArray<'caller>,
    doubles_arr: JDoubleArray<'caller>,
    kelvin: jni::sys::jdouble,
) -> JFloatArray<'caller> {
    unowned
        .with_env(|env| -> Result<_, BridgeError> {
            let g = wb_gains(&read_ints(env, &ints_arr)?, &read_doubles(env, &doubles_arr)?, kelvin)?;
            let out = JFloatArray::new(env, 4)?;
            out.set_region(env, 0, &g)?;
            Ok(out)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrays() -> (Vec<i32>, Vec<f32>, Vec<f64>) {
        let mut i = vec![0; ints::LEN];
        i[ints::CFA] = 0;
        i[ints::ORIENTATION] = 6;
        i[ints::ILLUMINANT1] = 17;
        i[ints::ILLUMINANT2] = 21;
        i[ints::HAL_SHADING_APPLIED] = 1;
        i[ints::SHADING_ROWS] = 2;
        i[ints::SHADING_COLS] = 3;
        i[ints::JPEG_QUALITY] = 92;
        let f = vec![64.0, 64.0, 64.0, 64.0, 1023.0, 2.0, 1.0, 1.0, 1.6];
        let mut d = vec![f64::NAN; DOUBLES_LEN];
        for m in [0, 1, 2, 3] {
            for k in 0..9 {
                d[9 * m + k] = if k % 4 == 0 { 1.0 } else { 0.0 };
            }
        }
        (i, f, d)
    }

    #[test]
    fn meta_from_arrays() {
        let (i, f, d) = arrays();
        let shading = vec![1.5; 2 * 3 * 4];
        let m = build_meta(&i, &f, &d, Some(&shading), "samsung").unwrap();
        assert_eq!(m.cfa, CfaPattern::Rggb);
        assert_eq!(m.orientation, 6);
        assert!(m.lens_shading.is_none(), "Samsung HAL applies shading itself");
        assert!(m.color.cal2.is_some() && m.color.cal1.forward_matrix.is_some());
        assert_eq!(m.color.cal1.camera_calibration, rawzero_core::color::IDENTITY);
        let m = build_meta(&i, &f, &d, Some(&shading), "Xiaomi").unwrap();
        assert_eq!(m.lens_shading.unwrap().gains.len(), 6);
    }

    #[test]
    fn wb_gains_warm_light_boosts_blue() {
        let (i, _, d) = arrays();
        let warm = wb_gains(&i, &d, 2800.0).unwrap();
        let cool = wb_gains(&i, &d, 7000.0).unwrap();
        assert_eq!(warm[1], 1.0);
        assert!(warm[3] > cool[3] && warm[0] < cool[0], "{warm:?} {cool:?}");
    }

    #[test]
    fn rejects_bad_shading_and_short_arrays() {
        let (i, f, d) = arrays();
        assert!(build_meta(&i, &f, &d, Some(&[1.0; 5]), "Xiaomi").is_err());
        assert!(build_meta(&i[..3], &f, &d, None, "Xiaomi").is_err());
    }
}
