//! CPU compositor for the monitor preview (and, later, export): the source frame is
//! letterbox-fitted into the canvas, then transformed (position, scale, rotation), colour-adjusted
//! (brightness, contrast, saturation) and faded (opacity, over black).
//!
//! Input is the clip's *evaluated* parameter array (`Fx::eval`), in the same units as the
//! Effect Controls panel. Rows are split across threads; every pixel is an inverse-mapped
//! bilinear sample, so rotation and sub-pixel moves stay smooth.

use crate::project::{EffectInstance, EffectKind, NPARAM};

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

/// Renders a source frame after a non-destructive effect stack and standard clip transform.
pub fn render_with_effects(
    src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], effects: &[EffectInstance],
    local_time: f64, clip_duration: f64, dst: &mut [u8], dw: usize, dh: usize,
) {
    let processed = apply_effects(src, sw, sh, effects, local_time, clip_duration);
    let source = processed.as_deref().unwrap_or(src);
    render(source, sw, sh, v, dst, dw, dh);
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

// Effects are evaluated on the decoded preview frame, in stack order, before the clip's
// transform/compositing controls. The kernels are intentionally dependency-free so the Windows
// build does not need a separate GPU or image-processing runtime.
fn apply_effects(src: &[u8], sw: usize, sh: usize, effects: &[EffectInstance], time: f64, duration: f64) -> Option<Vec<u8>> {
    if effects.is_empty() || sw == 0 || sh == 0 || src.len() < sw * sh * 4 { return None; }
    let mut current: Option<Vec<u8>> = None;
    for effect in effects.iter().filter(|e| e.enabled) {
        let input = current.as_deref().unwrap_or(src);
        let p = effect.evaluated_params(time);
        let next = match effect.kind {
            EffectKind::Wave => map_pixels(input, sw, sh, |x, y| {
                let phase = x as f32 / sw.max(1) as f32 * p[1] * std::f32::consts::TAU + time as f32 * p[2] * std::f32::consts::TAU;
                (x, y + phase.sin() * p[0])
            }),
            EffectKind::Ripple => map_pixels(input, sw, sh, |x, y| {
                let dx = x - sw as f32 * 0.5; let dy = y - sh as f32 * 0.5;
                let r = (dx * dx + dy * dy).sqrt().max(1.0);
                let maxr = ((sw * sw + sh * sh) as f32).sqrt() * 0.5;
                let falloff = (1.0 - r / maxr.max(1.0)).clamp(0.0, 1.0);
                let disp = ((r / maxr.max(1.0) * p[1] * std::f32::consts::TAU) - time as f32 * p[2] * std::f32::consts::TAU).sin() * p[0] * falloff;
                (x - dx / r * disp, y - dy / r * disp)
            }),
            EffectKind::Twirl => map_pixels(input, sw, sh, |x, y| {
                let cx = sw as f32 * 0.5; let cy = sh as f32 * 0.5;
                let dx = x - cx; let dy = y - cy; let r = (dx * dx + dy * dy).sqrt();
                let radius = (sw.min(sh) as f32 * p[1] / 100.0 * 0.5).max(1.0);
                if r >= radius { (x, y) } else {
                    let a = p[0].to_radians() * (1.0 - r / radius).powi(2);
                    let (sn, cs) = a.sin_cos(); (cx + dx * cs - dy * sn, cy + dx * sn + dy * cs)
                }
            }),
            EffectKind::GaussianBlur => gaussian_blur(input, sw, sh, p[0]),
            EffectKind::LensBlur => lens_blur(input, sw, sh, p[0], p[1]),
            EffectKind::MotionBlur => motion_blur(input, sw, sh, p[0], p[1]),
            EffectKind::Glow => glow(input, sw, sh, p[0], p[1], p[2]),
            EffectKind::Sharpen => sharpen(input, sw, sh, p[0], p[1]),
            EffectKind::RgbSplit => rgb_split(input, sw, sh, p[0], p[1]),
            EffectKind::Flash => {
                let dur = p[1].max(0.02) as f64; let start = p[2] as f64;
                let u = (time - start) / dur;
                if !(0.0..=1.0).contains(&u) { input.to_vec() } else {
                    let strength = (1.0 - u as f32).powf(2.0) * (p[0] / 100.0).clamp(0.0, 1.0);
                    let mut out = input.to_vec();
                    for px in out.chunks_exact_mut(4) { for c in 0..3 { px[c] = (px[c] as f32 + (255.0 - px[c] as f32) * strength).clamp(0.0, 255.0) as u8; } }
                    out
                }
            },
            EffectKind::FadeIn => scale_luma(input, (time / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32),
            EffectKind::FadeOut => scale_luma(input, ((duration - time) / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32),
        };
        current = Some(next);
    }
    current
}

fn sample(src: &[u8], sw: usize, sh: usize, x: f32, y: f32) -> [u8; 4] {
    let (x, y) = (x.clamp(0.0, sw.saturating_sub(1) as f32), y.clamp(0.0, sh.saturating_sub(1) as f32));
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);
    let ix = |xx, yy| (yy * sw + xx) * 4;
    let (a, b, c, d) = (ix(x0,y0), ix(x1,y0), ix(x0,y1), ix(x1,y1));
    let mut out = [255u8; 4];
    for k in 0..4 {
        let top = src[a+k] as f32 * (1.0-tx) + src[b+k] as f32 * tx;
        let bot = src[c+k] as f32 * (1.0-tx) + src[d+k] as f32 * tx;
        out[k] = (top*(1.0-ty)+bot*ty).round().clamp(0.0,255.0) as u8;
    }
    out
}

fn map_pixels<F: Fn(f32, f32) -> (f32, f32)>(src: &[u8], sw: usize, sh: usize, map: F) -> Vec<u8> {
    let mut out = vec![0; sw * sh * 4];
    for y in 0..sh { for x in 0..sw {
        let (sx, sy) = map(x as f32, y as f32);
        let px = sample(src, sw, sh, sx, sy);
        let at = (y * sw + x) * 4; out[at..at+4].copy_from_slice(&px);
    }}
    out
}

fn box_blur(src: &[u8], sw: usize, sh: usize, radius: usize) -> Vec<u8> {
    if radius == 0 || sw == 0 || sh == 0 { return src.to_vec(); }
    let r = radius.min(64) as isize;
    let mut tmp = vec![0u8; src.len()]; let mut out = vec![0u8; src.len()];
    let den = (2 * r + 1) as f32;
    for y in 0..sh { for c in 0..4 {
        let mut sum = 0u32;
        for k in -r..=r { sum += src[(y*sw + k.clamp(0, sw as isize - 1) as usize)*4+c] as u32; }
        for x in 0..sw {
            tmp[(y*sw+x)*4+c] = (sum as f32 / den).round() as u8;
            let rem = (x as isize-r).clamp(0, sw as isize-1) as usize;
            let add = (x as isize+r+1).clamp(0, sw as isize-1) as usize;
            sum = sum + src[(y*sw+add)*4+c] as u32 - src[(y*sw+rem)*4+c] as u32;
        }
    }}
    for x in 0..sw { for c in 0..4 {
        let mut sum = 0u32;
        for k in -r..=r { sum += tmp[(k.clamp(0, sh as isize-1) as usize*sw+x)*4+c] as u32; }
        for y in 0..sh {
            out[(y*sw+x)*4+c] = (sum as f32/den).round() as u8;
            let rem = (y as isize-r).clamp(0, sh as isize-1) as usize;
            let add = (y as isize+r+1).clamp(0, sh as isize-1) as usize;
            sum = sum + tmp[(add*sw+x)*4+c] as u32 - tmp[(rem*sw+x)*4+c] as u32;
        }
    }}
    out
}

fn gaussian_blur(src: &[u8], sw: usize, sh: usize, radius: f32) -> Vec<u8> {
    let r = radius.round().clamp(0.0, 48.0) as usize;
    if r == 0 { return src.to_vec(); }
    let mut b = src.to_vec(); let br = ((r as f32) * 0.58).round().max(1.0) as usize;
    for _ in 0..3 { b = box_blur(&b, sw, sh, br); }
    b
}

fn lens_blur(src: &[u8], sw: usize, sh: usize, radius: f32, blades: f32) -> Vec<u8> {
    let radius = radius.round().clamp(0.0, 40.0);
    if radius < 0.5 { return src.to_vec(); }
    let blades = blades.round().clamp(3.0, 12.0) as usize;
    let mut out = vec![0u8; src.len()];
    let mut offsets = vec![(0.0f32, 0.0f32)];
    // Two concentric polygonal rings create an iris-shaped defocus kernel.
    for ring in 1..=2 {
        let rr = radius * ring as f32 / 2.0;
        let count = blades * ring;
        for j in 0..count {
            let a = std::f32::consts::TAU * j as f32 / count as f32;
            let sector = std::f32::consts::TAU / blades as f32;
            let local = (a + sector * 0.5).rem_euclid(sector) - sector * 0.5;
            let bound = (std::f32::consts::FRAC_PI_2 / blades as f32).cos() / local.cos().max(0.2);
            offsets.push((a.cos() * rr * bound, a.sin() * rr * bound));
        }
    }
    for y in 0..sh { for x in 0..sw {
        let mut sum = [0.0f32; 4];
        for &(dx,dy) in &offsets {
            let px = sample(src, sw, sh, x as f32 + dx, y as f32 + dy);
            for c in 0..4 { sum[c] += px[c] as f32; }
        }
        let at = (y * sw + x) * 4;
        for c in 0..4 { out[at+c] = (sum[c] / offsets.len() as f32).round().clamp(0.0,255.0) as u8; }
    }}
    out
}

fn motion_blur(src: &[u8], sw: usize, sh: usize, length: f32, angle: f32) -> Vec<u8> {
    if length < 0.5 { return src.to_vec(); }
    let steps = ((length / 3.0).ceil() as usize).clamp(3, 17);
    let theta = angle.to_radians(); let dx = theta.cos() * length * 0.5; let dy = theta.sin() * length * 0.5;
    let mut out = vec![0u8; src.len()];
    for y in 0..sh { for x in 0..sw {
        let mut sum = [0.0f32;4];
        for i in 0..steps { let u = if steps == 1 { 0.5 } else { i as f32 / (steps-1) as f32 };
            let px = sample(src, sw, sh, x as f32 + (u*2.0-1.0)*dx, y as f32 + (u*2.0-1.0)*dy);
            for c in 0..4 { sum[c] += px[c] as f32; }
        }
        let at = (y*sw+x)*4; for c in 0..4 { out[at+c] = (sum[c]/steps as f32).round() as u8; }
    }} out
}

fn glow(src: &[u8], sw: usize, sh: usize, amount: f32, radius: f32, threshold: f32) -> Vec<u8> {
    if amount <= 0.0 || radius <= 0.0 { return src.to_vec(); }
    let mut bright = src.to_vec();
    for px in bright.chunks_exact_mut(4) {
        let luma = 0.2126*px[0] as f32 + 0.7152*px[1] as f32 + 0.0722*px[2] as f32;
        if luma < threshold { px[0]=0; px[1]=0; px[2]=0; }
    }
    let blurred = gaussian_blur(&bright, sw, sh, radius);
    let mut out = src.to_vec();
    for (dst, b) in out.chunks_exact_mut(4).zip(blurred.chunks_exact(4)) {
        for c in 0..3 { dst[c] = (dst[c] as f32 + b[c] as f32 * amount).clamp(0.0,255.0) as u8; }
    }
    out
}

fn sharpen(src: &[u8], sw: usize, sh: usize, amount: f32, radius: f32) -> Vec<u8> {
    if amount <= 0.0 { return src.to_vec(); }
    let blur = gaussian_blur(src, sw, sh, radius.max(1.0)); let mut out = src.to_vec();
    for ((d, s), b) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)).zip(blur.chunks_exact(4)) {
        for c in 0..3 { d[c] = (s[c] as f32 + (s[c] as f32 - b[c] as f32)*amount).clamp(0.0,255.0) as u8; }
    } out
}

fn rgb_split(src: &[u8], sw: usize, sh: usize, amount: f32, angle: f32) -> Vec<u8> {
    if amount <= 0.0 { return src.to_vec(); }
    let (dx,dy) = (angle.to_radians().cos()*amount, angle.to_radians().sin()*amount);
    let mut out = src.to_vec();
    for y in 0..sh { for x in 0..sw {
        let red = sample(src,sw,sh,x as f32+dx,y as f32+dy)[0];
        let blue = sample(src,sw,sh,x as f32-dx,y as f32-dy)[2]; let at=(y*sw+x)*4;
        out[at] = red; out[at+2] = blue;
    }} out
}

fn scale_luma(src: &[u8], amount: f32) -> Vec<u8> {
    let mut out=src.to_vec();
    for px in out.chunks_exact_mut(4) { for c in 0..3 { px[c]=(px[c] as f32*amount).round().clamp(0.0,255.0) as u8; } }
    out
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

    #[test]
    fn fade_in_is_dark_at_the_start_and_effect_stack_renders() {
        let src = vec![200u8; 16 * 16 * 4];
        let fx = Fx::default().eval(0.0);
        let effects = vec![EffectInstance::new(EffectKind::FadeIn, 0.0)];
        let mut dst = vec![0u8; src.len()];
        render_with_effects(&src, 16, 16, &fx, &effects, 0.0, 5.0, &mut dst, 16, 16);
        assert!(dst.chunks_exact(4).all(|px| px[0] == 0 && px[1] == 0 && px[2] == 0));
    }

}
