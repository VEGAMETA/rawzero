//! Lossless JPEG decoder (ITU-T T.81, process 14, Huffman, predictors 1..7), as used by
//! DNG `Compression = 7`. Needed to read DNGs from stock camera apps; our own captures
//! come from DngCreator uncompressed. Returns samples in stream order (row-major,
//! components interleaved), which DNG maps directly onto the tile's pixels.

type Res<T> = Result<T, String>;

struct Huffman {
    /// Indexed by the next 16 bits: (code length, symbol); length 0 = invalid code.
    lut: Vec<(u8, u8)>,
}

impl Huffman {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Res<Self> {
        let mut lut = vec![(0u8, 0u8); 1 << 16];
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16u32 {
            for _ in 0..counts[len as usize - 1] {
                let sym = *symbols.get(k).ok_or("DHT: too few symbols")?;
                let shift = 16 - len;
                let start = (code << shift) as usize;
                if start + (1 << shift) > lut.len() {
                    return Err("DHT: invalid code lengths".into());
                }
                lut[start..start + (1 << shift)].fill((len as u8, sym));
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Ok(Self { lut })
    }
}

/// MSB-first bit reader over entropy-coded data with 0xFF00 byte stuffing. Stops feeding
/// real bits at the first marker and pads with zeros.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
}

impl<'a> Bits<'a> {
    fn fill(&mut self) {
        while self.n <= 56 {
            let mut b = 0u8;
            if self.pos < self.d.len() {
                b = self.d[self.pos];
                if b == 0xFF {
                    match self.d.get(self.pos + 1) {
                        Some(0x00) => self.pos += 2,
                        _ => {
                            b = 0;
                            self.pos = self.d.len(); // marker: end of scan
                        }
                    }
                } else {
                    self.pos += 1;
                }
            }
            self.acc |= (b as u64) << (56 - self.n);
            self.n += 8;
        }
    }
    #[inline]
    fn peek16(&mut self) -> u32 {
        if self.n < 16 {
            self.fill();
        }
        (self.acc >> 48) as u32
    }
    #[inline]
    fn skip(&mut self, k: u32) {
        self.acc <<= k;
        self.n -= k;
    }
    #[inline]
    fn get(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        if self.n < k {
            self.fill();
        }
        let v = (self.acc >> (64 - k)) as u32;
        self.skip(k);
        v
    }
}

pub struct Decoded {
    pub samples: Vec<u16>,
}

pub fn decode(d: &[u8]) -> Res<Decoded> {
    if d.get(..2) != Some(&[0xFF, 0xD8]) {
        return Err("LJ92: missing SOI".into());
    }
    let mut tables: [Option<Huffman>; 4] = [None, None, None, None];
    let (mut precision, mut width, mut height, mut comps) = (0u32, 0usize, 0usize, Vec::<u8>::new());
    let mut p = 2;
    loop {
        if p + 4 > d.len() || d[p] != 0xFF {
            return Err(format!("LJ92: bad marker at {p}"));
        }
        let marker = d[p + 1];
        let len = u16::from_be_bytes([d[p + 2], d[p + 3]]) as usize;
        let seg = d.get(p + 4..p + 2 + len).ok_or("LJ92: truncated segment")?;
        match marker {
            0xC4 => {
                let mut s = seg;
                while !s.is_empty() {
                    let id = (s[0] & 0x0F) as usize;
                    let counts: [u8; 16] = s.get(1..17).ok_or("DHT short")?.try_into().unwrap();
                    let total: usize = counts.iter().map(|&c| c as usize).sum();
                    let syms = s.get(17..17 + total).ok_or("DHT short")?;
                    *tables.get_mut(id).ok_or("DHT id")? = Some(Huffman::new(&counts, syms)?);
                    s = &s[17 + total..];
                }
            }
            0xC3 => {
                precision = seg[0] as u32;
                height = u16::from_be_bytes([seg[1], seg[2]]) as usize;
                width = u16::from_be_bytes([seg[3], seg[4]]) as usize;
                let n = seg[5] as usize;
                comps = (0..n).map(|i| seg[6 + 3 * i]).collect();
            }
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                return Err(format!("LJ92: SOF{} is not lossless", marker - 0xC0));
            }
            0xDD => {
                if u16::from_be_bytes([seg[0], seg[1]]) != 0 {
                    return Err("LJ92: restart intervals not supported".into());
                }
            }
            0xDA => {
                let n = seg[0] as usize;
                if n != comps.len() || n == 0 {
                    return Err("LJ92: scan components differ from frame".into());
                }
                let tsel: Vec<usize> = (0..n).map(|i| (seg[2 + 2 * i] >> 4) as usize).collect();
                let psv = seg[1 + 2 * n];
                let pt = (seg[3 + 2 * n] & 0x0F) as u32;
                let tabs: Vec<&Huffman> = tsel
                    .iter()
                    .map(|&t| tables.get(t).and_then(|h| h.as_ref()).ok_or("LJ92: missing Huffman table"))
                    .collect::<Result<_, _>>()?;
                let data = &d[p + 2 + len..];
                return scan(data, &tabs, width, height, precision, psv, pt);
            }
            _ => {}
        }
        p += 2 + len;
    }
}

fn scan(data: &[u8], tabs: &[&Huffman], width: usize, height: usize, precision: u32, psv: u8, pt: u32) -> Res<Decoded> {
    if !(1..=7).contains(&psv) || precision == 0 || precision > 16 {
        return Err(format!("LJ92: predictor {psv}, precision {precision}"));
    }
    let nc = tabs.len();
    let row_len = width * nc;
    let mut out = vec![0u16; row_len * height];
    let mut bits = Bits { d: data, pos: 0, acc: 0, n: 0 };
    let initial = 1i32 << (precision - pt - 1);
    for y in 0..height {
        for x in 0..width {
            for c in 0..nc {
                let i = y * row_len + x * nc + c;
                let (len, ssss) = tabs[c].lut[bits.peek16() as usize];
                if len == 0 {
                    return Err(format!("LJ92: bad Huffman code at row {y}"));
                }
                bits.skip(len as u32);
                let diff = match ssss {
                    0 => 0,
                    16 => 32768,
                    s => {
                        let v = bits.get(s as u32) as i32;
                        if v < 1 << (s - 1) { v - (1 << s) + 1 } else { v }
                    }
                };
                let ra = || out[i - nc] as i32;
                let rb = || out[i - row_len] as i32;
                let rc = || out[i - row_len - nc] as i32;
                let pred = if y == 0 && x == 0 {
                    initial
                } else if y == 0 {
                    ra()
                } else if x == 0 {
                    rb()
                } else {
                    match psv {
                        1 => ra(),
                        2 => rb(),
                        3 => rc(),
                        4 => ra() + rb() - rc(),
                        5 => ra() + ((rb() - rc()) >> 1),
                        6 => rb() + ((ra() - rc()) >> 1),
                        _ => (ra() + rb()) >> 1,
                    }
                };
                out[i] = (pred + diff) as u16;
            }
        }
    }
    if pt > 0 {
        out.iter_mut().for_each(|v| *v <<= pt);
    }
    Ok(Decoded { samples: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built stream: 1 component, 2x2, 8-bit, predictor 1, a table where
    /// SSSS 0 -> "0", SSSS 1 -> "10", SSSS 2 -> "11".
    #[test]
    fn decodes_tiny_stream() {
        let mut s = vec![0xFF, 0xD8];
        s.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x16, 0x00, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2]);
        s.extend_from_slice(&[0xFF, 0xC3, 0x00, 0x0B, 8, 0, 2, 0, 2, 1, 1, 0x11, 0]);
        s.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0x00, 1, 0, 0]);
        // Samples: 128 (diff 0), 130 (diff +2), 127 (from above 128: diff -1), 129 (pred left 127: +2)
        // Bits: 0 | 11 10 | 10 0 | 11 10  -> 0111 0100 1110 + padding 1111
        s.extend_from_slice(&[0b0111_0100, 0b1110_1111, 0xFF, 0xD9]);
        let d = decode(&s).unwrap();
        assert_eq!(d.samples, vec![128, 130, 127, 129]);
    }
}
