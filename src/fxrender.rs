//! CPU compositor for the monitor preview (and, later, export): the source frame is
//! letterbox-fitted into the canvas, then transformed (position, scale, rotation), colour-adjusted
//! (brightness, contrast, saturation) and faded (opacity, over black).
//!
//! Input is the clip's *evaluated* parameter array (`Fx::eval`), in the same units as the
//! Effect Controls panel. Rows are split across threads; every pixel is an inverse-mapped
//! bilinear sample, so rotation and sub-pixel moves stay smooth.

use crate::project::NPARAM;

struct Params {
    sw: usize,
    sh: usize,
    dw: usize,
    cx: f32,
    cy: f32,
    k: f32,   // canvas px per source px
    sin: f32, // inverse rotation
    cos: f32,
    opacity: f32,
    bright: f32,
    contrast: f32,
    sat: f32,
    color: bool,
}

/// Renders `src` (RGBA, `sw`x`sh`) into `dst` (RGBA, `dw`x`dh`).
pub fn render(src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], dst: &mut [u8], dw: usize, dh: usize) {
    let n = dw * dh * 4;
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 4 || dst.len() < n {
        if dst.len() >= n {
            dst[..n].fill(0);
        }
        return;
    }
    let fit = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
    let (sin, cos) = (-v[3].to_radians()).sin_cos();
    let p = Params {
        sw,
        sh,
        dw,
        cx: dw as f32 * 0.5 + (v[0] - 960.0) / 960.0 * dw as f32 * 0.5,
        cy: dh as f32 * 0.5 + (v[1] - 540.0) / 540.0 * dh as f32 * 0.5,
        k: (fit * (v[2] / 100.0).max(0.001)).max(1e-4),
        sin,
        cos,
        opacity: (v[4] / 100.0).clamp(0.0, 1.0),
        bright: v[5] / 100.0 * 255.0,
        contrast: v[6] / 100.0,
        sat: v[7] / 100.0,
        color: v[5] != 0.0 || v[6] != 100.0 || v[7] != 100.0,
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let rows_per = ((dh + threads - 1) / threads).max(1);
    let p = &p;
    std::thread::scope(|s| {
        for (ci, chunk) in dst[..n].chunks_mut(rows_per * dw * 4).enumerate() {
            s.spawn(move || render_rows(src, p, chunk, ci * rows_per));
        }
    });
}

fn render_rows(src: &[u8], p: &Params, chunk: &mut [u8], y0: usize) {
    let (sw, sh) = (p.sw as i32, p.sh as i32);
    for (ry, row) in chunk.chunks_exact_mut(p.dw * 4).enumerate() {
        let y = (y0 + ry) as f32 + 0.5 - p.cy;
        for x in 0..p.dw {
            let xx = x as f32 + 0.5 - p.cx;
            let sx = (xx * p.cos - y * p.sin) / p.k + p.sw as f32 * 0.5;
            let sy = (xx * p.sin + y * p.cos) / p.k + p.sh as f32 * 0.5;
            let o = x * 4;
            if sx < 0.0 || sy < 0.0 || sx >= p.sw as f32 || sy >= p.sh as f32 {
                row[o..o + 4].copy_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            // bilinear sample (pixel centres at +0.5)
            let (fx0, fy0) = (sx - 0.5, sy - 0.5);
            let (ix, iy) = (fx0.floor(), fy0.floor());
            let (tx, ty) = (fx0 - ix, fy0 - iy);
            let xa = (ix as i32).clamp(0, sw - 1) as usize;
            let xb = (ix as i32 + 1).clamp(0, sw - 1) as usize;
            let ya = (iy as i32).clamp(0, sh - 1) as usize;
            let yb = (iy as i32 + 1).clamp(0, sh - 1) as usize;
            let at = |xx: usize, yy: usize| (yy * p.sw + xx) * 4;
            let (i00, i10, i01, i11) = (at(xa, ya), at(xb, ya), at(xa, yb), at(xb, yb));
            let mut v = [0f32; 3];
            for (c, vc) in v.iter_mut().enumerate() {
                let top = src[i00 + c] as f32 * (1.0 - tx) + src[i10 + c] as f32 * tx;
                let bot = src[i01 + c] as f32 * (1.0 - tx) + src[i11 + c] as f32 * tx;
                *vc = top * (1.0 - ty) + bot * ty;
            }
            if p.color {
                for c in v.iter_mut() {
                    *c = (*c + p.bright - 128.0) * p.contrast + 128.0;
                }
                let luma = 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
                for c in v.iter_mut() {
                    *c = luma + (*c - luma) * p.sat;
                }
            }
            for c in 0..3 {
                row[o + c] = (v[c] * p.opacity).clamp(0.0, 255.0) as u8;
            }
            row[o + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Fx;

    #[test]
    fn identity_keeps_the_picture() {
        let src: Vec<u8> = (0..4 * 4 * 4).map(|i| (i * 3 % 251) as u8).collect();
        let mut dst = vec![0u8; src.len()];
        render(&src, 4, 4, &Fx::default().eval(0.0), &mut dst, 4, 4);
        for (a, b) in src.chunks_exact(4).zip(dst.chunks_exact(4)) {
            for c in 0..3 {
                assert!((a[c] as i32 - b[c] as i32).abs() <= 1);
            }
        }
    }

    #[test]
    fn zero_opacity_is_black() {
        let src = vec![200u8; 4 * 4 * 4];
        let mut dst = vec![9u8; src.len()];
        let mut fx = Fx::default();
        fx.base[4] = 0.0;
        render(&src, 4, 4, &fx.eval(0.0), &mut dst, 4, 4);
        assert!(dst.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0));
    }
}
