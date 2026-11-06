//! rawzero desktop CLI: DNG -> JPEG through rawzero-core.

mod dng;
mod ljpeg;

use rawzero_core::encode::{encode_jpeg, DEFAULT_QUALITY};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: rawzero <input.dng> [-o out.jpg] [-q quality] [--fix-hot-pixels] [--no-shading] [--info]

  -o PATH            output JPEG (default: input with .jpg extension)
  -q N               JPEG quality 1..100 (default 92)
  --fix-hot-pixels   run hot pixel suppression (DNG does not say whether the HAL did it)
  --no-shading       skip lens shading even if the DNG carries a GainMap
  --classic          Classic mode: raw noise reduction (+ tone match with --ref)
  --ref IMG          Classic look reference: ISP JPEG / preview PNG of the same shot (implies --classic)
  --ref-rot DEG      reference = our image rotated clockwise by DEG (preview frames: sensor orientation)
  --ref-mirror       ... and mirrored left-right (front-camera preview frames)
  --ref-scan         print the reference registration score for every orientation and exit
  --info             print metadata and exit";

struct Args {
    input: PathBuf,
    output: Option<PathBuf>,
    quality: u8,
    fix_hot: bool,
    no_shading: bool,
    info: bool,
    reference: Option<PathBuf>,
    classic: bool,
    ref_rot: u16,
    ref_mirror: bool,
    ref_scan: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args { input: PathBuf::new(), output: None, quality: DEFAULT_QUALITY, fix_hot: false, no_shading: false, info: false, reference: None, classic: false, ref_rot: 0, ref_mirror: false, ref_scan: false };
    let mut input = None;
    while let Some(s) = it.next() {
        match s.as_str() {
            "-o" => a.output = Some(it.next().ok_or("-o needs a path")?.into()),
            "-q" => a.quality = it.next().and_then(|v| v.parse().ok()).ok_or("-q needs 1..100")?,
            "--fix-hot-pixels" => a.fix_hot = true,
            "--no-shading" => a.no_shading = true,
            "--ref" => a.reference = Some(it.next().ok_or("--ref needs a path")?.into()),
            "--classic" => a.classic = true,
            "--ref-rot" => a.ref_rot = it.next().and_then(|v| v.parse().ok()).ok_or("--ref-rot needs degrees")?,
            "--ref-mirror" => a.ref_mirror = true,
            "--ref-scan" => a.ref_scan = true,
            "--info" => a.info = true,
            "-h" | "--help" => return Err(USAGE.into()),
            _ if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            _ if input.is_none() => input = Some(PathBuf::from(s)),
            _ => return Err(format!("extra argument {s}\n{USAGE}")),
        }
    }
    a.input = input.ok_or(USAGE)?;
    Ok(a)
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let t0 = Instant::now();
    let bytes = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?;
    let mut d = dng::read_dng(&bytes)?;
    let t_read = t0.elapsed();

    let m = &mut d.meta;
    m.hot_pixels_corrected = !args.fix_hot;
    // DNG does not carry SENSOR_INFO_LENS_SHADING_APPLIED. Camera2 DNGs of the measured
    // devices: Samsung reports true (and is right), Xiaomi reports true (and is wrong);
    // other files keep their GainMap.
    let hal_applied = d.make.eq_ignore_ascii_case("samsung");
    if args.no_shading || !rawzero_core::meta::needs_lens_shading(&d.make, hal_applied) {
        m.lens_shading = None;
    }
    eprintln!("{} {} | {}x{} {}-bit {:?}", d.make, d.model, d.raw.width, d.raw.height, d.bits, m.cfa);
    eprintln!("black {:?} white {} | WB gains {:?}", m.black_level, m.white_level, m.wb_gains);
    eprintln!(
        "illuminants {} / {} | forward matrix: {} | lens shading: {} | crop {:?} | orientation {}",
        m.color.cal1.illuminant,
        m.color.cal2.as_ref().map_or("-".into(), |c| c.illuminant.to_string()),
        m.color.cal1.forward_matrix.is_some(),
        m.lens_shading.as_ref().map_or("none".into(), |s| format!("{}x{}", s.cols, s.rows)),
        m.crop,
        m.orientation
    );
    let neutral = rawzero_core::wb::camera_neutral(m);
    let srgb = rawzero_core::color::camera_to_srgb(&m.color, neutral).map_err(|e| e.to_string())?;
    eprintln!("camera->sRGB {srgb:.4?}");
    for w in &d.warnings {
        eprintln!("warning: {w}");
    }
    if args.info {
        return Ok(());
    }

    let reference = args.reference.as_deref().map(read_jpeg).transpose()?;
    if args.ref_scan {
        let r = reference.as_ref().ok_or("--ref-scan needs --ref")?;
        let lin = rawzero_core::render_linear(&d.raw, &d.meta).map_err(|e| e.to_string())?;
        for (g, score) in rawzero_core::look::registration_scan(&lin, r) {
            eprintln!("rotate_cw {:3} mirror {:5}: luminance correlation {score:.3}", g.rotate_cw, g.mirror);
        }
        return Ok(());
    }
    // The ISP still has the RAW's orientation: identity geometry.
    let look = if args.classic || reference.is_some() {
        let geometry = rawzero_core::look::RefGeometry { rotate_cw: args.ref_rot, mirror: args.ref_mirror };
        rawzero_core::Look::Classic(reference.as_ref().map(|image| rawzero_core::look::Reference { image, geometry }))
    } else {
        rawzero_core::Look::Fixed
    };
    let (img, timings) = rawzero_core::process_look(&d.raw, &d.meta, look).map_err(|e| e.to_string())?;
    let t1 = Instant::now();
    let jpeg = encode_jpeg(&img, args.quality, d.meta.orientation).map_err(|e| e.to_string())?;
    let t_enc = t1.elapsed();
    let out = args.output.unwrap_or_else(|| args.input.with_extension("jpg"));
    std::fs::write(&out, &jpeg).map_err(|e| format!("{}: {e}", out.display()))?;

    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let mut line = format!("read {:.0}", ms(t_read));
    for (name, dt) in &timings {
        line += &format!(" | {name} {:.0}", ms(*dt));
    }
    let total: f64 = timings.iter().map(|(_, d)| ms(*d)).sum::<f64>() + ms(t_enc);
    eprintln!("{line} | jpeg {:.0} | pipeline {total:.0} ms", ms(t_enc));
    eprintln!("wrote {} ({}x{}, {} KB)", out.display(), img.width, img.height, jpeg.len() / 1024);
    Ok(())
}

/// Decodes a baseline/progressive JPEG to 8-bit RGB (zune-jpeg; desktop only).
fn read_jpeg(path: &std::path::Path) -> Result<rawzero_core::Rgb8Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
    let data = dec.decode().map_err(|e| format!("{}: {e:?}", path.display()))?;
    let (width, height) = dec.dimensions().ok_or("no JPEG dimensions")?;
    if data.len() != width * height * 3 {
        return Err(format!("{}: expected RGB output, got {} bytes for {width}x{height}", path.display(), data.len()));
    }
    Ok(rawzero_core::Rgb8Image { width, height, data })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
