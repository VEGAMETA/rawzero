//! rawzero-core: one RAW frame -> one display-referred image, deterministic.
//!
//! Steps (each a pure function in its own module):
//! 1 unpack, 2 levels, 3 lens shading, 4 hot pixels, 5 white balance, 6 demosaic,
//! 7 color, 8 tone, 9 encode. No denoise, no sharpening, no local operators.

pub mod burst;
pub mod color;
pub mod demosaic;
pub mod denoise;
pub mod encode;
pub mod hotpixel;
pub mod image;
pub mod levels;
pub mod look;
pub mod meta;
pub mod shading;
pub mod tone;
pub mod unpack;
pub mod wb;

#[cfg(test)]
mod testutil;

use std::time::{Duration, Instant};

pub use image::{Mosaic, RawImage, Rect, Rgb8Image, RgbImage};
pub use meta::{Calibration, CfaPattern, ColorProfile, FrameMeta, LensShadingMap};

#[derive(Debug)]
pub enum Error {
    InvalidInput(String),
    Unsupported(String),
    Encode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidInput(s) => write!(f, "invalid input: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Encode(s) => write!(f, "encode: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// Wall time of each step, in pipeline order.
pub type Timings = Vec<(&'static str, Duration)>;

/// How step 8 turns linear RGB into display values.
#[derive(Clone, Copy, Debug)]
pub enum Look<'a> {
    /// Process Zero: no noise reduction, fixed global S-curve, then sRGB OETF.
    Fixed,
    /// Classic: raw-domain noise reduction (denoise.rs) and a global tone curve fitted to
    /// the ISP output of the same request (look.rs); fixed curve when no reference exists.
    Classic(Option<look::Reference<'a>>),
}

/// Linear sRGB render (steps 2..7, Process Zero, no crop): diagnostics such as the look
/// reference orientation scan.
pub fn render_linear(raw: &RawImage, meta: &FrameMeta) -> Result<RgbImage, Error> {
    meta.validate()?;
    let mut m = levels::apply_levels(raw, meta);
    if let Some(map) = &meta.lens_shading {
        m = shading::apply_lens_shading(&m, map, meta.cfa);
    }
    let m = wb::apply_white_balance(&m, wb::cell_gains(meta));
    let rgb = demosaic::demosaic_mhc(&m, meta.cfa);
    let rgb = match meta.crop {
        Some(r) => image::crop(&rgb, r)?,
        None => rgb,
    };
    Ok(color::apply_matrix(&rgb, &color::camera_to_srgb(&meta.color, wb::camera_neutral(meta))?))
}

/// Steps 2..8 on an unpacked frame (Process Zero). Encoding is separate.
pub fn process(raw: &RawImage, meta: &FrameMeta) -> Result<(Rgb8Image, Timings), Error> {
    process_look(raw, meta, Look::Fixed)
}

/// Steps 2..8 with a selectable tone stage.
pub fn process_look(raw: &RawImage, meta: &FrameMeta, look: Look) -> Result<(Rgb8Image, Timings), Error> {
    meta.validate()?;
    if raw.data.len() != raw.width * raw.height || raw.width < 2 || raw.height < 2 {
        return Err(Error::InvalidInput(format!("raw {}x{}", raw.width, raw.height)));
    }
    let mut t = Timings::new();
    let mut clock = Instant::now();
    let mut lap = |name: &'static str, t: &mut Timings| {
        let now = Instant::now();
        t.push((name, now - clock));
        clock = now;
    };

    let m = levels::apply_levels(raw, meta);
    lap("levels", &mut t);
    let (out, rest) = process_mosaic(m, meta, look)?;
    t.extend(rest);
    Ok((out, t))
}

/// Steps 3..8 on a normalized mosaic (after levels): a single frame or a merged burst.
pub fn process_mosaic(mut m: Mosaic, meta: &FrameMeta, look: Look) -> Result<(Rgb8Image, Timings), Error> {
    meta.validate()?;
    let mut t = Timings::new();
    let mut clock = Instant::now();
    let mut lap = |name: &'static str, t: &mut Timings| {
        let now = Instant::now();
        t.push((name, now - clock));
        clock = now;
    };
    // Noise reduction on stationary-noise data: after levels, before shading gains and WB.
    if matches!(look, Look::Classic(_)) {
        if let Some(nm) = denoise::estimate_noise(&m, meta.cfa) {
            m = denoise::denoise_mosaic(&m, meta.cfa, nm);
        }
        lap("denoise", &mut t);
    }
    if let Some(map) = &meta.lens_shading {
        m = shading::apply_lens_shading(&m, map, meta.cfa);
        lap("lens_shading", &mut t);
    }
    if !meta.hot_pixels_corrected {
        m = hotpixel::fix_hot_pixels(&m);
        lap("hot_pixels", &mut t);
    }
    let m = wb::apply_white_balance(&m, wb::cell_gains(meta));
    lap("white_balance", &mut t);
    let mut rgb = demosaic::demosaic_mhc(&m, meta.cfa);
    drop(m);
    lap("demosaic", &mut t);
    if let Some(r) = meta.crop {
        rgb = image::crop(&rgb, r)?;
        lap("crop", &mut t);
    }
    let cam_to_srgb = color::camera_to_srgb(&meta.color, wb::camera_neutral(meta))?;
    let rgb = color::apply_matrix(&rgb, &cam_to_srgb);
    lap("color", &mut t);
    let out = match look {
        Look::Fixed | Look::Classic(None) => tone::render_8bit(&rgb, &tone::build_lut()),
        Look::Classic(Some(reference)) => {
            let r = reference.image;
            // Hue/WB: full 3x3 chromaticity match to the reference when it registers,
            // else per-channel WB gains (distribution-based, needs no registration).
            let rgb = match look::fit_ccm(&rgb, &reference) {
                Some(m) => {
                    lap("ccm", &mut t);
                    color::apply_matrix(&rgb, &m)
                }
                None => {
                    lap("wb_gains", &mut t);
                    look::apply_gains(&rgb, look::fit_wb(&rgb, r))
                }
            };
            let tm = look::ToneMap::fit(&rgb, r);
            let sat = look::fit_saturation(&rgb, &tm, r);
            lap("look_fit", &mut t);
            look::render_matched_sat(&rgb, &tm, sat)
        }
    };
    lap("tone", &mut t);
    Ok((out, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::meta_rggb;

    #[test]
    fn gray_card_renders_neutral_and_deterministic() {
        // Flat scene: camera neutral at 18% of the range after WB -> sRGB 118 gray.
        let mut meta = meta_rggb();
        meta.wb_gains = [2.0, 1.0, 1.0, 1.6];
        meta.black_level = [64.0; 4];
        meta.white_level = 1023.0;
        let (w, h) = (64, 48);
        let level = |c: usize| -> f64 { 64.0 + 0.18 * 959.0 / [2.0, 1.0, 1.0, 1.6][c] };
        let data = (0..w * h)
            .map(|i| level(meta.cfa.camera2_channel(meta::cell(i % w, i / w))).round() as u16)
            .collect();
        let raw = RawImage::new(w, h, data).unwrap();
        let (a, _) = process(&raw, &meta).unwrap();
        let (b, _) = process(&raw, &meta).unwrap();
        assert_eq!(a, b);
        for px in a.data.chunks(3) {
            for &v in px {
                assert!((v as i32 - 118).abs() <= 1, "{px:?}");
            }
        }
    }
}
