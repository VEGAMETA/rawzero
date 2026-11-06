//! Minimal DNG reader: uncompressed Bayer CFA (what AOSP `DngCreator` writes) -> RawImage + FrameMeta.
//! Tag numbers and semantics: DNG spec 1.6 and TIFF 6.0.

use rawzero_core::color::{Mat3, IDENTITY};
use rawzero_core::{Calibration, CfaPattern, ColorProfile, FrameMeta, LensShadingMap, RawImage, Rect};
use std::collections::{HashMap, HashSet};

pub struct Dng {
    pub raw: RawImage,
    pub meta: FrameMeta,
    pub make: String,
    pub model: String,
    pub bits: u32,
    /// Things that were present in the file but ignored.
    pub warnings: Vec<String>,
}

type Res<T> = Result<T, String>;

mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const WIDTH: u16 = 256;
    pub const HEIGHT: u16 = 257;
    pub const BITS: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const MAKE: u16 = 271;
    pub const MODEL: u16 = 272;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const ORIENTATION: u16 = 274;
    pub const SAMPLES: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTES: u16 = 279;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTES: u16 = 325;
    pub const SUB_IFDS: u16 = 330;
    pub const CFA_REPEAT_DIM: u16 = 33421;
    pub const CFA_PATTERN: u16 = 33422;
    pub const LINEARIZATION: u16 = 50712;
    pub const BLACK_REPEAT_DIM: u16 = 50713;
    pub const BLACK_LEVEL: u16 = 50714;
    pub const BLACK_DELTA_H: u16 = 50715;
    pub const BLACK_DELTA_V: u16 = 50716;
    pub const WHITE_LEVEL: u16 = 50717;
    pub const CROP_ORIGIN: u16 = 50719;
    pub const CROP_SIZE: u16 = 50720;
    pub const COLOR_MATRIX1: u16 = 50721;
    pub const COLOR_MATRIX2: u16 = 50722;
    pub const CALIBRATION1: u16 = 50723;
    pub const CALIBRATION2: u16 = 50724;
    pub const ANALOG_BALANCE: u16 = 50727;
    pub const AS_SHOT_NEUTRAL: u16 = 50728;
    pub const ILLUMINANT1: u16 = 50778;
    pub const ILLUMINANT2: u16 = 50779;
    pub const ACTIVE_AREA: u16 = 50829;
    pub const FORWARD_MATRIX1: u16 = 50964;
    pub const FORWARD_MATRIX2: u16 = 50965;
    pub const OPCODE_LIST1: u16 = 51008;
    pub const OPCODE_LIST2: u16 = 51009;
    pub const OPCODE_LIST3: u16 = 51022;
}

const PHOTOMETRIC_CFA: u32 = 32803;

#[derive(Clone, Copy)]
struct Entry {
    typ: u16,
    count: usize,
    off: usize,
}

type Ifd = HashMap<u16, Entry>;

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

impl<'a> Tiff<'a> {
    fn bytes(&self, off: usize, len: usize) -> Res<&'a [u8]> {
        off.checked_add(len)
            .and_then(|end| self.d.get(off..end))
            .ok_or_else(|| format!("read of {len} bytes at {off} past end of file ({})", self.d.len()))
    }
    fn u16(&self, off: usize) -> Res<u16> {
        let b: [u8; 2] = self.bytes(off, 2)?.try_into().unwrap();
        Ok(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }
    fn u32(&self, off: usize) -> Res<u32> {
        let b: [u8; 4] = self.bytes(off, 4)?.try_into().unwrap();
        Ok(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn u64(&self, off: usize) -> Res<u64> {
        let b: [u8; 8] = self.bytes(off, 8)?.try_into().unwrap();
        Ok(if self.le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) })
    }

    fn ifd(&self, off: usize) -> Res<(Ifd, usize)> {
        let n = self.u16(off)? as usize;
        let mut ifd = Ifd::new();
        for i in 0..n {
            let e = off + 2 + 12 * i;
            let (tag, typ, count) = (self.u16(e)?, self.u16(e + 2)?, self.u32(e + 4)? as usize);
            let size = type_size(typ).saturating_mul(count);
            let off = if size <= 4 { e + 8 } else { self.u32(e + 8)? as usize };
            ifd.insert(tag, Entry { typ, count, off });
        }
        Ok((ifd, self.u32(off + 2 + 12 * n)? as usize))
    }

    fn f64s(&self, e: Entry) -> Res<Vec<f64>> {
        let sz = type_size(e.typ);
        self.bytes(e.off, sz.saturating_mul(e.count))?;
        (0..e.count)
            .map(|i| {
                let o = e.off + i * sz;
                Ok(match e.typ {
                    1 | 7 => self.d[o] as f64,
                    6 => self.d[o] as i8 as f64,
                    3 => self.u16(o)? as f64,
                    8 => self.u16(o)? as i16 as f64,
                    4 | 13 => self.u32(o)? as f64,
                    9 => self.u32(o)? as i32 as f64,
                    5 => self.u32(o)? as f64 / self.u32(o + 4)? as f64,
                    10 => self.u32(o)? as i32 as f64 / self.u32(o + 4)? as i32 as f64,
                    11 => f32::from_bits(self.u32(o)?) as f64,
                    12 => f64::from_bits(self.u64(o)?),
                    t => return Err(format!("non-numeric TIFF type {t}")),
                })
            })
            .collect()
    }

    fn raw_bytes(&self, e: Entry) -> Res<&'a [u8]> {
        self.bytes(e.off, type_size(e.typ).saturating_mul(e.count))
    }
}

fn type_size(t: u16) -> usize {
    match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// Tag lookup: raw IFD first, then IFD0 (DNG allows most tags in either).
struct Lookup<'a, 'b> {
    t: &'b Tiff<'a>,
    raw: &'b Ifd,
    ifd0: &'b Ifd,
}

impl Lookup<'_, '_> {
    fn entry(&self, tag: u16) -> Option<Entry> {
        self.raw.get(&tag).or_else(|| self.ifd0.get(&tag)).copied()
    }
    fn nums(&self, tag: u16) -> Res<Option<Vec<f64>>> {
        self.entry(tag).map(|e| self.t.f64s(e)).transpose()
    }
    fn num(&self, tag: u16) -> Res<Option<f64>> {
        Ok(self.nums(tag)?.and_then(|v| v.first().copied()))
    }
    fn req(&self, tag: u16, name: &str) -> Res<Vec<f64>> {
        self.nums(tag)?.ok_or_else(|| format!("missing {name} ({tag})"))
    }
    fn mat3(&self, tag: u16) -> Res<Option<Mat3>> {
        match self.nums(tag)? {
            None => Ok(None),
            Some(v) if v.len() == 9 => Ok(Some(std::array::from_fn(|i| std::array::from_fn(|j| v[3 * i + j])))),
            Some(v) => Err(format!("tag {tag}: {} values, only 3-color CFA supported", v.len())),
        }
    }
    fn ascii(&self, tag: u16) -> String {
        self.entry(tag)
            .and_then(|e| self.t.raw_bytes(e).ok())
            .map(|b| String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string())
            .unwrap_or_default()
    }
}

pub fn read_dng(d: &[u8]) -> Res<Dng> {
    let le = match d.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err("not a TIFF/DNG file".into()),
    };
    let t = Tiff { d, le };
    if t.u16(2)? != 42 {
        return Err("bad TIFF magic".into());
    }

    // Collect IFD0, its chain and SubIFDs (DngCreator puts the raw in IFD0, or in a
    // SubIFD when a thumbnail is present).
    let first = t.u32(4)? as usize;
    let mut ifds: Vec<Ifd> = Vec::new();
    let (mut queue, mut seen) = (vec![first], HashSet::new());
    while let Some(off) = queue.pop() {
        if off == 0 || !seen.insert(off) || ifds.len() > 64 {
            continue;
        }
        let (ifd, next) = t.ifd(off)?;
        if let Some(&e) = ifd.get(&tag::SUB_IFDS) {
            queue.extend(t.f64s(e)?.into_iter().map(|v| v as usize));
        }
        queue.push(next);
        ifds.push(ifd);
    }
    let ifd0 = ifds.first().ok_or("no IFD")?.clone();
    let photometric = |i: &Ifd| i.get(&tag::PHOTOMETRIC).and_then(|&e| t.f64s(e).ok()).map(|v| v[0] as u32);
    let subtype = |i: &Ifd| i.get(&tag::NEW_SUBFILE_TYPE).and_then(|&e| t.f64s(e).ok()).map_or(0, |v| v[0] as u32);
    let raw_ifd = ifds
        .iter()
        .find(|i| photometric(i) == Some(PHOTOMETRIC_CFA) && subtype(i) & 1 == 0)
        .ok_or("no full-resolution CFA image (LinearRaw / demosaiced DNGs are not supported)")?
        .clone();
    let l = Lookup { t: &t, raw: &raw_ifd, ifd0: &ifd0 };
    let mut warnings = Vec::new();

    // --- pixels ---
    let width = l.req(tag::WIDTH, "ImageWidth")?[0] as usize;
    let height = l.req(tag::HEIGHT, "ImageLength")?[0] as usize;
    let bits = l.req(tag::BITS, "BitsPerSample")?[0] as u32;
    let compression = l.num(tag::COMPRESSION)?.unwrap_or(1.0) as u32;
    if compression != 1 && compression != 7 {
        return Err(format!("compression {compression} not supported (uncompressed or lossless JPEG only)"));
    }
    if l.num(tag::SAMPLES)?.unwrap_or(1.0) != 1.0 {
        return Err("SamplesPerPixel != 1".into());
    }
    if !(8..=16).contains(&bits) {
        return Err(format!("BitsPerSample {bits}"));
    }
    if l.entry(tag::LINEARIZATION).is_some() {
        return Err("LinearizationTable not supported".into());
    }
    let full = if compression == 7 {
        read_ljpeg(&t, &raw_ifd, width, height)?
    } else {
        read_pixels(&t, &raw_ifd, width, height, bits)?
    };

    // ActiveArea: top, left, bottom, right. CFA, black level and opcodes are relative to it.
    let (aw, ah, data) = match l.nums(tag::ACTIVE_AREA)? {
        Some(a) if a.len() == 4 => {
            let [top, left, bottom, right] = [a[0], a[1], a[2], a[3]].map(|v| v as usize);
            if bottom > height || right > width || top >= bottom || left >= right {
                return Err(format!("ActiveArea {a:?} outside {width}x{height}"));
            }
            let mut out = Vec::with_capacity((bottom - top) * (right - left));
            for y in top..bottom {
                out.extend_from_slice(&full[y * width + left..y * width + right]);
            }
            (right - left, bottom - top, out)
        }
        _ => (width, height, full),
    };
    let raw = RawImage::new(aw, ah, data).map_err(|e| e.to_string())?;

    // --- CFA ---
    let dim = l.nums(tag::CFA_REPEAT_DIM)?.unwrap_or(vec![2.0, 2.0]);
    if dim != [2.0, 2.0] {
        return Err(format!("CFARepeatPatternDim {dim:?} (only 2x2 Bayer)"));
    }
    let pat = l.req(tag::CFA_PATTERN, "CFAPattern")?;
    if pat.len() != 4 {
        return Err(format!("CFAPattern {pat:?}"));
    }
    let cfa = CfaPattern::from_dng(std::array::from_fn(|i| pat[i] as u8)).map_err(|e| e.to_string())?;

    // --- levels ---
    let bdim = l.nums(tag::BLACK_REPEAT_DIM)?.unwrap_or(vec![1.0, 1.0]);
    let (br, bc) = (bdim[0] as usize, bdim.get(1).copied().unwrap_or(1.0) as usize);
    let bl = l.nums(tag::BLACK_LEVEL)?.unwrap_or(vec![0.0]);
    let black_level: [f32; 4] = if (br == 1 || br == 2) && (bc == 1 || bc == 2) && bl.len() >= br * bc {
        std::array::from_fn(|p| bl[(p / 2 % br) * bc + p % 2 % bc] as f32)
    } else {
        let avg = bl.iter().sum::<f64>() / bl.len().max(1) as f64;
        warnings.push(format!("BlackLevelRepeatDim {bdim:?}: using mean black {avg:.2}"));
        [avg as f32; 4]
    };
    for (t_, name) in [(tag::BLACK_DELTA_H, "BlackLevelDeltaH"), (tag::BLACK_DELTA_V, "BlackLevelDeltaV")] {
        if l.nums(t_)?.is_some_and(|v| v.iter().any(|&x| x != 0.0)) {
            warnings.push(format!("{name} ignored"));
        }
    }
    let white_level = l.num(tag::WHITE_LEVEL)?.unwrap_or(((1u32 << bits) - 1) as f64) as f32;

    // --- white balance ---
    let wb_gains = match l.nums(tag::AS_SHOT_NEUTRAL)? {
        Some(n) if n.len() == 3 && n.iter().all(|&v| v > 0.0) => {
            [1.0 / n[0], 1.0 / n[1], 1.0 / n[1], 1.0 / n[2]].map(|v| v as f32)
        }
        _ => {
            warnings.push("no AsShotNeutral: WB gains 1".into());
            [1.0; 4]
        }
    };

    // --- color ---
    let cal = |cm: u16, fm: u16, cc: u16, il: u16| -> Res<Option<Calibration>> {
        let Some(color_matrix) = l.mat3(cm)? else { return Ok(None) };
        Ok(Some(Calibration {
            illuminant: l.num(il)?.unwrap_or(21.0) as u16,
            color_matrix,
            forward_matrix: l.mat3(fm)?,
            camera_calibration: l.mat3(cc)?.unwrap_or(IDENTITY),
        }))
    };
    let cal1 = cal(tag::COLOR_MATRIX1, tag::FORWARD_MATRIX1, tag::CALIBRATION1, tag::ILLUMINANT1)?
        .ok_or("missing ColorMatrix1")?;
    let cal2 = cal(tag::COLOR_MATRIX2, tag::FORWARD_MATRIX2, tag::CALIBRATION2, tag::ILLUMINANT2)?;
    let analog_balance = match l.nums(tag::ANALOG_BALANCE)? {
        Some(v) if v.len() == 3 => [v[0], v[1], v[2]],
        _ => [1.0; 3],
    };

    // --- opcodes ---
    if let Some(e) = l.entry(tag::OPCODE_LIST1) {
        for op in parse_opcodes(t.raw_bytes(e)?)? {
            warnings.push(format!("OpcodeList1: opcode {} ignored", op.id));
        }
    }
    let mut lens_shading = None;
    if let Some(e) = l.entry(tag::OPCODE_LIST2) {
        let ops = parse_opcodes(t.raw_bytes(e)?)?;
        let maps: Vec<GainMap> = ops.iter().filter(|o| o.id == 9).map(|o| GainMap::parse(o.params)).collect::<Res<_>>()?;
        for op in ops.iter().filter(|o| o.id != 9) {
            warnings.push(format!("OpcodeList2: opcode {} ignored", op.id));
        }
        if !maps.is_empty() {
            match gain_maps_to_shading(&maps, cfa) {
                Ok(m) => lens_shading = non_identity(m),
                Err(err) => warnings.push(format!("GainMap ignored: {err}")),
            }
        }
    }
    if l.entry(tag::OPCODE_LIST3).is_some() {
        warnings.push("OpcodeList3 (lens distortion) ignored".into());
    }

    // --- crop / orientation ---
    let crop = match (l.nums(tag::CROP_ORIGIN)?, l.nums(tag::CROP_SIZE)?) {
        (Some(o), Some(s)) if o.len() == 2 && s.len() == 2 => {
            let r = Rect { x: o[0].round() as usize, y: o[1].round() as usize, width: s[0].round() as usize, height: s[1].round() as usize };
            (r.x + r.width <= aw && r.y + r.height <= ah && (r.width, r.height) != (aw, ah)).then_some(r)
        }
        _ => None,
    };
    let orientation = ifd0
        .get(&tag::ORIENTATION)
        .and_then(|&e| t.f64s(e).ok())
        .map_or(1, |v| v[0] as u16)
        .clamp(1, 8);

    let meta = FrameMeta {
        cfa,
        black_level,
        white_level,
        lens_shading,
        // DNG does not record HAL hot pixel correction; the CLI decides.
        hot_pixels_corrected: true,
        wb_gains,
        color: ColorProfile { cal1, cal2, analog_balance },
        orientation,
        crop,
    };
    Ok(Dng { raw, meta, make: l.ascii(tag::MAKE), model: l.ascii(tag::MODEL), bits, warnings })
}

/// Uncompressed strips or tiles. 8/16-bit samples use the file byte order; other depths
/// are bit-packed MSB first regardless of byte order (DNG spec, "BitsPerSample").
fn read_pixels(t: &Tiff, ifd: &Ifd, w: usize, h: usize, bits: u32) -> Res<Vec<u16>> {
    let get = |tag: u16| ifd.get(&tag).map(|&e| t.f64s(e)).transpose();
    let mut out = vec![0u16; w * h];
    let decode_row = |src: &[u8], dst: &mut [u16]| {
        match bits {
            8 => dst.iter_mut().zip(src).for_each(|(d, &s)| *d = s as u16),
            16 => dst.iter_mut().zip(src.chunks_exact(2)).for_each(|(d, s)| {
                *d = if t.le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) }
            }),
            _ => {
                let mut bitpos = 0usize;
                for d in dst.iter_mut() {
                    let mut v = 0u32;
                    for _ in 0..bits {
                        v = v << 1 | (src[bitpos >> 3] >> (7 - (bitpos & 7)) & 1) as u32;
                        bitpos += 1;
                    }
                    *d = v as u16;
                }
            }
        }
    };
    let row_bytes = |n: usize| (n * bits as usize).div_ceil(8);

    if let Some(offsets) = get(tag::TILE_OFFSETS)? {
        let tw = get(tag::TILE_WIDTH)?.ok_or("TileWidth")?[0] as usize;
        let th = get(tag::TILE_LENGTH)?.ok_or("TileLength")?[0] as usize;
        let across = w.div_ceil(tw);
        let mut buf = vec![0u16; tw];
        for (i, &off) in offsets.iter().enumerate() {
            let (ty, tx) = (i / across * th, i % across * tw);
            for r in 0..th.min(h.saturating_sub(ty)) {
                let src = t.bytes(off as usize + r * row_bytes(tw), row_bytes(tw))?;
                decode_row(src, &mut buf);
                let n = tw.min(w - tx);
                out[(ty + r) * w + tx..][..n].copy_from_slice(&buf[..n]);
            }
        }
    } else {
        let offsets = get(tag::STRIP_OFFSETS)?.ok_or("no StripOffsets / TileOffsets")?;
        let rps = get(tag::ROWS_PER_STRIP)?.map_or(h, |v| v[0] as usize).max(1);
        for y in 0..h {
            let strip = *offsets.get(y / rps).ok_or("too few strips")? as usize;
            let src = t.bytes(strip + (y % rps) * row_bytes(w), row_bytes(w))?;
            decode_row(src, &mut out[y * w..(y + 1) * w]);
        }
        let _ = get(tag::STRIP_BYTES)?;
    }
    Ok(out)
}

/// Compression 7: every tile (or strip) is an independent lossless JPEG stream whose
/// samples, in stream order, fill the tile row by row (DNG spec 1.6, "Compression").
fn read_ljpeg(t: &Tiff, ifd: &Ifd, w: usize, h: usize) -> Res<Vec<u16>> {
    let get = |tag: u16| ifd.get(&tag).map(|&e| t.f64s(e)).transpose();
    let tiled = get(tag::TILE_OFFSETS)?.is_some();
    let (offsets, counts, tw, th) = if tiled {
        (
            get(tag::TILE_OFFSETS)?.unwrap(),
            get(tag::TILE_BYTES)?.ok_or("TileByteCounts")?,
            get(tag::TILE_WIDTH)?.ok_or("TileWidth")?[0] as usize,
            get(tag::TILE_LENGTH)?.ok_or("TileLength")?[0] as usize,
        )
    } else {
        let rps = get(tag::ROWS_PER_STRIP)?.map_or(h, |v| v[0] as usize).max(1);
        (get(tag::STRIP_OFFSETS)?.ok_or("StripOffsets")?, get(tag::STRIP_BYTES)?.ok_or("StripByteCounts")?, w, rps)
    };
    if offsets.len() != counts.len() {
        return Err("tile offsets / byte counts mismatch".into());
    }
    let across = w.div_ceil(tw);
    let mut out = vec![0u16; w * h];
    for (i, (&off, &len)) in offsets.iter().zip(&counts).enumerate() {
        let dec = crate::ljpeg::decode(t.bytes(off as usize, len as usize)?).map_err(|e| format!("tile {i}: {e}"))?;
        if dec.samples.len() < tw * th.min(h - (i / across * th).min(h)) {
            return Err(format!("tile {i}: {} samples for {tw}x{th}", dec.samples.len()));
        }
        let (ty, tx) = (i / across * th, i % across * tw);
        for r in 0..th.min(h.saturating_sub(ty)) {
            let n = tw.min(w - tx);
            out[(ty + r) * w + tx..][..n].copy_from_slice(&dec.samples[r * tw..][..n]);
        }
    }
    Ok(out)
}

struct Opcode<'a> {
    id: u32,
    params: &'a [u8],
}

/// Opcode lists are always big-endian (DNG spec 1.6, ch. 7 "Opcode List Processing").
fn parse_opcodes(d: &[u8]) -> Res<Vec<Opcode<'_>>> {
    let be = |o: usize| -> Res<u32> {
        d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| "truncated opcode list".to_string())
    };
    let n = be(0)? as usize;
    let mut ops = Vec::with_capacity(n);
    let mut o = 4;
    for _ in 0..n {
        let (id, len) = (be(o)?, be(o + 12)? as usize);
        let params = d.get(o + 16..o + 16 + len).ok_or("truncated opcode")?;
        ops.push(Opcode { id, params });
        o += 16 + len;
    }
    Ok(ops)
}

/// GainMap opcode (id 9) parameters, DNG spec 1.6, ch. 7.
struct GainMap {
    top: u32,
    left: u32,
    row_pitch: u32,
    col_pitch: u32,
    points_v: usize,
    points_h: usize,
    spacing: [f64; 2],
    origin: [f64; 2],
    planes: u32,
    gains: Vec<f32>,
}

impl GainMap {
    fn parse(p: &[u8]) -> Res<Self> {
        let u = |i: usize| -> Res<u32> { p.get(i..i + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or("short GainMap".into()) };
        let f = |i: usize| -> Res<f64> { p.get(i..i + 8).map(|b| f64::from_be_bytes(b.try_into().unwrap())).ok_or("short GainMap".into()) };
        let (points_v, points_h, map_planes) = (u(32)? as usize, u(36)? as usize, u(72)?);
        let n = points_v * points_h * map_planes as usize;
        let gains = (0..n)
            .map(|i| p.get(76 + 4 * i..80 + 4 * i).map(|b| f32::from_be_bytes(b.try_into().unwrap())).ok_or("short GainMap data".to_string()))
            .collect::<Res<_>>()?;
        Ok(Self {
            top: u(0)?,
            left: u(4)?,
            row_pitch: u(24)?,
            col_pitch: u(28)?,
            points_v,
            points_h,
            spacing: [f(40)?, f(48)?],
            origin: [f(56)?, f(64)?],
            planes: map_planes,
            gains,
        })
    }
}

/// Converts the 4 per-CFA-position GainMaps written by DngCreator back to the Camera2
/// lens shading map layout. Other GainMap layouts are rejected rather than approximated.
fn gain_maps_to_shading(maps: &[GainMap], cfa: CfaPattern) -> Res<LensShadingMap> {
    if maps.len() != 4 {
        return Err(format!("{} GainMaps, expected 4", maps.len()));
    }
    let (rows, cols) = (maps[0].points_v, maps[0].points_h);
    let mut gains = vec![[1.0f32; 4]; rows * cols];
    let mut filled = [false; 4];
    for m in maps {
        // A single-point axis has no spacing to check (Samsung writes 1x1 maps with spacing 1).
        let spacing_ok = |n: usize, s: f64| n == 1 || (s - 1.0 / (n - 1) as f64).abs() < 1e-6;
        let fits = m.row_pitch == 2
            && m.col_pitch == 2
            && m.planes == 1
            && (m.points_v, m.points_h) == (rows, cols)
            && m.origin.iter().all(|o| o.abs() < 1e-6)
            && spacing_ok(rows, m.spacing[0])
            && spacing_ok(cols, m.spacing[1]);
        if !fits {
            return Err("GainMap layout differs from DngCreator's".into());
        }
        let pos = ((m.top & 1) * 2 + (m.left & 1)) as usize;
        let ch = cfa.camera2_channel(pos);
        filled[ch] = true;
        for (g, &v) in gains.iter_mut().zip(&m.gains) {
            g[ch] = v;
        }
    }
    if filled != [true; 4] {
        return Err("GainMaps do not cover all 4 CFA positions".into());
    }
    Ok(LensShadingMap { rows, cols, gains })
}

/// `None` when the map is all ones: the HAL already applied shading and wrote an identity map.
fn non_identity(m: LensShadingMap) -> Option<LensShadingMap> {
    m.gains.iter().flatten().any(|&g| (g - 1.0).abs() > 1e-6).then_some(m)
}
