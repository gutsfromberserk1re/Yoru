//! Lightweight point tracking for the preview monitor.
//!
//! This is a deterministic grayscale template matcher, not a neural detector. A small patch is
//! sampled when the user places a point, then searched for in a bounded neighbourhood on each
//! newly presented frame. Only the compact point coordinates are serialized in the project;
//! runtime templates are rebuilt from the current frame after a project is opened.

use std::collections::HashMap;

use crate::media::Frame;
use crate::project::TrackingPoint;

const PATCH_SIDE: i32 = 9;
const PATCH_STEP: i32 = 2;
const SEARCH_RADIUS: i32 = 28;
const SEARCH_STEP: i32 = 2;
const MAX_MEAN_SAD: f32 = 43.0;

#[derive(Clone)]
struct Template {
    x: f32,
    y: f32,
    last_t: f64,
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

#[derive(Default)]
pub struct Runtime {
    templates: HashMap<u32, Template>,
}

fn luma(frame: &Frame, x: i32, y: i32) -> u8 {
    let x = x.clamp(0, frame.w.saturating_sub(1) as i32) as usize;
    let y = y.clamp(0, frame.h.saturating_sub(1) as i32) as usize;
    let i = (y * frame.w + x) * 4;
    let p = &frame.rgba;
    // Integer Rec. 601 luma; alpha is opaque in decoded preview frames.
    ((77u32 * p[i] as u32 + 150u32 * p[i + 1] as u32 + 29u32 * p[i + 2] as u32) >> 8) as u8
}

fn sample_patch(frame: &Frame, x: f32, y: f32) -> Option<Vec<u8>> {
    if frame.w < PATCH_SIDE as usize || frame.h < PATCH_SIDE as usize || frame.rgba.len() < frame.w * frame.h * 4 {
        return None;
    }
    let cx = x.round() as i32;
    let cy = y.round() as i32;
    let half = (PATCH_SIDE / 2) * PATCH_STEP;
    if cx - half < 0 || cy - half < 0 || cx + half >= frame.w as i32 || cy + half >= frame.h as i32 {
        return None;
    }
    let mut out = Vec::with_capacity((PATCH_SIDE * PATCH_SIDE) as usize);
    for py in 0..PATCH_SIDE {
        for px in 0..PATCH_SIDE {
            let sx = cx + (px - PATCH_SIDE / 2) * PATCH_STEP;
            let sy = cy + (py - PATCH_SIDE / 2) * PATCH_STEP;
            out.push(luma(frame, sx, sy));
        }
    }
    Some(out)
}

fn match_patch(frame: &Frame, template: &Template, x: f32, y: f32) -> Option<(f32, f32, f32)> {
    if frame.rgba.len() < frame.w * frame.h * 4 || template.pixels.len() != (PATCH_SIDE * PATCH_SIDE) as usize {
        return None;
    }
    let center_x = x.round() as i32;
    let center_y = y.round() as i32;
    let half = (PATCH_SIDE / 2) * PATCH_STEP;
    let min_x = (center_x - SEARCH_RADIUS).max(half);
    let max_x = (center_x + SEARCH_RADIUS).min(frame.w as i32 - half - 1);
    let min_y = (center_y - SEARCH_RADIUS).max(half);
    let max_y = (center_y + SEARCH_RADIUS).min(frame.h as i32 - half - 1);
    if min_x > max_x || min_y > max_y {
        return None;
    }

    let mut best = (f32::INFINITY, center_x, center_y);
    for cy in (min_y..=max_y).step_by(SEARCH_STEP as usize) {
        for cx in (min_x..=max_x).step_by(SEARCH_STEP as usize) {
            let mut sad = 0u32;
            let mut n = 0usize;
            for py in 0..PATCH_SIDE {
                for px in 0..PATCH_SIDE {
                    let sx = cx + (px - PATCH_SIDE / 2) * PATCH_STEP;
                    let sy = cy + (py - PATCH_SIDE / 2) * PATCH_STEP;
                    let current = luma(frame, sx, sy);
                    let reference = template.pixels[n];
                    sad += current.abs_diff(reference) as u32;
                    n += 1;
                }
            }
            let mean = sad as f32 / n as f32;
            if mean < best.0 {
                best = (mean, cx, cy);
            }
        }
    }
    if best.0 <= MAX_MEAN_SAD {
        Some((best.1 as f32, best.2 as f32, best.0))
    } else {
        None
    }
}

impl Runtime {
    pub fn clear(&mut self) {
        self.templates.clear();
    }

    pub fn seed(&mut self, id: u32, frame: &Frame, nx: f32, ny: f32) {
        let x = nx.clamp(0.0, 1.0) * frame.w.saturating_sub(1) as f32;
        let y = ny.clamp(0.0, 1.0) * frame.h.saturating_sub(1) as f32;
        if let Some(pixels) = sample_patch(frame, x, y) {
            self.templates.insert(id, Template { x, y, last_t: frame.pts, width: frame.w, height: frame.h, pixels });
        }
    }

    /// Track all points belonging to the visible clip through this newly presented frame.
    /// `frame.pts` is the source-media timestamp, matching the timestamp stored in each sample.
    pub fn update_points(&mut self, frame: &Frame, clip_id: u32, points: &mut [TrackingPoint]) -> bool {
        let mut changed = false;
        let mut active = Vec::new();
        for point in points.iter_mut().filter(|p| p.clip_id == clip_id) {
            active.push(point.id);
            let fallback = point.samples.last().map(|s| (s.x, s.y));
            let Some((nx, ny)) = point.position_at(frame.pts).or(fallback) else { continue };

            let last_t = self.templates.get(&point.id).map(|template| template.last_t);
            let dimensions_changed = self.templates.get(&point.id)
                .map_or(false, |template| template.width != frame.w || template.height != frame.h);
            let discontinuity = last_t.map_or(false, |last| frame.pts - last > 0.75);
            if last_t.is_none()
                || dimensions_changed
                || discontinuity
                || last_t.map_or(false, |last| frame.pts + 1e-5 < last)
            {
                self.templates.remove(&point.id);
                self.seed(point.id, frame, nx, ny);
                continue;
            }
            if last_t.map_or(false, |last| frame.pts <= last + 1e-5) {
                continue;
            }

            let tracked = if let Some(template) = self.templates.get_mut(&point.id) {
                let expected_x = template.x.clamp(0.0, frame.w.saturating_sub(1) as f32);
                let expected_y = template.y.clamp(0.0, frame.h.saturating_sub(1) as f32);
                if let Some((x, y, _score)) = match_patch(frame, template, expected_x, expected_y) {
                    if let Some(new_patch) = sample_patch(frame, x, y) {
                        // A little template adaptation tolerates gradual lighting changes while
                        // retaining enough of the original appearance to resist drift.
                        for (old, new) in template.pixels.iter_mut().zip(new_patch) {
                            *old = ((*old as u16 * 4 + new as u16) / 5) as u8;
                        }
                    }
                    template.x = x;
                    template.y = y;
                    template.last_t = frame.pts;
                    template.width = frame.w;
                    template.height = frame.h;
                    Some((x, y))
                } else {
                    // Keep the timestamp current so a temporarily occluded point does not try to
                    // catch up from an increasingly stale frame. The last good coordinates remain.
                    template.last_t = frame.pts;
                    None
                }
            } else {
                None
            };

            if let Some((x, y)) = tracked {
                point.set_sample(frame.pts, x / frame.w.max(1) as f32, y / frame.h.max(1) as f32);
                changed = true;
            }
        }
        self.templates.retain(|id, _| active.contains(id));
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(shift: usize) -> Frame {
        let (w, h) = (96usize, 72usize);
        let mut rgba = vec![24u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) { px[3] = 255; }
        // A high-contrast asymmetric feature, shifted horizontally between frames.
        for y in 25..42 {
            for x in (30 + shift)..(43 + shift) {
                let i = (y * w + x) * 4;
                rgba[i] = if (x + y) % 3 == 0 { 245 } else { 180 };
                rgba[i + 1] = 40;
                rgba[i + 2] = 15;
            }
        }
        Frame {
            pts: shift as f64 / 30.0,
            gen: 0,
            w,
            h,
            rgba,
            decode_ms: 0.0,
            convert_ms: 0.0,
            decode_backend: crate::media::DecodeBackend::Software,
        }
    }

    #[test]
    fn template_follows_a_small_translation() {
        let first = frame(0);
        let second = frame(4);
        let mut point = TrackingPoint { id: 1, clip_id: 7, name: "Point 1".into(), samples: vec![] };
        point.set_sample(first.pts, 36.0 / first.w as f32, 33.0 / first.h as f32);
        let mut runtime = Runtime::default();
        runtime.seed(point.id, &first, 36.0 / first.w as f32, 33.0 / first.h as f32);
        assert!(runtime.update_points(&second, 7, std::slice::from_mut(&mut point)));
        let (x, _) = point.position_at(second.pts).expect("tracked sample");
        assert!(x > 0.38 && x < 0.48, "tracked x should follow the shifted feature, got {x}");
    }
}
