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
  --info             print metadata and exit";

struct Args {
    input: PathBuf,
    output: Option<PathBuf>,
    quality: u8,
    fix_hot: bool,
    no_shading: bool,
    info: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args { input: PathBuf::new(), output: None, quality: DEFAULT_QUALITY, fix_hot: false, no_shading: false, info: false };
    let mut input = None;
    while let Some(s) = it.next() {
        match s.as_str() {
            "-o" => a.output = Some(it.next().ok_or("-o needs a path")?.into()),
            "-q" => a.quality = it.next().and_then(|v| v.parse().ok()).ok_or("-q needs 1..100")?,
            "--fix-hot-pixels" => a.fix_hot = true,
            "--no-shading" => a.no_shading = true,
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
    if args.no_shading {
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

    let (img, timings) = rawzero_core::process(&d.raw, &d.meta).map_err(|e| e.to_string())?;
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

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
