//! Step 6: demosaic, Malvar–He–Cutler.
//!
//! H. S. Malvar, L. He, R. Cutler, "High-quality linear interpolation for demosaicing of
//! Bayer-patterned color images", ICASSP 2004. Bilinear interpolation plus a Laplacian
//! correction from the known channel; four fixed 5x5 kernels, coefficients in eighths.
//! Borders use reflect-101 padding, which preserves the CFA phase.

use crate::image::{Mosaic, RgbImage};
use crate::meta::{cell, CfaPattern, B, G, R};

const PAD: usize = 2;

/// Kernels as (dy, dx, weight*8), Fig. 2 of the paper.
/// G at an R or B site.
const K_G_AT_RB: &[(isize, isize, f32)] = &[
    (0, 0, 4.0),
    (-1, 0, 2.0), (1, 0, 2.0), (0, -1, 2.0), (0, 1, 2.0),
    (-2, 0, -1.0), (2, 0, -1.0), (0, -2, -1.0), (0, 2, -1.0),
];
/// R (or B) at a G site whose horizontal neighbors are R (or B).
const K_ROW: &[(isize, isize, f32)] = &[
    (0, 0, 5.0),
    (0, -1, 4.0), (0, 1, 4.0),
    (-1, -1, -1.0), (-1, 1, -1.0), (1, -1, -1.0), (1, 1, -1.0),
    (0, -2, -1.0), (0, 2, -1.0),
    (-2, 0, 0.5), (2, 0, 0.5),
];
/// R (or B) at a G site whose vertical neighbors are R (or B): transpose of `K_ROW`.
const K_COL: &[(isize, isize, f32)] = &[
    (0, 0, 5.0),
    (-1, 0, 4.0), (1, 0, 4.0),
    (-1, -1, -1.0), (-1, 1, -1.0), (1, -1, -1.0), (1, 1, -1.0),
    (-2, 0, -1.0), (2, 0, -1.0),
    (0, -2, 0.5), (0, 2, 0.5),
];
/// R at a B site and B at an R site.
const K_DIAG: &[(isize, isize, f32)] = &[
    (0, 0, 6.0),
    (-1, -1, 2.0), (-1, 1, 2.0), (1, -1, 2.0), (1, 1, 2.0),
    (-2, 0, -1.5), (2, 0, -1.5), (0, -2, -1.5), (0, 2, -1.5),
];

/// What to compute for each of the 3 output channels at one 2x2 cell position.
#[derive(Clone, Copy)]
enum Op {
    Copy,
    Kernel(&'static [(isize, isize, f32)]),
}

fn ops_for(cfa: CfaPattern) -> [[Op; 3]; 4] {
    let lay = cfa.layout();
    std::array::from_fn(|p| {
        let here = lay[p];
        // Color of the horizontal neighbor (same row, other column of the cell).
        let horiz = lay[p ^ 1];
        std::array::from_fn(|c| {
            if c == here {
                Op::Copy
            } else if c == G {
                Op::Kernel(K_G_AT_RB)
            } else if here == G {
                Op::Kernel(if horiz == c { K_ROW } else { K_COL })
            } else {
                Op::Kernel(K_DIAG)
            }
        })
    })
}

pub fn demosaic_mhc(m: &Mosaic, cfa: CfaPattern) -> RgbImage {
    let (w, h) = (m.width, m.height);
    let p = pad_reflect(m);
    let pw = w + 2 * PAD;
    let ops = ops_for(cfa);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let center = (y + PAD) * pw + x + PAD;
            let at = |dy: isize, dx: isize| p[(center as isize + dy * pw as isize + dx) as usize];
            let op = &ops[cell(x, y)];
            let px: [f32; 3] = std::array::from_fn(|c| match op[c] {
                Op::Copy => at(0, 0),
                Op::Kernel(k) => k.iter().map(|&(dy, dx, wt)| wt * at(dy, dx)).sum::<f32>() * 0.125,
            });
            data.push(px);
        }
    }
    debug_assert_eq!([R, G, B], [0, 1, 2]);
    RgbImage { width: w, height: h, data }
}

/// Reflect-101 (mirror without repeating the edge): index -1 -> 1, w -> w-2.
fn pad_reflect(m: &Mosaic) -> Vec<f32> {
    let (w, h) = (m.width as isize, m.height as isize);
    let refl = |i: isize, n: isize| -> usize {
        let i = if i < 0 { -i } else if i >= n { 2 * (n - 1) - i } else { i };
        i.clamp(0, n - 1) as usize
    };
    let pw = m.width + 2 * PAD;
    let mut out = Vec::with_capacity(pw * (m.height + 2 * PAD));
    for py in -(PAD as isize)..h + PAD as isize {
        let row = &m.data[refl(py, h) * m.width..][..m.width];
        for px in -(PAD as isize)..0 {
            out.push(row[refl(px, w)]);
        }
        out.extend_from_slice(row);
        for px in w..w + PAD as isize {
            out.push(row[refl(px, w)]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mosaic_from(w: usize, h: usize, cfa: CfaPattern, f: impl Fn(usize, usize) -> [f32; 3]) -> Mosaic {
        let data = (0..w * h).map(|i| f(i % w, i / w)[cfa.color_at(i % w, i / w)]).collect();
        Mosaic { width: w, height: h, data }
    }

    #[test]
    fn flat_color_is_exact_for_all_patterns() {
        for cfa in [CfaPattern::Rggb, CfaPattern::Grbg, CfaPattern::Gbrg, CfaPattern::Bggr] {
            let m = mosaic_from(8, 6, cfa, |_, _| [0.2, 0.5, 0.8]);
            for px in demosaic_mhc(&m, cfa).data {
                for (a, b) in px.iter().zip([0.2, 0.5, 0.8]) {
                    assert!((a - b).abs() < 1e-6, "{cfa:?}: {px:?}");
                }
            }
        }
    }

    #[test]
    fn linear_ramp_is_exact_in_interior() {
        // All kernels sum to the identity on affine signals (zero-sum Laplacian terms).
        for cfa in [CfaPattern::Rggb, CfaPattern::Gbrg] {
            let f = |x: usize, y: usize| {
                let (x, y) = (x as f32, y as f32);
                [0.01 * x + 0.02 * y, 0.03 * x - 0.01 * y + 0.5, 0.02 * x + 0.02 * y + 0.1]
            };
            let (w, h) = (12, 10);
            let out = demosaic_mhc(&mosaic_from(w, h, cfa, f), cfa);
            for y in 2..h - 2 {
                for x in 2..w - 2 {
                    let (got, exp) = (out.data[y * w + x], f(x, y));
                    for c in 0..3 {
                        assert!((got[c] - exp[c]).abs() < 1e-5, "{cfa:?} ({x},{y}) c{c}");
                    }
                }
            }
        }
    }

    #[test]
    fn reflect_padding_keeps_phase() {
        let m = Mosaic { width: 3, height: 1, data: vec![1.0, 2.0, 3.0] };
        let p = pad_reflect(&m);
        assert_eq!(&p[0..7], &[3.0, 2.0, 1.0, 2.0, 3.0, 2.0, 1.0]);
    }
}
