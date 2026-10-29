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
            EffectKind::Vignette => vignette(input, sw, sh, p[0], p[1]),
            EffectKind::Pixelate => pixelate(input, sw, sh, p[0].round().max(2.0) as usize),
            EffectKind::Sepia => tone_mix(input, 0, p[0], 6.0),
            EffectKind::Grayscale => tone_mix(input, 1, p[0], 6.0),
            EffectKind::Invert => tone_mix(input, 2, p[0], 6.0),
            EffectKind::Posterize => tone_mix(input, 3, p[0], p[0].round().max(2.0)),
            EffectKind::FilmGrain => film_grain(input, sw, sh, p[0], p[1], time),
            EffectKind::Scanlines => scanlines(input, sw, sh, p[0], p[1].round().max(2.0) as usize),
            EffectKind::Mirror => {
                if p[0] < 0.5 { map_pixels(input, sw, sh, |x, y| (sw as f32 - 1.0 - x, y)) }
                else { map_pixels(input, sw, sh, |x, y| (x, sh as f32 - 1.0 - y)) }
            }
            EffectKind::EdgeDetect => edge_detect(input, sw, sh, p[0]),
            EffectKind::ColorTemperature => color_temperature(input, p[0], p[1]),
            EffectKind::VHS => vhs(input, sw, sh, p[0], p[1], time),
        };
        current = Some(next);
    }
    current
}

fn vignette(src: &[u8], sw: usize, sh: usize, strength: f32, radius: f32) -> Vec<u8> {
    if sw == 0 || sh == 0 { return src.to_vec(); }
    let mut out = src.to_vec();
    let (cx, cy) = (sw as f32 * 0.5, sh as f32 * 0.5);
    let norm = (cx * cx + cy * cy).sqrt().max(1.0);
    let threshold = radius.clamp(0.1, 1.0);
    let strength = strength.clamp(0.0, 1.0);
    for y in 0..sh { for x in 0..sw {
        let d = (((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() / norm).clamp(0.0, 1.0);
        let u = ((d - threshold) / (1.0 - threshold).max(0.001)).clamp(0.0, 1.0);
        let smooth = u * u * (3.0 - 2.0 * u);
        let factor = 1.0 - strength * smooth;
        let i = (y * sw + x) * 4;
        for c in 0..3 { out[i+c] = (src[i+c] as f32 * factor).round().clamp(0.0, 255.0) as u8; }
    }}
    out
}

fn pixelate(src: &[u8], sw: usize, sh: usize, block: usize) -> Vec<u8> {
    if sw == 0 || sh == 0 { return src.to_vec(); }
    let block = block.clamp(2, 64);
    let mut out = src.to_vec();
    let mut y0 = 0;
    while y0 < sh {
        let mut x0 = 0;
        let y1 = (y0 + block).min(sh);
        while x0 < sw {
            let x1 = (x0 + block).min(sw);
            let mut sums = [0u64; 4];
            let mut count = 0u64;
            for y in y0..y1 { for x in x0..x1 {
                let i = (y * sw + x) * 4;
                for c in 0..4 { sums[c] += src[i+c] as u64; }
                count += 1;
            }}
            let avg: [u8; 4] = std::array::from_fn(|c| (sums[c] / count.max(1)) as u8);
            for y in y0..y1 { for x in x0..x1 {
                let i = (y * sw + x) * 4;
                out[i..i+4].copy_from_slice(&avg);
            }}
            x0 = x1;
        }
        y0 = y1;
    }
    out
}

/// mode: 0 sepia, 1 grayscale, 2 invert, 3 posterize.
fn tone_mix(src: &[u8], mode: u8, amount: f32, levels: f32) -> Vec<u8> {
    let mut out = src.to_vec();
    let a = (amount / 100.0).clamp(0.0, 1.0);
    let levels = levels.round().clamp(2.0, 32.0);
    for px in out.chunks_exact_mut(4) {
        let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
        let target = match mode {
            0 => [r*0.393 + g*0.769 + b*0.189, r*0.349 + g*0.686 + b*0.168, r*0.272 + g*0.534 + b*0.131],
            1 => { let l = 0.2126*r + 0.7152*g + 0.0722*b; [l, l, l] },
            2 => [255.0-r, 255.0-g, 255.0-b],
            _ => [r, g, b].map(|v| ((v/255.0*(levels-1.0)).round()/(levels-1.0)*255.0)),
        };
        for c in 0..3 { px[c] = (if mode == 3 { target[c] } else { [r,g,b][c]*(1.0-a)+target[c]*a }).round().clamp(0.0,255.0) as u8; }
    }
    out
}

fn film_grain(src: &[u8], sw: usize, sh: usize, amount: f32, size: f32, time: f64) -> Vec<u8> {
    let mut out = src.to_vec();
    let tile = size.round().clamp(1.0, 4.0) as usize;
    let frame = (time.max(0.0) * 24.0).floor() as u64;
    for y in 0..sh { for x in 0..sw {
        let bx = (x / tile) as u64;
        let by = (y / tile) as u64;
        let seed = bx.wrapping_mul(0x9E3779B185EBCA87) ^ by.wrapping_mul(0xC2B2AE3D27D4EB4F) ^ frame.wrapping_mul(0x165667B19E3779F9);
        let hash = seed.wrapping_mul(0x27D4EB2F165667C5) ^ (seed >> 29);
        let noise = (((hash >> 32) & 0xffff) as f32 / 32767.5 - 1.0) * amount.clamp(0.0, 50.0) * 1.25;
        let i = (y * sw + x) * 4;
        for c in 0..3 { out[i+c] = (src[i+c] as f32 + noise).round().clamp(0.0,255.0) as u8; }
    }}
    out
}

fn scanlines(src: &[u8], sw: usize, sh: usize, intensity: f32, spacing: usize) -> Vec<u8> {
    let mut out = src.to_vec();
    let spacing = spacing.clamp(2, 12);
    let factor = 1.0 - intensity.clamp(0.0, 100.0) / 100.0;
    for y in (0..sh).step_by(spacing) { for x in 0..sw {
        let i = (y * sw + x) * 4;
        for c in 0..3 { out[i+c] = (src[i+c] as f32 * factor).round().clamp(0.0,255.0) as u8; }
    }}
    out
}

fn edge_detect(src: &[u8], sw: usize, sh: usize, amount: f32) -> Vec<u8> {
    if sw < 3 || sh < 3 || amount <= 0.0 { return src.to_vec(); }
    let mut out = src.to_vec();
    let mix = (amount / 100.0).clamp(0.0, 1.0);
    let luma = |x: usize, y: usize| {
        let i = (y * sw + x) * 4;
        0.2126 * src[i] as f32 + 0.7152 * src[i + 1] as f32 + 0.0722 * src[i + 2] as f32
    };
    for y in 1..sh-1 { for x in 1..sw-1 {
        let gx = -luma(x-1,y-1) + luma(x+1,y-1) - 2.0*luma(x-1,y) + 2.0*luma(x+1,y) - luma(x-1,y+1) + luma(x+1,y+1);
        let gy = -luma(x-1,y-1) - 2.0*luma(x,y-1) - luma(x+1,y-1) + luma(x-1,y+1) + 2.0*luma(x,y+1) + luma(x+1,y+1);
        let edge = (gx*gx + gy*gy).sqrt().clamp(0.0, 255.0);
        let i = (y * sw + x) * 4;
        for c in 0..3 { out[i+c] = (src[i+c] as f32 * (1.0-mix) + edge * mix).round().clamp(0.0,255.0) as u8; }
    }}
    out
}

fn color_temperature(src: &[u8], temperature: f32, tint: f32) -> Vec<u8> {
    let mut out = src.to_vec();
    let warm = temperature.clamp(-100.0, 100.0) * 0.75;
    let magenta = tint.clamp(-100.0, 100.0) * 0.35;
    for px in out.chunks_exact_mut(4) {
        px[0] = (px[0] as f32 + warm + magenta).round().clamp(0.0,255.0) as u8;
        px[1] = (px[1] as f32 - magenta * 0.65).round().clamp(0.0,255.0) as u8;
        px[2] = (px[2] as f32 - warm + magenta).round().clamp(0.0,255.0) as u8;
    }
    out
}

fn vhs(src: &[u8], sw: usize, sh: usize, amount: f32, tracking: f32, time: f64) -> Vec<u8> {
    if sw == 0 || sh == 0 { return src.to_vec(); }
    let a = (amount.clamp(0.0,100.0) / 100.0);
    let jitter = a * (tracking.clamp(0.0,100.0) * 0.09 + 2.0);
    let chroma = a * 2.0;
    let mut out = src.to_vec();
    for y in 0..sh {
        let row_shift = ((y as f32 * 0.13 + time as f32 * 17.0).sin() * jitter).round();
        let line = if y % 3 == 0 { 1.0 - a * 0.24 } else { 1.0 };
        for x in 0..sw {
            let i = (y * sw + x) * 4;
            let red = sample(src, sw, sh, x as f32 + row_shift + chroma, y as f32)[0] as f32;
            let blue = sample(src, sw, sh, x as f32 + row_shift - chroma, y as f32)[2] as f32;
            out[i] = (red * line).round().clamp(0.0,255.0) as u8;
            out[i+1] = (src[i+1] as f32 * line).round().clamp(0.0,255.0) as u8;
            out[i+2] = (blue * line).round().clamp(0.0,255.0) as u8;
        }
    }
    out
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
    fn every_registered_effect_renders_a_well_sized_frame() {
        let src: Vec<u8> = (0..16 * 12 * 4).map(|i| (i * 17 % 256) as u8).collect();
        for kind in EffectKind::ALL {
            let effect = EffectInstance::new(kind, 0.2);
            let out = apply_effects(&src, 16, 12, &[effect], 0.2, 1.0).expect("one enabled effect produces a frame");
            assert_eq!(out.len(), src.len(), "effect {:?} returned a different frame size", kind);
        }
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
