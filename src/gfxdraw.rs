//! Draws titles and shapes over the monitor picture (egui, GPU) in 1920x1080 canvas space.
//! Position / scale / rotation / opacity come from the clip's evaluated effect values, so
//! keyframed moves and fades on a title just work.

use super::*;
use crate::project::{Gfx, GfxKind, NPARAM};
use eframe::egui::epaint::TextShape;

pub struct Overlay {
    pub gfx: Gfx,
    pub v: [f32; NPARAM], // Fx::eval at the playhead
}

fn rot(v: Vec2, a: f32) -> Vec2 {
    let (s, c) = a.sin_cos();
    vec2(v.x * c - v.y * s, v.x * s + v.y * c)
}

fn tint(c: [u8; 4], op: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], (c[3] as f32 * op).round().clamp(0.0, 255.0) as u8)
}

pub fn draw_overlays(p: &Painter, canvas: Rect, items: &[Overlay]) {
    let p = p.with_clip_rect(canvas);
    let k = canvas.height() / 1080.0;
    for o in items {
        draw_one(&p, canvas, k, o);
    }
}

fn draw_one(p: &Painter, canvas: Rect, k: f32, o: &Overlay) {
    let v = &o.v;
    let centre = canvas.center() + vec2((v[0] - 960.0) * k, (v[1] - 540.0) * k);
    let s = k * (v[2] / 100.0).max(0.0);
    let ang = v[3].to_radians();
    let op = (v[4] / 100.0).clamp(0.0, 1.0);
    if op <= 0.0 || s <= 0.0 {
        return;
    }
    let g = &o.gfx;
    let quad = |hw: f32, hh: f32| -> Vec<Pos2> {
        [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)].iter().map(|&(x, y)| centre + rot(vec2(x, y), ang)).collect()
    };

    match g.kind {
        GfxKind::Rect | GfxKind::Ellipse => {
            let (hw, hh) = (g.w * 0.5 * s, g.h * 0.5 * s);
            let pts: Vec<Pos2> = if g.kind == GfxKind::Rect {
                quad(hw, hh)
            } else {
                (0..64)
                    .map(|i| {
                        let a = i as f32 / 64.0 * std::f32::consts::TAU;
                        centre + rot(vec2(a.cos() * hw, a.sin() * hh), ang)
                    })
                    .collect()
            };
            let stroke = if g.outline > 0.0 { Stroke::new(g.outline * s, tint(g.outline_color, op)) } else { Stroke::NONE };
            p.add(Shape::convex_polygon(pts, tint(g.color, op), stroke));
        }
        GfxKind::Text => {
            if g.text.trim().is_empty() {
                return;
            }
            let font = if g.mono { FontId::monospace(g.size * s) } else { FontId::proportional(g.size * s) };
            let lines: Vec<_> = g
                .text
                .split('\n')
                .map(|l| p.layout_no_wrap(if l.is_empty() { " ".to_string() } else { l.to_string() }, font.clone(), Color32::PLACEHOLDER))
                .collect();
            let bw = lines.iter().map(|l| l.size().x).fold(0.0, f32::max);
            let bh: f32 = lines.iter().map(|l| l.size().y).sum();
            let pad = g.pad * s;
            if g.bg[3] > 0 {
                p.add(Shape::convex_polygon(quad(bw * 0.5 + pad, bh * 0.5 + pad), tint(g.bg, op), Stroke::NONE));
            }
            let mut y = -bh * 0.5;
            for line in &lines {
                let (w, h) = (line.size().x, line.size().y);
                let x = match g.align {
                    0 => -bw * 0.5,
                    2 => bw * 0.5 - w,
                    _ => -w * 0.5,
                };
                // rotating the line's top-left about the block centre, and the text about that corner
                let at = |dx: f32, dy: f32, col: Color32| {
                    let mut t = TextShape::new(centre + rot(vec2(x + dx, y + dy), ang), line.clone(), col);
                    t.angle = ang;
                    Shape::Text(t)
                };
                if g.shadow {
                    p.add(at(3.0 * s, 3.0 * s, tint([0, 0, 0, 170], op)));
                }
                if g.outline > 0.0 {
                    let r = g.outline * s;
                    for (dx, dy) in [(-1.0, -1.0), (0.0, -1.0), (1.0, -1.0), (-1.0, 0.0), (1.0, 0.0), (-1.0, 1.0), (0.0, 1.0), (1.0, 1.0)] {
                        p.add(at(dx * r, dy * r, tint(g.outline_color, op)));
                    }
                }
                p.add(at(0.0, 0.0, tint(g.color, op)));
                y += h;
            }
        }
    }
}
