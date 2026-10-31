//! CPU compositor for the monitor preview (and, later, export): the source frame is
//! letterbox-fitted into the canvas, then transformed (position, scale, rotation), colour-adjusted
//! (brightness, contrast, saturation) and faded (opacity, over black).
//!
//! Input is the clip's *evaluated* parameter array (`Fx::eval`), in the same units as the
//! Effect Controls panel. Rows are split across threads; every pixel is an inverse-mapped
//! bilinear sample, so rotation and sub-pixel moves stay smooth.

use crate::project::{ClipMask, EffectInstance, EffectKind, MaskShape, NPARAM};

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
    // A final Flash/Fade effect is fused into the compositor to avoid a redundant full-frame
    // allocation and image pass on every preview frame.
    fade_factor: f32,
    flash_strength: f32,
    mask: Option<ClipMask>,
}

/// Backwards-compatible render entry point without a clip mask.
pub fn render_with_effects(
    src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], effects: &[EffectInstance],
    local_time: f64, clip_duration: f64, dst: &mut [u8], dw: usize, dh: usize,
) {
    render_with_effects_mask(src, sw, sh, v, effects, None, local_time, clip_duration, dst, dw, dh);
}

/// Renders a source frame after its effects and optional clip-local alpha mask, then applies
/// the standard clip transform. The mask is evaluated in source-image coordinates so it rotates
/// and scales with the footage just like a conventional clip mask.
pub fn render_with_effects_mask(
    src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], effects: &[EffectInstance],
    mask: Option<&ClipMask>, local_time: f64, clip_duration: f64,
    dst: &mut [u8], dw: usize, dh: usize,
) {
    if effects.is_empty() || !effects.iter().any(|e| e.enabled) {
        render_adjusted_mask(src, sw, sh, v, mask.copied(), dst, dw, dh, 1.0, 0.0);
        return;
    }
    // Flash and fades are scalar blends. When one is the last active effect, fuse the blend
    // into the transform sampler. The optional mask is evaluated in the same sampler too, so
    // no full-frame alpha-mask allocation is needed on every preview frame.
    if let Some((last_index, effect)) = effects.iter().enumerate().filter(|(_, e)| e.enabled).last() {
        if matches!(effect.kind, EffectKind::Flash | EffectKind::FadeIn | EffectKind::FadeOut) {
            let processed = apply_effects(src, sw, sh, &effects[..last_index], local_time, clip_duration);
            let source = processed.as_deref().unwrap_or(src);
            let p = effect.evaluated_params(local_time);
            let (fade_factor, flash_strength) = match effect.kind {
                EffectKind::FadeIn => (
                    (local_time / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32,
                    0.0,
                ),
                EffectKind::FadeOut => (
                    ((clip_duration - local_time) / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32,
                    0.0,
                ),
                EffectKind::Flash => {
                    let duration = p[1].max(0.02) as f64;
                    let u = (local_time - p[2] as f64) / duration;
                    let strength = if (0.0..1.0).contains(&u) {
                        (1.0 - u as f32).powi(2) * (p[0] / 100.0).clamp(0.0, 1.0)
                    } else { 0.0 };
                    (1.0, strength)
                }
                _ => unreachable!(),
            };
            render_adjusted_mask(source, sw, sh, v, mask.copied(), dst, dw, dh, fade_factor, flash_strength);
            return;
        }
    }
    let processed = apply_effects(src, sw, sh, effects, local_time, clip_duration);
    let source = processed.as_deref().unwrap_or(src);
    render_adjusted_mask(source, sw, sh, v, mask.copied(), dst, dw, dh, 1.0, 0.0);
}

/// Signed-distance alpha coverage for a clip-local rectangle or ellipse mask.
/// Evaluated at the inverse-mapped source coordinate during resampling (zero extra frame copies).
#[inline]
fn mask_coverage(mask: ClipMask, x: f32, y: f32, sw: usize, sh: usize) -> f32 {
    let (w, h) = (sw as f32, sh as f32);
    let short = w.min(h).max(1.0);
    let cx = w * mask.center_x.clamp(-100.0, 200.0) / 100.0;
    let cy = h * mask.center_y.clamp(-100.0, 200.0) / 100.0;
    let rx = (w * mask.width.clamp(1.0, 200.0) / 200.0).max(0.5);
    let ry = (h * mask.height.clamp(1.0, 200.0) / 200.0).max(0.5);
    let dx = x - cx;
    let dy = y - cy;
    let expansion = short * mask.expansion.clamp(-50.0, 50.0) / 100.0;
    let signed_distance = match mask.shape {
        MaskShape::Rectangle => (rx - dx.abs()).min(ry - dy.abs()) + expansion,
        MaskShape::Ellipse => (1.0 - ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt()) * rx.min(ry) + expansion,
    };
    let feather = short * mask.feather.clamp(0.0, 50.0) / 100.0;
    let mut coverage = if feather <= 0.01 {
        if signed_distance >= 0.0 { 1.0 } else { 0.0 }
    } else {
        let u = ((signed_distance + feather * 0.5) / feather).clamp(0.0, 1.0);
        u * u * (3.0 - 2.0 * u)
    };
    if mask.invert { coverage = 1.0 - coverage; }
    coverage
}

/// Renders `src` (RGBA, `sw`x`sh`) into `dst` (RGBA, `dw`x`dh`).
pub fn render(src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], dst: &mut [u8], dw: usize, dh: usize) {
    render_adjusted_mask(src, sw, sh, v, None, dst, dw, dh, 1.0, 0.0);
}

fn render_adjusted_mask(src: &[u8], sw: usize, sh: usize, v: &[f32; NPARAM], mask: Option<ClipMask>, dst: &mut [u8], dw: usize, dh: usize, fade_factor: f32, flash_strength: f32) {
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
        fade_factor: fade_factor.clamp(0.0, 1.0),
        flash_strength: flash_strength.clamp(0.0, 1.0),
        mask,
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
                row[o..o + 4].copy_from_slice(&[0, 0, 0, 0]);
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
                *vc = (*vc * p.fade_factor + (255.0 - *vc) * p.flash_strength).clamp(0.0, 255.0);
            }
            let alpha_top = src[i00 + 3] as f32 * (1.0 - tx) + src[i10 + 3] as f32 * tx;
            let alpha_bot = src[i01 + 3] as f32 * (1.0 - tx) + src[i11 + 3] as f32 * tx;
            let mut alpha = ((alpha_top * (1.0 - ty) + alpha_bot * ty) * p.opacity).clamp(0.0, 255.0);
            if let Some(mask) = p.mask {
                alpha *= mask_coverage(mask, sx, sy, p.sw, p.sh);
            }
            let alpha_scale = alpha / 255.0;
            if p.color {
                for c in v.iter_mut() {
                    *c = (*c + p.bright - 128.0) * p.contrast + 128.0;
                }
                let luma = 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
                for c in v.iter_mut() {
                    *c = luma + (*c - luma) * p.sat;
                }
            }
            // ColorImage::from_rgba_premultiplied expects RGB multiplied by alpha.
            for c in 0..3 {
                row[o + c] = (v[c] * alpha_scale).clamp(0.0, 255.0) as u8;
            }
            row[o + 3] = alpha as u8;
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
                if !(0.0..1.0).contains(&u) { continue; } else {
                    let strength = (1.0 - u as f32).powf(2.0) * (p[0] / 100.0).clamp(0.0, 1.0);
                    let mut out = input.to_vec();
                    for px in out.chunks_exact_mut(4) { for c in 0..3 { px[c] = (px[c] as f32 + (255.0 - px[c] as f32) * strength).clamp(0.0, 255.0) as u8; } }
                    out
                }
            },
            EffectKind::FadeIn => {
                let factor = (time / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32;
                if factor >= 0.9999 { continue; }
                scale_luma(input, factor)
            }
            EffectKind::FadeOut => {
                let factor = ((duration - time) / p[0].max(0.02) as f64).clamp(0.0, 1.0) as f32;
                if factor >= 0.9999 { continue; }
                scale_luma(input, factor)
            }
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
            EffectKind::ChromaKey => chroma_key(input, p[0], p[1], p[2], p[3]),
            EffectKind::Duotone => duotone(input, p[0], p[1], p[2]),
            EffectKind::Thermal => thermal(input, p[0]),
            EffectKind::Halftone => halftone(input, sw, sh, p[0], p[1], p[2]),
            EffectKind::Solarize => solarize(input, p[0], p[1]),
            EffectKind::Emboss => emboss(input, sw, sh, p[0], p[1]),
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
            _ => [r, g, b].map(|v| (v/255.0*(levels-1.0)).round()/(levels-1.0)*255.0),
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
    let a = amount.clamp(0.0,100.0) / 100.0;
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



fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if (edge1 - edge0).abs() < 1e-5 { return if x >= edge1 { 1.0 } else { 0.0 }; }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn chroma_key(src: &[u8], tolerance: f32, softness: f32, spill: f32, mode: f32) -> Vec<u8> {
    let key = match mode.round() as i32 {
        1 => [0.0f32, 0.0, 255.0],
        2 => [255.0f32, 0.0, 255.0],
        _ => [0.0f32, 255.0, 0.0],
    };
    let mut out = src.to_vec();
    let edge0 = tolerance.clamp(0.0, 255.0);
    let edge1 = edge0 + softness.clamp(0.0, 160.0);
    let spill_mix = spill.clamp(0.0, 100.0) / 100.0;
    for px in out.chunks_exact_mut(4) {
        let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
        let dr = r - key[0]; let dg = g - key[1]; let db = b - key[2];
        // Weight the selected screen channel most strongly so green, blue and magenta
        // keys behave consistently instead of all using green-screen weighting.
        let (wr, wg, wb) = match mode.round() as i32 {
            1 => (0.22, 0.30, 0.48), // blue
            2 => (0.40, 0.20, 0.40), // magenta
            _ => (0.30, 0.48, 0.22), // green
        };
        let distance = (dr * dr * wr + dg * dg * wg + db * db * wb).sqrt();
        let alpha = smoothstep(edge0, edge1.max(edge0 + 1e-4), distance);
        px[3] = (px[3] as f32 * alpha).round().clamp(0.0, 255.0) as u8;
        let suppress = (1.0 - alpha) * spill_mix;
        match mode.round() as i32 {
            1 => { let clean = r.max(g); px[2] = (b * (1.0 - suppress) + clean * suppress).round().clamp(0.0, 255.0) as u8; }
            2 => { let clean = g.max(b); px[0] = (r * (1.0 - suppress) + clean * suppress).round().clamp(0.0, 255.0) as u8; px[2] = (b * (1.0 - suppress) + clean * suppress).round().clamp(0.0, 255.0) as u8; }
            _ => { let clean = r.max(b); px[1] = (g * (1.0 - suppress) + clean * suppress).round().clamp(0.0, 255.0) as u8; }
        }
    }
    out
}

fn hue_color(hue: f32, saturation: f32, value: f32) -> [f32; 3] {
    let h = hue.rem_euclid(360.0) / 60.0;
    let c = value * saturation;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as i32 {
        0 => (c, x, 0.0), 1 => (x, c, 0.0), 2 => (0.0, c, x),
        3 => (0.0, x, c), 4 => (x, 0.0, c), _ => (c, 0.0, x),
    };
    let m = value - c;
    [(r + m) * 255.0, (g + m) * 255.0, (b + m) * 255.0]
}

fn duotone(src: &[u8], shadow_hue: f32, highlight_hue: f32, amount: f32) -> Vec<u8> {
    let shadow = hue_color(shadow_hue, 0.88, 0.24);
    let highlight = hue_color(highlight_hue, 0.62, 1.0);
    let mix = amount.clamp(0.0, 100.0) / 100.0;
    let mut out = src.to_vec();
    for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let luma = (0.2126 * s[0] as f32 + 0.7152 * s[1] as f32 + 0.0722 * s[2] as f32) / 255.0;
        for c in 0..3 {
            let mapped = shadow[c] + (highlight[c] - shadow[c]) * luma;
            d[c] = (s[c] as f32 * (1.0 - mix) + mapped * mix).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn thermal(src: &[u8], amount: f32) -> Vec<u8> {
    let stops = [
        (0.00f32, [4.0, 2.0, 24.0]),
        (0.22, [24.0, 18.0, 120.0]),
        (0.45, [0.0, 125.0, 210.0]),
        (0.66, [230.0, 40.0, 30.0]),
        (0.84, [255.0, 145.0, 10.0]),
        (1.00, [255.0, 250.0, 210.0]),
    ];
    let mix = amount.clamp(0.0, 100.0) / 100.0;
    let mut out = src.to_vec();
    for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let l = (0.2126 * s[0] as f32 + 0.7152 * s[1] as f32 + 0.0722 * s[2] as f32) / 255.0;
        let pair = stops.windows(2).find(|w| l <= w[1].0).unwrap_or(&stops[stops.len()-2..]);
        let t = ((l - pair[0].0) / (pair[1].0 - pair[0].0)).clamp(0.0, 1.0);
        for c in 0..3 {
            let mapped = pair[0].1[c] + (pair[1].1[c] - pair[0].1[c]) * t;
            d[c] = (s[c] as f32 * (1.0 - mix) + mapped * mix).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn halftone(src: &[u8], sw: usize, sh: usize, dot_size: f32, contrast: f32, amount: f32) -> Vec<u8> {
    if sw == 0 || sh == 0 { return src.to_vec(); }
    let cell = dot_size.round().clamp(3.0, 28.0);
    let half = cell * 0.5;
    let mix = amount.clamp(0.0, 100.0) / 100.0;
    let mut out = src.to_vec();
    for y in 0..sh { for x in 0..sw {
        let at = (y * sw + x) * 4;
        let luma = (0.2126 * src[at] as f32 + 0.7152 * src[at+1] as f32 + 0.0722 * src[at+2] as f32) / 255.0;
        let radius = ((1.0 - luma).powf(contrast.clamp(0.4, 3.0)) * half * 1.34).clamp(0.0, half * 1.34);
        let dx = (x as f32 + 0.5).rem_euclid(cell) - half;
        let dy = (y as f32 + 0.5).rem_euclid(cell) - half;
        let ink = if dx * dx + dy * dy <= radius * radius { 18.0 } else { 242.0 };
        for c in 0..3 { out[at+c] = (src[at+c] as f32 * (1.0 - mix) + ink * mix).round().clamp(0.0, 255.0) as u8; }
    }}
    out
}

fn solarize(src: &[u8], threshold: f32, amount: f32) -> Vec<u8> {
    let threshold = threshold.clamp(0.0, 255.0);
    let mix = amount.clamp(0.0, 100.0) / 100.0;
    let mut out = src.to_vec();
    for px in out.chunks_exact_mut(4) {
        for c in 0..3 {
            let old = px[c] as f32;
            let target = if old >= threshold { 255.0 - old } else { old };
            px[c] = (old * (1.0 - mix) + target * mix).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn emboss(src: &[u8], sw: usize, sh: usize, amount: f32, angle: f32) -> Vec<u8> {
    if sw == 0 || sh == 0 { return src.to_vec(); }
    let theta = angle.to_radians();
    let (dx, dy) = (theta.cos().round() as isize, theta.sin().round() as isize);
    let mix = amount.clamp(0.0, 100.0) / 100.0;
    let mut out = src.to_vec();
    for y in 0..sh { for x in 0..sw {
        let ax = (x as isize - dx).clamp(0, sw as isize - 1) as usize;
        let ay = (y as isize - dy).clamp(0, sh as isize - 1) as usize;
        let bx = (x as isize + dx).clamp(0, sw as isize - 1) as usize;
        let by = (y as isize + dy).clamp(0, sh as isize - 1) as usize;
        let ia = (ay * sw + ax) * 4; let ib = (by * sw + bx) * 4; let at = (y * sw + x) * 4;
        let la = 0.2126*src[ia] as f32 + 0.7152*src[ia+1] as f32 + 0.0722*src[ia+2] as f32;
        let lb = 0.2126*src[ib] as f32 + 0.7152*src[ib+1] as f32 + 0.0722*src[ib+2] as f32;
        let shade = (128.0 + (la - lb) * 1.8).clamp(0.0, 255.0);
        for c in 0..3 { out[at+c] = (src[at+c] as f32 * (1.0 - mix) + shade * mix).round().clamp(0.0,255.0) as u8; }
    }}
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
    fn clip_mask_supports_feather_and_inversion() {
        let mut mask = ClipMask { width: 50.0, height: 50.0, ..ClipMask::default() };
        assert!(mask_coverage(mask, 50.0, 50.0, 100, 100) > 0.99);
        assert!(mask_coverage(mask, 5.0, 50.0, 100, 100) < 0.01);
        mask.feather = 10.0;
        assert!((mask_coverage(mask, 75.0, 50.0, 100, 100) - 0.5).abs() < 0.02);
        mask.invert = true;
        assert!(mask_coverage(mask, 5.0, 50.0, 100, 100) > 0.99);
    }

    #[test]
    fn identity_keeps_the_picture() {
        let mut src: Vec<u8> = (0..4 * 4 * 4).map(|i| (i * 3 % 251) as u8).collect();
        for px in src.chunks_exact_mut(4) { px[3] = 255; }
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
    fn chroma_key_makes_green_transparent_and_keeps_subject_pixels() {
        let src = vec![0, 255, 0, 255, 220, 35, 25, 255];
        let out = chroma_key(&src, 108.0, 72.0, 55.0, 0.0);
        assert_eq!(out[3], 0);
        assert_eq!(out[7], 255);
    }

    #[test]
    fn compositor_preserves_alpha_from_keyed_frames() {
        let src = vec![120u8, 80, 40, 96];
        let mut dst = vec![0u8; 4];
        render(&src, 1, 1, &Fx::default().eval(0.0), &mut dst, 1, 1);
        assert!((dst[3] as i32 - 96).abs() <= 1);
        assert!(dst[0] <= src[0]);
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
