//! Yoru - video editor (Rust / egui)
//!
//! * Panels: solid French Blue (#394988), rounded, 1px border.
//! * Everything else (title bar, gaps between panels, window background):
//!   transparent frosted-glass (Windows acrylic blur, Windows 11 rounded corners).
//!
//! Run:  cargo run --release

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]
#![allow(unknown_lints, float_literal_f32_fallback, deprecated)]

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, ecolor::Hsva, *};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod assets;
mod engine;
mod fxrender;
mod gfxdraw;
mod fxui;
mod media;
mod project;
mod timeline;

use assets::Assets;
use project::{Clip, Lane, Project, NPARAM, PARAMS};
use timeline::{timeline_panel, Ghost, Timeline, Tool};

// ------------------------------------------------------------------ settings
const PROJECT_NAME: &str = "Untitled";
const FPS: f64 = 30.0;
const DEFAULT_DURATION: f64 = 111.0;

/// Project length in seconds. Global for now; the real project model replaces this next step.
static DURATION_BITS: AtomicU64 = AtomicU64::new(0);
fn duration() -> f64 {
    match DURATION_BITS.load(AtomicOrdering::Relaxed) {
        0 => DEFAULT_DURATION,
        b => f64::from_bits(b),
    }
}
fn set_duration(d: f64) {
    DURATION_BITS.store(d.max(1.0).to_bits(), AtomicOrdering::Relaxed);
}

/// Linear peak (0..1+) -> meter fill (0..1), matching the dB ticks drawn beside the meter.
fn peak_to_meter(peak: f32) -> f32 {
    const PTS: [(f32, f32); 5] = [(-60.0, 0.0), (-30.0, 0.38), (-12.0, 0.68), (0.0, 0.9), (3.0, 1.0)];
    let db = 20.0 * peak.max(1e-6).log10();
    if db <= PTS[0].0 {
        return 0.0;
    }
    for w in PTS.windows(2) {
        if db <= w[1].0 {
            return w[0].1 + (db - w[0].0) / (w[1].0 - w[0].0) * (w[1].1 - w[0].1);
        }
    }
    1.0
}
const WINDOW_RADIUS: f32 = 12.0;

/// Acrylic tint (R, G, B, A). Lower alpha = more see-through glass.
const GLASS_TINT: (u8, u8, u8, u8) = (0x35, 0x3A, 0x4C, 110);

// ------------------------------------------------------------------ palette
const POWDER: Color32 = Color32::from_rgb(0xF0, 0xF6, 0xFC);
const VISTA: Color32 = Color32::from_rgb(0xC9, 0xD1, 0xD9);
const GLAUCOUS: Color32 = Color32::from_rgb(0x8B, 0x94, 0x9E);
const TEXT: Color32 = POWDER;
const MUTED: Color32 = GLAUCOUS;
const DEEP: Color32 = Color32::from_rgb(0x22, 0x25, 0x30);
/// Panel glass: very dark navy, ~84% opaque (premultiplied RGBA; lower the last number for more see-through).
const PANEL_BG: Color32 = Color32::from_rgba_premultiplied(6, 13, 39, 215);
const DOT_COLORS: [Color32; 3] = [
    Color32::from_rgb(0x48, 0x52, 0x66),
    Color32::from_rgb(0x5E, 0x6B, 0x82),
    Color32::from_rgb(0x8A, 0x99, 0xB3),
];

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}
fn panel_border() -> Color32 {
    rgba(80, 100, 160, 170)
}

fn fmt_tc(t: f64) -> String {
    let frames = (t.max(0.0) * FPS).round() as u64;
    let f = frames % FPS as u64;
    let s = frames / FPS as u64;
    format!("{:02}:{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60, f)
}

fn thumb_hue(seed: u32, i: u32) -> u32 {
    210 + (seed * 19 + i * 11) % 40
}

fn hsv(h: u32, s: f32, v: f32) -> Color32 {
    Hsva::new(h as f32 / 360.0, s / 255.0, v / 255.0, 1.0).into()
}

/// Precomputed fake waveform (20 samples / second).
fn wave() -> &'static [f32] {
    static W: OnceLock<Vec<f32>> = OnceLock::new();
    W.get_or_init(|| {
        (0..(DEFAULT_DURATION as usize * 20 + 40))
            .map(|i| {
                let x = i as f32 / 20.0;
                let env = ((x * 0.35).sin() + 1.0) / 2.0;
                let v = ((x * 7.3).sin() * 0.5 + (x * 2.1).sin() * 0.3 + (x * 13.7).sin() * 0.2).abs();
                (v * (0.25 + 0.75 * env) * 1.2).min(1.0)
            })
            .collect()
    })
}

// ------------------------------------------------------------------ drawing helpers
fn push_rect_gradient(m: &mut Mesh, r: Rect, top: Color32, bot: Color32) {
    let i = m.vertices.len() as u32;
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.right_bottom(), bot);
    m.colored_vertex(r.left_bottom(), bot);
    m.add_triangle(i, i + 1, i + 2);
    m.add_triangle(i, i + 2, i + 3);
}

/// Rounded rect with a vertical gradient (single triangle-fan mesh).
fn rounded_gradient(p: &Painter, rect: Rect, r: f32, top: Color32, bot: Color32) {
    let col = |y: f32| top.lerp_to_gamma(bot, ((y - rect.top()) / rect.height()).clamp(0.0, 1.0));
    let corners = [
        (rect.right() - r, rect.top() + r, -90.0f32),
        (rect.right() - r, rect.bottom() - r, 0.0),
        (rect.left() + r, rect.bottom() - r, 90.0),
        (rect.left() + r, rect.top() + r, 180.0),
    ];
    let mut m = Mesh::default();
    m.colored_vertex(rect.center(), col(rect.center().y));
    const SEG: usize = 4;
    for (cx, cy, a0) in corners {
        for i in 0..=SEG {
            let a = (a0 + 90.0 * i as f32 / SEG as f32).to_radians();
            let pt = pos2(cx + r * a.cos(), cy + r * a.sin());
            m.colored_vertex(pt, col(pt.y));
        }
    }
    let n = m.vertices.len() as u32 - 1;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    p.add(Shape::mesh(m));
}

fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap.max_width = w.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.fonts(|f| f.layout_job(job))
}

// ------------------------------------------------------------------ icons
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ico {
    Select,
    Ripple,
    Razor,
    Slip,
    Rect,
    Hand,
    Zoom,
    Play,
    Pause,
}

fn draw_icon(p: &Painter, k: Ico, center: Pos2, size: f32, c: Color32) {
    let s = size / 18.0;
    let o = center - vec2(size / 2.0, size / 2.0);
    let pt = |x: f32, y: f32| pos2(o.x + x * s, o.y + y * s);
    let st = Stroke::new(1.6 * s, c);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
    };
    let fill = |pts: &[(f32, f32)]| {
        p.add(Shape::convex_polygon(
            pts.iter().map(|&(x, y)| pt(x, y)).collect(),
            c,
            Stroke::NONE,
        ));
    };
    let bar = |x: f32, y: f32, w: f32, h: f32| {
        p.rect_filled(Rect::from_min_size(pt(x, y), vec2(w, h) * s), 1.2 * s, c);
    };
    match k {
        Ico::Select => {
            // concave arrow, split into convex pieces
            fill(&[(4.0, 2.0), (4.0, 14.5), (7.4, 11.2)]);
            fill(&[(4.0, 2.0), (7.4, 11.2), (9.7, 10.4)]);
            fill(&[(4.0, 2.0), (9.7, 10.4), (14.2, 10.0)]);
            fill(&[(7.4, 11.2), (9.8, 16.0), (12.0, 15.0), (9.7, 10.4)]);
        }
        Ico::Ripple => {
            line(&[(9.0, 3.0), (9.0, 15.0)]);
            line(&[(6.0, 6.0), (3.0, 9.0), (6.0, 12.0)]);
            line(&[(12.0, 6.0), (15.0, 9.0), (12.0, 12.0)]);
        }
        Ico::Razor => fill(&[(3.0, 13.0), (11.0, 2.5), (15.0, 5.5), (7.0, 15.5)]),
        Ico::Slip => {
            line(&[(4.5, 3.0), (4.5, 15.0)]);
            line(&[(13.5, 3.0), (13.5, 15.0)]);
            line(&[(7.0, 9.0), (11.0, 9.0)]);
            line(&[(8.5, 7.3), (7.0, 9.0), (8.5, 10.7)]);
            line(&[(9.5, 7.3), (11.0, 9.0), (9.5, 10.7)]);
        }
        Ico::Rect => {
            p.rect_stroke(Rect::from_min_size(pt(3.0, 4.5), vec2(12.0, 9.0) * s), 2.0 * s, st);
        }
        Ico::Hand => {
            p.rect_stroke(Rect::from_min_size(pt(5.0, 8.0), vec2(8.0, 7.0) * s), 2.5 * s, st);
            line(&[(6.5, 8.0), (6.5, 4.5)]);
            line(&[(8.5, 8.0), (8.5, 3.0)]);
            line(&[(10.5, 8.0), (10.5, 3.5)]);
            line(&[(12.5, 8.0), (12.5, 5.0)]);
        }
        Ico::Zoom => {
            p.circle_stroke(pt(7.25, 7.25), 4.25 * s, st);
            line(&[(10.0, 10.0), (15.0, 15.0)]);
        }
        Ico::Play => fill(&[(5.0, 2.5), (5.0, 15.5), (15.0, 9.0)]),
        Ico::Pause => {
            bar(4.5, 3.0, 3.0, 12.0);
            bar(10.5, 3.0, 3.0, 12.0);
        }
    }
}

fn paint_icon_btn(ui: &Ui, r: &Response, k: Ico, checked: bool, isz: f32) {
    let bg = if checked {
        rgba(201, 209, 217, 56)
    } else if r.hovered() {
        rgba(139, 148, 158, 41)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(r.rect, 6.0, bg);
    draw_icon(ui.painter(), k, r.rect.center(), isz, if checked { POWDER } else { TEXT });
}

// ------------------------------------------------------------------ small widgets
fn glass(l: f32, t: f32, r: f32, b: f32) -> Frame {
    Frame::none()
        .fill(PANEL_BG)
        .stroke(Stroke::new(1.0, panel_border()))
        .rounding(8.0)
        .inner_margin(Margin { left: l, right: r, top: t, bottom: b })
}

fn tab_strip(ui: &mut Ui, idx: &mut usize, names: &[&str]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, n) in names.iter().enumerate() {
            let font = FontId::monospace(11.0);
            let w = ui.painter().layout_no_wrap(n.to_string(), font.clone(), MUTED).size().x + 24.0;
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
            let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
            let on = *idx == i;
            let col = if on || resp.hovered() { TEXT } else { MUTED };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, n, font, col);
            if on {
                ui.painter()
                    .line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(2.0, POWDER));
            }
            if resp.clicked() {
                *idx = i;
            }
        }
    });
}

fn slider(ui: &mut Ui, val: &mut f32, lo: f32, hi: f32, width: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 16.0), Sense::click_and_drag());
    if resp.is_pointer_button_down_on() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let f = ((pos.x - rect.left() - 6.0) / (rect.width() - 12.0)).clamp(0.0, 1.0);
            *val = (lo + (hi - lo) * f).round();
        }
    }
    let f = (*val - lo) / (hi - lo);
    let x = rect.left() + 6.0 + (rect.width() - 12.0) * f;
    let cy = rect.center().y;
    let p = ui.painter();
    p.rect_filled(
        Rect::from_min_max(pos2(rect.left(), cy - 2.0), pos2(rect.right(), cy + 2.0)),
        2.0,
        rgba(139, 148, 158, 56),
    );
    p.rect_filled(
        Rect::from_min_max(pos2(rect.left(), cy - 2.0), pos2(x, cy + 2.0)),
        2.0,
        POWDER,
    );
    p.circle_filled(pos2(x, cy), 6.0, TEXT);
}

/// Monitor picture for one frame: the raw video when no effect is active (and it is 16:9),
/// otherwise the frame composited into a 16:9 canvas with the clip's effects applied.
fn preview_image(f: &media::Frame, params: &[f32; NPARAM]) -> (ColorImage, f32) {
    let asp = f.w as f32 / f.h.max(1) as f32;
    let neutral = (0..NPARAM).all(|i| (params[i] - PARAMS[i].def).abs() < 1e-4);
    if neutral && (asp - 16.0 / 9.0).abs() < 0.02 {
        // alpha is always 255 after the RGBA conversion, so premultiplied == straight
        return (ColorImage::from_rgba_premultiplied([f.w, f.h], &f.rgba), asp);
    }
    let cw = f.w.max(320) & !1;
    let ch = (cw * 9 / 16) & !1;
    let mut buf = vec![0u8; cw * ch * 4];
    fxrender::render(&f.rgba, f.w, f.h, params, &mut buf, cw, ch);
    (ColorImage::from_rgba_premultiplied([cw, ch], &buf), cw as f32 / ch as f32)
}

fn level_meter(ui: &Ui, rect: Rect, l: f32, r: f32) {
    let p = ui.painter_at(rect);
    let (top, bottom) = (rect.top() + 30.0, rect.bottom() - 8.0);
    let span = bottom - top;
    let bx = rect.left() + rect.width() - 16.0;
    for (label, f) in [("0", 0.1), ("-12", 0.32), ("-30", 0.62), ("-60", 1.0)] {
        p.text(
            pos2(bx - 6.0, top + span * f),
            Align2::RIGHT_CENTER,
            label,
            FontId::monospace(7.0),
            rgba(139, 148, 158, 170),
        );
    }
    for (i, lv) in [l, r].into_iter().enumerate() {
        let x = bx + i as f32 * 8.0;
        p.rect_filled(Rect::from_min_size(pos2(x, top), vec2(5.0, span)), 2.0, rgba(255, 255, 255, 12));
        let fh = span * lv.clamp(0.0, 1.0);
        if fh > 0.5 {
            let y0 = bottom - fh;
            let col = |y: f32| TEXT.lerp_to_gamma(GLAUCOUS, ((y - top) / span).clamp(0.0, 1.0));
            let mut m = Mesh::default();
            push_rect_gradient(
                &mut m,
                Rect::from_min_max(pos2(x, y0), pos2(x + 5.0, bottom)),
                col(y0),
                col(bottom),
            );
            p.add(Shape::mesh(m));
        }
    }
}

// ------------------------------------------------------------------ panels
const FITS: [&str; 4] = ["Fit", "25%", "50%", "100%"];
/// Playback resolution: decode + preview at full / half / quarter size (smoother on heavy footage).
const QUALITIES: [(&str, u32); 3] = [("Full", 1), ("1/2", 2), ("1/4", 4)];

fn monitor_panel(
    ui: &mut Ui,
    project: &str,
    t: f64,
    playing: bool,
    fit: &mut usize,
    quality: &mut usize,
    video: Option<(TextureId, f32)>,
    overlays: &[gfxdraw::Overlay],
) -> bool {
    let mut play = false;
    glass(14.0, 8.0, 14.0, 10.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.horizontal(|ui| {
            ui.label(RichText::new(project).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ComboBox::from_id_salt("fit").selected_text(FITS[*fit]).show_ui(ui, |ui| {
                    for (i, f) in FITS.iter().enumerate() {
                        ui.selectable_value(fit, i, *f);
                    }
                });
                ComboBox::from_id_salt("quality").selected_text(QUALITIES[*quality].0).show_ui(ui, |ui| {
                    for (i, (name, _)) in QUALITIES.iter().enumerate() {
                        ui.selectable_value(quality, i, *name);
                    }
                });
                ui.label(RichText::new("Playback").size(10.0).color(MUTED));
            });
        });

        let bar_h = 34.0;
        let surf_h = (ui.available_height() - bar_h - 6.0).max(90.0);
        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), surf_h), Sense::hover());
        let asp = video.map_or(16.0 / 9.0, |v| v.1.max(0.1));
        let aw = area.width().min(area.height() * asp);
        let r = Rect::from_center_size(area.center(), vec2(aw, aw / asp));
        if let Some((id, _)) = video {
            Image::new(load::SizedTexture::new(id, r.size())).rounding(6.0).paint_at(ui, r);
        } else {
            rounded_gradient(ui.painter(), r, 6.0, rgba(0x22, 0x28, 0x38, 255), rgba(0x17, 0x1A, 0x24, 255));
            ui.painter().text(
                r.center(),
                Align2::CENTER_CENTER,
                "Drop a video file here",
                FontId::monospace(10.0),
                rgba(139, 148, 158, 150),
            );
        }
        ui.painter().rect_stroke(r, 6.0, Stroke::new(1.0, rgba(110, 125, 170, 120)));
        gfxdraw::draw_overlays(ui.painter(), r, overlays);

        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), bar_h), Sense::hover());
        let s = fmt_tc(t);
        let mut job = LayoutJob::default();
        job.append(&s[..6], 0.0, TextFormat { font_id: FontId::monospace(16.0), color: MUTED, ..Default::default() });
        job.append(&s[6..], 0.0, TextFormat { font_id: FontId::monospace(16.0), color: TEXT, ..Default::default() });
        let g = ui.fonts(|f| f.layout_job(job));
        let total = 34.0 + 10.0 + g.size().x;
        let x0 = bar.center().x - total / 2.0;
        let br = Rect::from_min_size(pos2(x0, bar.top() + 2.0), vec2(34.0, 30.0));
        let resp = ui.interact(br, ui.id().with("play"), Sense::click());
        paint_icon_btn(ui, &resp, if playing { Ico::Pause } else { Ico::Play }, false, 20.0);
        if resp.clicked() {
            play = true;
        }
        ui.painter()
            .galley(pos2(x0 + 44.0, bar.center().y - g.size().y / 2.0), g, TEXT);
    });
    play
}

fn placeholder(ui: &mut Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new(text).color(MUTED).size(13.0));
    });
}

#[allow(dead_code)]
const CLIP_NAMES: [&str; 10] = [
    "Cabana", "Mountain gold", "Jungle forest", "Hammock", "Waterfall", "Beach", "Pool", "Rocks", "Trail", "Canopy",
];
const EFFECTS: [(&str, &[&str]); 6] = [
    ("Presets", &["Blurs", "Distort", "Lens Flares"]),
    ("Lumetri Presets", &["Cinematic", "Technicolor", "Monochrome"]),
    ("Audio Effects", &["Reverb", "Parametric EQ", "Compressor"]),
    ("Audio Transitions", &["Constant Power", "Cross Fade"]),
    ("Video Effects", &["Color Correction", "Blur & Sharpen", "Transform", "Stylize"]),
    ("Video Transitions", &["Cross Dissolve", "Dip to Black", "Wipe", "Push"]),
];

struct Browser {
    tab: usize,
    sel: usize,
    view: usize, // 0 = thumbnail grid, 1 = compact list
    search: String,
    fx_sel: Option<&'static str>,
}

/// Returns the media id the user asked to add to the timeline (double-click).
fn browser_panel(
    ui: &mut Ui,
    b: &mut Browser,
    proj: &Project,
    assets: &Assets,
    drag: &mut Option<u32>,
    import_requested: &mut bool,
) -> Option<u32> {
    let mut add = None;
    glass(12.0, 8.0, 12.0, 12.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 8.0;
        // The Project bin owns the imported media pool. Grid/list are two views of the same
        // assets, not separate panels; Effects remains its own tab.
        tab_strip(ui, &mut b.tab, &["Project", "Effects"]);
        match b.tab {
            0 => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Project bin").size(10.0).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.selectable_label(b.view == 1, "List").clicked() { b.view = 1; }
                        if ui.selectable_label(b.view == 0, "Grid").clicked() { b.view = 0; }
                        if ui.button("+ Import").on_hover_text("Import video files into the Project bin").clicked() {
                            *import_requested = true;
                        }
                    });
                });
                add = if b.view == 0 {
                    media_grid(ui, proj, assets, &mut b.sel, drag)
                } else {
                    media_list(ui, proj, &mut b.sel, drag)
                };
            }
            _ => {
                ui.add(
                    TextEdit::singleline(&mut b.search)
                        .hint_text("Search effects")
                        .desired_width(f32::INFINITY)
                        .margin(vec2(10.0, 5.0)),
                );
                let q = b.search.trim().to_lowercase();
                ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                    for (folder, kids) in EFFECTS {
                        let vis: Vec<&'static str> = kids
                            .iter()
                            .copied()
                            .filter(|k| q.is_empty() || k.to_lowercase().contains(&q))
                            .collect();
                        if vis.is_empty() {
                            continue;
                        }
                        let mut h = CollapsingHeader::new(folder);
                        if !q.is_empty() {
                            h = h.open(Some(true));
                        }
                        h.show(ui, |ui| {
                            for k in vis {
                                if ui.selectable_label(b.fx_sel == Some(k), k).clicked() {
                                    b.fx_sel = Some(k);
                                }
                            }
                        });
                    }
                });
            }
        }
    });
    add
}

fn fmt_len(sec: f64) -> String {
    let s = sec.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The Clips tab: one card per imported file with its real thumbnail. Click selects,
/// double-click appends to the timeline, dragging it onto a track places it there.
fn media_grid(ui: &mut Ui, proj: &Project, assets: &Assets, sel: &mut usize, drag: &mut Option<u32>) -> Option<u32> {
    if proj.media.is_empty() {
        placeholder(ui, "Click + Import, or drop video files onto the window.");
        return None;
    }
    let mut add = None;
    ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        let gap = 8.0;
        let cell_w = ((ui.available_width() - gap) / 2.0).floor().max(60.0);
        let thumb_h = (cell_w * 9.0 / 16.0).floor();
        let cell_h = thumb_h + 22.0;
        let rows = proj.media.len().div_ceil(2) as f32;
        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), rows * (cell_h + gap)), Sense::hover());
        for (i, m) in proj.media.iter().enumerate() {
            let x = area.left() + (i % 2) as f32 * (cell_w + gap);
            let y = area.top() + (i / 2) as f32 * (cell_h + gap);
            let cell = Rect::from_min_size(pos2(x, y), vec2(cell_w, cell_h));
            let tr = Rect::from_min_size(cell.min, vec2(cell_w, thumb_h));
            let resp = ui.interact(cell, ui.id().with(("media_cell", m.id)), Sense::click_and_drag());
            let p = ui.painter();
            match assets.thumb_at(m.id, 0.0) {
                Some((tex, _)) => {
                    p.image(tex.id(), tr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => rounded_gradient(p, tr, 4.0, hsv(thumb_hue(m.seed, 0), 35.0, 55.0), hsv(thumb_hue(m.seed, 0), 45.0, 25.0)),
            }
            let on = *sel == i;
            p.rect_stroke(tr, 4.0, Stroke::new(if on { 2.0 } else { 1.0 }, if on { TEXT } else { rgba(110, 125, 170, 120) }));
            if m.duration > 0.0 {
                let badge = Rect::from_min_size(pos2(tr.right() - 44.0, tr.bottom() - 17.0), vec2(40.0, 13.0));
                p.rect_filled(badge, 3.0, rgba(0, 0, 0, 150));
                p.text(badge.center(), Align2::CENTER_CENTER, fmt_len(m.duration), FontId::monospace(8.0), TEXT);
            }
            let g = elided(ui, &m.name, FontId::monospace(9.0), if on { TEXT } else { MUTED }, cell_w);
            p.galley(pos2(cell.left(), tr.bottom() + 4.0), g, MUTED);
            if resp.clicked() {
                *sel = i;
            }
            if resp.double_clicked() {
                add = Some(m.id);
            }
            if resp.drag_started() {
                *sel = i;
                *drag = Some(m.id);
            }
        }
    });
    add
}

fn media_list(ui: &mut Ui, proj: &Project, sel: &mut usize, drag: &mut Option<u32>) -> Option<u32> {
    if proj.media.is_empty() {
        placeholder(ui, "Click + Import, or drop video files onto the window.");
        return None;
    }
    ui.label(RichText::new("Double-click to append - drag onto a track (Ctrl = insert)").size(10.0).color(MUTED));
    let mut add = None;
    ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        for (i, m) in proj.media.iter().enumerate() {
            let len = if m.duration > 0.0 { fmt_tc(m.duration) } else { "--".into() };
            let text = format!("{}   {}{}", m.name, len, if m.has_audio { "" } else { "   (no audio)" });
            let r = ui.selectable_label(*sel == i, text).interact(Sense::click_and_drag());
            if r.clicked() {
                *sel = i;
            }
            if r.double_clicked() {
                add = Some(m.id);
            }
            if r.drag_started() {
                *sel = i;
                *drag = Some(m.id);
            }
        }
    });
    add
}

#[allow(dead_code)]
fn thumb_grid(ui: &mut Ui, sel: &mut usize) {
    const GAP: f32 = 10.0;
    const LBL: f32 = 22.0;
    ScrollArea::vertical()
        .auto_shrink([false; 2])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            let w = ui.available_width();
            let tw = (w - GAP) / 2.0;
            let th = tw * 9.0 / 16.0;
            let rows = CLIP_NAMES.len().div_ceil(2) as f32;
            let (rect, resp) =
                ui.allocate_exact_size(vec2(w, rows * (th + LBL + GAP)), Sense::click());
            if resp.clicked() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let (dx, dy) = (pos.x - rect.left(), pos.y - rect.top());
                    let (col, row) = ((dx / (tw + GAP)) as usize, (dy / (th + LBL + GAP)) as usize);
                    let i = row * 2 + col;
                    if col < 2 && i < CLIP_NAMES.len() {
                        *sel = i;
                    }
                }
            }
            let p = ui.painter_at(rect);
            for (i, name) in CLIP_NAMES.iter().enumerate() {
                let x = rect.left() + (i % 2) as f32 * (tw + GAP);
                let y = rect.top() + (i / 2) as f32 * (th + LBL + GAP);
                let r = Rect::from_min_size(pos2(x, y), vec2(tw, th));
                if r.bottom() < p.clip_rect().top() || r.top() > p.clip_rect().bottom() + LBL {
                    continue;
                }
                let hue = thumb_hue(i as u32 + 1, 0);
                rounded_gradient(&p, r, 5.0, hsv(hue, 35.0, 55.0), hsv(hue, 45.0, 25.0));
                let on = i == *sel;
                p.rect_stroke(
                    r,
                    5.0,
                    if on { Stroke::new(1.5, TEXT) } else { Stroke::new(1.0, rgba(110, 125, 170, 255)) },
                );
                let col = if on { TEXT } else { MUTED };
                let g = elided(ui, name, FontId::monospace(8.0), col, tw);
                p.galley(pos2(x, y + th + 3.0 + (LBL - 3.0 - g.size().y) / 2.0), g, col);
            }
        });
}

// ------------------------------------------------------------------ title bar
fn title_bar(ui: &mut Ui, ctx: &Context, title: &str, maximized: bool) {
    let rect = ui.max_rect();
    let drag = ui.interact(rect, Id::new("titlebar"), Sense::click_and_drag());
    if drag.drag_started_by(PointerButton::Primary) {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if drag.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }
    let p = ui.painter();
    let cy = rect.center().y;

    // logo badge
    let badge = Rect::from_center_size(pos2(rect.left() + 14.0 + 10.0, cy), vec2(18.0, 18.0));
    rounded_gradient(p, badge, 5.0, DOT_COLORS[1], DOT_COLORS[0]);
    p.add(Shape::convex_polygon(
        vec![
            pos2(badge.left() + 7.0, badge.top() + 5.0),
            pos2(badge.left() + 7.0, badge.top() + 13.0),
            pos2(badge.left() + 13.0, badge.top() + 9.0),
        ],
        rgba(240, 246, 252, 235),
        Stroke::NONE,
    ));
    p.text(
        pos2(rect.left() + 42.0, cy),
        Align2::LEFT_CENTER,
        title,
        FontId::monospace(11.0),
        TEXT,
    );

    // window dots
    for (i, tip) in ["Minimize", "Maximize", "Close"].iter().enumerate() {
        let cx = rect.right() - 14.0 - 9.0 - (2 - i) as f32 * 26.0;
        let r = Rect::from_center_size(pos2(cx, cy), vec2(18.0, 18.0));
        let resp = ui
            .interact(r, Id::new(("dot", i)), Sense::click())
            .on_hover_text(*tip)
            .on_hover_cursor(CursorIcon::PointingHand);
        let c = DOT_COLORS[i];
        let fillc = if resp.hovered() { c.lerp_to_gamma(Color32::WHITE, 0.12) } else { c };
        let p = ui.painter();
        p.circle(r.center(), 7.5, fillc, Stroke::new(1.0, rgba(0, 0, 0, 55)));
        if resp.hovered() {
            let luma = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
            let ink = if luma > 150.0 { DEEP } else { rgba(240, 246, 252, 235) };
            let st = Stroke::new(1.4, ink);
            let o = r.min;
            match i {
                0 => {
                    p.line_segment([o + vec2(5.5, 9.0), o + vec2(12.5, 9.0)], st);
                }
                1 => {
                    p.rect_stroke(Rect::from_min_size(o + vec2(5.5, 5.5), vec2(7.0, 7.0)), 0.0, st);
                }
                _ => {
                    p.line_segment([o + vec2(6.0, 6.0), o + vec2(12.0, 12.0)], st);
                    p.line_segment([o + vec2(12.0, 6.0), o + vec2(6.0, 12.0)], st);
                }
            }
        }
        if resp.clicked() {
            ctx.send_viewport_cmd(match i {
                0 => ViewportCommand::Minimized(true),
                1 => ViewportCommand::Maximized(!maximized),
                _ => ViewportCommand::Close,
            });
        }
    }
}

// ------------------------------------------------------------------ recovery / autosave
/// Per-user data folder, independent of the directory from which Yoru was launched.
fn yoru_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base).join("Yoru");
        }
    }
    if let Some(base) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(base).join("yoru");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local").join("share").join("yoru");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("." )).join(".yoru")
}

fn autosave_file() -> PathBuf {
    yoru_data_dir().join("autosave.yoru")
}

/// Replace a file via a staged write, retaining the old copy if replacement fails.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("yoru.tmp");
    let backup = path.with_extension("yoru.bak");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }

    // Unix rename replaces an existing destination. Windows does not, so preserve the
    // previous autosave as a backup while installing the new one.
    if path.exists() {
        let _ = std::fs::remove_file(&backup);
        if let Err(e) = std::fs::rename(path, &backup) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, path);
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

fn recoverable_autosave_path(path: &Path) -> Option<PathBuf> {
    let backup = path.with_extension("yoru.bak");
    [path.to_path_buf(), backup].into_iter().find(|candidate| {
        std::fs::read_to_string(candidate)
            .ok()
            .and_then(|text| Project::from_text(&text).ok())
            .is_some()
    })
}

// ------------------------------------------------------------------ app
struct App {
    project: String,
    dirty: bool,
    t: f64,
    rate: i32,
    last: Instant,
    levels: (f32, f32),
    fit: usize,
    timeline: Timeline,
    inspector: fxui::Inspector,
    browser: Browser,
    proj: Project,
    engine: engine::Engine,
    jumped: bool,
    tex: Option<TextureHandle>,
    tex_aspect: f32,
    assets: Assets,
    project_path: Option<std::path::PathBuf>,
    quality_idx: usize,
    applied_quality: usize,
    drag_media: Option<u32>,
    ghost: Option<Ghost>,
    last_params: Option<[f32; NPARAM]>,
    autosave_path: PathBuf,
    last_change: Instant,
    last_autosave: Instant,
    recovery_offer: bool,
}

impl App {
    fn new() -> Self {
        set_duration(10.0);
        let now = Instant::now();
        let autosave_path = autosave_file();
        let recovery_offer = recoverable_autosave_path(&autosave_path).is_some();
        Self {
            project: PROJECT_NAME.into(),
            dirty: false,
            t: 0.0,
            rate: 0,
            last: Instant::now(),
            levels: (0.0, 0.0),
            fit: 0,
            timeline: Timeline::new(),
            inspector: fxui::Inspector::new(),
            browser: Browser { tab: 0, sel: 0, view: 0, search: String::new(), fx_sel: None },
            proj: Project::new(),
            engine: engine::Engine::new(),
            jumped: false,
            tex: None,
            tex_aspect: 16.0 / 9.0,
            assets: Assets::new(),
            project_path: None,
            quality_idx: 1, // 1/2 resolution: snappy by default
            applied_quality: 1,
            drag_media: None,
            ghost: None,
            last_params: None,
            autosave_path,
            last_change: now,
            last_autosave: now,
            recovery_offer,
        }
    }

    /// User-initiated jump: moves the playhead and repositions the decoder.
    fn seek(&mut self, t: f64) {
        self.set_time(t);
        self.jumped = true; // the engine repositions its decoders on the next frame
    }

    fn project_file_dialog() -> rfd::FileDialog {
        rfd::FileDialog::new().add_filter("Yoru project", &["yoru"])
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_change = Instant::now();
    }

    fn maybe_autosave(&mut self) {
        if !self.dirty {
            return;
        }
        // Save shortly after edits stop, with a 20-second upper bound even while the user
        // continuously drags a control or performs a long sequence of edits.
        let idle = self.last_change.elapsed() >= Duration::from_secs(2)
            && self.last_change > self.last_autosave;
        let periodic = self.last_autosave.elapsed() >= Duration::from_secs(20);
        if idle || periodic {
            match write_atomic(&self.autosave_path, &self.proj.to_text()) {
                Ok(()) => self.last_autosave = Instant::now(),
                Err(e) => eprintln!("Yoru autosave failed at {}: {e}", self.autosave_path.display()),
            }
        }
    }

    fn clear_autosave(&self) {
        let _ = std::fs::remove_file(&self.autosave_path);
        let _ = std::fs::remove_file(self.autosave_path.with_extension("yoru.bak"));
        let _ = std::fs::remove_file(self.autosave_path.with_extension("yoru.tmp"));
    }

    fn rebuild_media_runtime(&mut self, ctx: &Context) {
        self.engine = engine::Engine::new();
        self.assets = Assets::new();
        for m in &self.proj.media {
            self.assets.spawn(ctx, m.id, &m.path, m.duration, m.has_audio);
            match media::Player::open(&m.path, ctx.clone()) {
                Ok(player) => self.engine.insert(m.id, player),
                Err(e) => eprintln!("cannot reopen media {}: {e}", m.path.display()),
            }
        }
    }

    fn recover_autosave(&mut self, ctx: &Context) {
        let source = recoverable_autosave_path(&self.autosave_path)
            .unwrap_or_else(|| self.autosave_path.clone());
        let loaded = std::fs::read_to_string(&source)
            .map_err(|e| e.to_string())
            .and_then(|text| Project::from_text(&text));
        match loaded {
            Ok(project) => {
                self.set_rate(0);
                self.proj = project;
                self.rebuild_media_runtime(ctx);
                self.tex = None;
                self.t = 0.0;
                self.timeline.sel.clear();
                self.timeline.scroll = 0.0;
                self.project = "Recovered Project".into();
                self.project_path = None;
                self.dirty = true;
                self.last_change = Instant::now();
                self.last_autosave = Instant::now();
                self.recovery_offer = false;
                set_duration(self.proj.end().max(10.0));
                self.timeline.fit(duration());
                self.jumped = true;
            }
            Err(e) => {
                eprintln!("cannot recover Yoru autosave: {e}");
                self.recovery_offer = false;
            }
        }
    }

    fn save_project(&mut self, save_as: bool) {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => Self::project_file_dialog().set_file_name(format!("{}.yoru", self.project)).save_file(),
        };
        let Some(mut path) = path else { return };
        if path.extension().is_none() {
            path.set_extension("yoru");
        }
        match write_atomic(&path, &self.proj.to_text()) {
            Ok(()) => {
                if let Some(stem) = path.file_stem() {
                    self.project = stem.to_string_lossy().into_owned();
                }
                self.project_path = Some(path);
                self.dirty = false;
                self.last_autosave = Instant::now();
                self.clear_autosave();
                self.recovery_offer = false;
            }
            Err(e) => eprintln!("cannot save {}: {e}", path.display()),
        }
    }

    fn open_project(&mut self, ctx: &Context) {
        let Some(path) = Self::project_file_dialog().pick_file() else { return };
        let loaded = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| Project::from_text(&t));
        match loaded {
            Ok(p) => {
                self.set_rate(0);
                self.proj = p;
                self.rebuild_media_runtime(ctx);
                self.tex = None;
                self.t = 0.0;
                self.timeline.sel.clear();
                self.timeline.scroll = 0.0;
                self.after_edit();
                self.timeline.fit(duration());
                if let Some(stem) = path.file_stem() {
                    self.project = stem.to_string_lossy().into_owned();
                }
                self.project_path = Some(path);
                self.dirty = false;
                self.last_autosave = Instant::now();
                self.clear_autosave();
                self.recovery_offer = false;
            }
            Err(e) => eprintln!("cannot open {}: {e}", path.display()),
        }
    }

    /// Call after ANY change to the clip list (edit, undo, redo, import).
    fn after_edit(&mut self) {
        set_duration(self.proj.end().max(10.0));
        self.t = self.t.min(duration());
        let proj = &self.proj;
        self.timeline.sel.retain(|id| proj.clip(*id).is_some());
        self.jumped = true;
        self.mark_dirty();
    }

    /// Clock-driven move (playback tick): never touches the decoder.
    fn set_time(&mut self, t: f64) {
        let t = t.clamp(0.0, duration());
        if (self.rate > 0 && t >= duration()) || (self.rate < 0 && t <= 0.0) {
            self.set_rate(0);
        }
        self.t = t;
        if self.rate != 0 && self.proj.media.is_empty() {
            let w = wave();
            let i = (t * 20.0) as usize % w.len();
            self.levels = (w[i], w[(i + 5) % w.len()]);
        }
    }

    fn set_rate(&mut self, r: i32) {
        self.rate = r;
        if r != 0 {
            self.last = Instant::now();
        } else {
            self.levels = (0.0, 0.0);
        }
    }

    fn toggle_play(&mut self) {
        self.set_rate(if self.rate != 0 { 0 } else { 1 });
    }

    fn shuttle(&mut self, dir: i32) {
        if dir > 0 {
            self.set_rate(if self.rate <= 0 { 1 } else { (self.rate * 2).min(8) });
        } else {
            self.set_rate(if self.rate >= 0 { -1 } else { (self.rate * 2).max(-8) });
        }
    }

    /// Previous (-1) / next (+1) cut on any track.
    fn edit_point(&self, dir: i32) -> f64 {
        let tg = self.proj.snap_targets(&[]);
        if dir < 0 {
            tg.into_iter().filter(|e| *e < self.t - 1e-3).fold(0.0, f64::max)
        } else {
            tg.into_iter().filter(|e| *e > self.t + 1e-3).fold(f64::INFINITY, f64::min).min(duration())
        }
    }

    fn handle_keys(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let k = |key: Key, m: Modifiers| ctx.input_mut(|i| i.consume_key(m, key));
        let (none, cmd, shift) = (Modifiers::NONE, Modifiers::COMMAND, Modifiers::SHIFT);

        // ---- file / history
        if k(Key::S, cmd | shift) {
            self.save_project(true);
        } else if k(Key::S, cmd) {
            self.save_project(false);
        }
        if k(Key::O, cmd) {
            self.open_project(ctx);
        }
        if k(Key::Z, cmd | shift) || k(Key::Y, cmd) {
            if self.proj.redo() {
                self.after_edit();
            }
        } else if k(Key::Z, cmd) && self.proj.undo() {
            self.after_edit();
        }

        // ---- editing
        if (k(Key::K, cmd) || k(Key::B, cmd)) && self.proj.cut_at(self.t) {
            self.after_edit(); // Add Edit
        }
        if k(Key::A, cmd) {
            self.timeline.sel = self.proj.clips.iter().map(|c| c.id).collect();
        }
        if k(Key::L, cmd) {
            let sel = self.timeline.sel.clone();
            if self.proj.unlink(&sel) {
                self.after_edit();
            }
        }
        // Delete = lift (leave a gap), Shift+Delete = ripple (close the gap)
        let ripple = k(Key::Delete, shift) || k(Key::Backspace, shift);
        let lift = !ripple && (k(Key::Delete, none) || k(Key::Backspace, none));
        if ripple || lift {
            let sel = self.timeline.sel.clone();
            if self.proj.delete_many(&sel, ripple) {
                self.timeline.sel.clear();
                self.after_edit();
            }
        }
        // `,` insert / `.` overwrite the media selected in the browser at the playhead (on V1)
        let insert = k(Key::Comma, none);
        let overwrite = k(Key::Period, none);
        if insert || overwrite {
            if let Some(m) = self.proj.media.get(self.browser.sel).map(|m| m.id) {
                if self.proj.place_media(m, Lane::V1, self.t, insert).is_some() {
                    self.after_edit();
                }
            }
        }

        // ---- transport
        if k(Key::Space, none) {
            self.toggle_play();
        }
        if k(Key::K, none) {
            self.set_rate(0);
        }
        if k(Key::L, none) {
            self.shuttle(1);
        }
        if k(Key::J, none) {
            self.shuttle(-1);
        }
        if k(Key::ArrowRight, none) {
            // Frame stepping is deterministic: stop transport first and move exactly one source
            // frame, not one hard-coded 30 fps frame for every clip.
            if self.rate != 0 { self.set_rate(0); }
            let step = self.engine.frame_step();
            self.seek(self.t + step);
        }
        if k(Key::ArrowLeft, none) {
            if self.rate != 0 { self.set_rate(0); }
            let step = self.engine.frame_step();
            self.seek(self.t - step);
        }
        if k(Key::ArrowUp, none) {
            let e = self.edit_point(-1);
            self.seek(e);
        }
        if k(Key::ArrowDown, none) {
            let e = self.edit_point(1);
            self.seek(e);
        }
        if k(Key::Home, none) {
            self.seek(0.0);
        }
        if k(Key::End, none) {
            self.seek(duration());
        }

        // ---- tools (Premiere layout)
        for (key, tool) in [
            (Key::V, Tool::Select),
            (Key::B, Tool::Ripple),
            (Key::N, Tool::Rect),
            (Key::Y, Tool::Slip),
            (Key::C, Tool::Razor),
            (Key::Z, Tool::Zoom),
            (Key::H, Tool::Hand),
        ] {
            if k(key, none) {
                self.timeline.tool = tool;
            }
        }
        if k(Key::S, none) {
            self.timeline.snap = !self.timeline.snap;
        }
        if k(Key::Equals, none) || k(Key::Plus, none) {
            self.timeline.zoom_step(1.5, self.t);
        }
        if k(Key::Minus, none) {
            self.timeline.zoom_step(1.0 / 1.5, self.t);
        }
        if k(Key::Backslash, none) {
            self.timeline.fit(self.proj.end().max(1.0));
        }
    }
}

impl eframe::App for App {
    /// Fully transparent clear colour so the acrylic blur shows through.
    fn clear_color(&self, _v: &Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _f: &mut eframe::Frame) {
        // playback clock (only repaints while playing -> idle UI costs ~0 CPU)
        if self.rate != 0 {
            let now = Instant::now();
            let dt = now.duration_since(self.last).as_secs_f64();
            self.last = now;
            // The audio device is the master clock at 1x; wall clock otherwise. Do not clamp
            // to the existing playhead: that value can be a little ahead of the device clock and
            // would freeze the playhead (and video) until the clock catches up. Allow at most a
            // tiny correction backwards to absorb callback/latency jitter without visible jumps.
            let audio_t = if self.rate == 1 { self.engine.clock() } else { None };
            let nt = audio_t.map_or(self.t + self.rate as f64 * dt, |c| c.max(self.t - 0.01));
            if self.rate == 1 {
                self.set_time(nt);
            } else {
                self.seek(nt);
            }
            if self.rate != 0 {
                if let Some((l, r)) = self.engine.peaks() {
                    self.levels = (
                        peak_to_meter(l).max(self.levels.0 - 0.05),
                        peak_to_meter(r).max(self.levels.1 - 0.05),
                    );
                }
            }
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        self.handle_keys(ctx);

        // Keep an on-disk recovery copy even if the application is later terminated abruptly.
        self.maybe_autosave();

        // A small recovery prompt is shown only when a valid unsaved project was found on disk.
        if self.recovery_offer {
            let mut choice = 0; // 1 = recover, 2 = discard
            Window::new("Recover unsaved work?")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Yoru found an autosave from an earlier session.");
                    ui.label("Would you like to restore the timeline and media list?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Recover project").clicked() { choice = 1; }
                        if ui.button("Discard autosave").clicked() { choice = 2; }
                    });
                });
            match choice {
                1 => self.recover_autosave(ctx),
                2 => {
                    self.clear_autosave();
                    self.recovery_offer = false;
                }
                _ => {}
            }
        }

        self.assets.pump(ctx);

        // drop video files onto the window: import into the media pool and append to the timeline
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let mut imported = false;
        for f in dropped {
            let Some(path) = f.path else { continue };
            match media::Player::open(&path, ctx.clone()) {
                Ok(p) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let (dur, audio) = (p.info.duration, p.has_audio());
                    let id = self.proj.add_media(path.clone(), name, dur, audio);
                    self.assets.spawn(ctx, id, &path, dur, audio);
                    self.engine.insert(id, p); // already open: reuse it for playback
                    imported |= self.proj.append_media(id);
                }
                Err(e) => eprintln!("cannot open {}: {e}", path.display()),
            }
        }
        if imported {
            self.set_rate(0);
            self.after_edit();
            self.timeline.scroll = 0.0;
            self.timeline.fit(duration());
        }

        // playback engine: follow the playhead, switch decoders at cuts, pre-roll the next clip
        let jumped = std::mem::take(&mut self.jumped);
        let muted = self.proj.audio_muted_at(self.t);
        self.engine.update(ctx, &self.proj, self.t, self.rate == 1, jumped, muted, self.proj.audio_gain_at(self.t));
        // new decoded frame, or an effect value changed (slider, keyframe, playhead on an animated clip)
        let new_frame = self.engine.poll(self.t).is_some();
        let params = self.proj.video_clip_at(self.t).map(|c| c.fx.eval(c.fx_time(self.t)));
        if new_frame || params != self.last_params {
            self.last_params = params;
            if let (Some(f), Some(pr)) = (self.engine.current(), params) {
                let (img, aspect) = preview_image(f, &pr);
                self.tex_aspect = aspect;
                match &mut self.tex {
                    Some(t) => t.set(img, TextureOptions::LINEAR),
                    None => self.tex = Some(ctx.load_texture("video", img, TextureOptions::LINEAR)),
                }
            }
        }
        if !self.engine.is_active() {
            self.tex = None; // gap / placeholder clip -> placeholder monitor
        }
        let aspect = self.tex_aspect;
        let video = self.tex.as_ref().map(|t| (t.id(), aspect));
        let overlays: Vec<gfxdraw::Overlay> = self.proj.clips.iter()
            .filter(|c| c.gfx.is_some() && c.lane.is_video() && !self.proj.hidden[c.lane.slot()] && c.start <= self.t && self.t < c.end())
            .filter_map(|c| c.gfx.clone().map(|gfx| gfxdraw::Overlay { gfx, v: c.fx.eval(c.fx_time(self.t)) }))
            .collect();
        let label = self.project.clone();

        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let radius = if maximized { 0.0 } else { WINDOW_RADIUS };

        // window glass: faint sheen + crisp border over the acrylic backdrop
        {
            let bg = ctx.layer_painter(LayerId::background());
            let r = ctx.screen_rect().shrink(0.5);
            bg.rect_filled(r, radius, rgba(255, 255, 255, 6));
            bg.rect_stroke(r, radius, Stroke::new(1.2, rgba(110, 125, 170, 180)));
        }

        let title = format!("Yoru  -  {}{}", self.project, if self.dirty { " *" } else { "" });
        TopBottomPanel::top("title")
            .exact_height(36.0)
            .frame(Frame::none())
            .show_separator_line(false)
            .show(ctx, |ui| title_bar(ui, ctx, &title, maximized));

        let (mut edited, mut seeked, mut play) = (false, false, false);
        let mut add_media: Option<u32> = None;
        let mut import_requested = false;
        let mut fx_changed = false;
        {
            let App { project, t, rate, levels, fit, timeline, inspector, browser, proj, assets, quality_idx, drag_media, ghost, .. } = self;
            let playing = *rate > 0;

            TopBottomPanel::bottom("bottom")
                .resizable(true)
                .default_height(372.0)
                .height_range(240.0..=720.0)
                .show_separator_line(false)
                .frame(Frame::none().inner_margin(Margin { left: 12.0, right: 12.0, top: 6.0, bottom: 12.0 }))
                .show(ctx, |ui| {
                    SidePanel::right("browser")
                        .resizable(true)
                        .default_width(300.0)
                        .width_range(220.0..=640.0)
                        .show_separator_line(false)
                        .frame(Frame::none().inner_margin(Margin { left: 12.0, ..Default::default() }))
                        .show_inside(ui, |ui| {
                            add_media = browser_panel(ui, browser, proj, assets, drag_media, &mut import_requested);
                        });
                    CentralPanel::default().frame(Frame::none()).show_inside(ui, |ui| {
                        let r = timeline_panel(ui, timeline, t, project, *levels, proj, assets, ghost.as_ref());
                        edited |= r.0;
                        seeked |= r.1;
                    });
                });

            CentralPanel::default()
                .frame(Frame::none().inner_margin(Margin { left: 12.0, right: 12.0, top: 6.0, bottom: 6.0 }))
                .show(ctx, |ui| {
                    SidePanel::right("inspector")
                        .resizable(true)
                        .default_width(640.0)
                        .width_range(360.0..=1100.0)
                        .show_separator_line(false)
                        .frame(Frame::none().inner_margin(Margin { left: 12.0, ..Default::default() }))
                        .show_inside(ui, |ui| {
                            fx_changed |= fxui::inspector_panel(ui, inspector, proj, &mut timeline.sel, *t);
                        });
                    CentralPanel::default().frame(Frame::none()).show_inside(ui, |ui| {
                        play |= monitor_panel(ui, &label, *t, playing, fit, quality_idx, video, &overlays);
                    });
                });
        }
        // Import goes into the Project bin only. Users can then drag or double-click media
        // when they actually want it on the timeline.
        if import_requested {
            let files = rfd::FileDialog::new()
                .add_filter("Video media", &["mp4", "mov", "mkv", "webm", "avi", "m4v", "mpg", "mpeg"])
                .pick_files();
            if let Some(paths) = files {
                let mut imported_any = false;
                for path in paths {
                    if self.proj.media.iter().any(|m| m.path == path) {
                        continue; // do not duplicate an already-imported source
                    }
                    match media::Player::open(&path, ctx.clone()) {
                        Ok(player) => {
                            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            let (dur, audio) = (player.info.duration, player.has_audio());
                            let id = self.proj.add_media(path.clone(), name, dur, audio);
                            self.assets.spawn(ctx, id, &path, dur, audio);
                            self.engine.insert(id, player);
                            self.browser.sel = self.proj.media.iter().position(|m| m.id == id).unwrap_or(0);
                            imported_any = true;
                        }
                        Err(e) => eprintln!("cannot import {}: {e}", path.display()),
                    }
                }
                if imported_any {
                    self.mark_dirty();
                }
            }
        }

        if let Some(id) = add_media {
            if self.proj.append_media(id) {
                edited = true;
            }
        }
        if fx_changed {
            self.mark_dirty();
            set_duration(self.proj.end().max(10.0));
        }
        if let Some(g) = self.inspector.goto.take() {
            self.seek(g); // keyframe navigation
        }
        // playback resolution changed: every open decoder rescales, then we re-seek to refill
        if self.quality_idx != self.applied_quality {
            self.applied_quality = self.quality_idx;
            self.engine.set_quality(QUALITIES[self.quality_idx].1);
            self.jumped = true;
            ctx.request_repaint();
        }
        // media dragged from the browser: ghost clip on the timeline, drop = overwrite (Ctrl = insert)
        if let Some(mid) = self.drag_media {
            let pointer = ctx.input(|i| i.pointer.hover_pos());
            let released = ctx.input(|i| i.pointer.any_released());
            let target = pointer.and_then(|p| self.timeline.drop_target(p, &self.proj, self.t));
            let info = self.proj.media(mid).map(|m| (m.name.clone(), m.duration.max(1.0)));
            self.ghost = match (target, &info) {
                (Some((lane, start)), Some((name, dur))) => Some(Ghost { lane, start, dur: *dur, name: name.clone() }),
                _ => None,
            };
            if let (Some(p), Some((name, _))) = (pointer, &info) {
                egui::Area::new(Id::new("media_drag_label"))
                    .fixed_pos(p + vec2(14.0, 14.0))
                    .order(Order::Tooltip)
                    .interactable(false)
                    .show(ctx, |ui| {
                        ui.label(RichText::new(name).monospace().size(10.0).color(TEXT));
                    });
            }
            if released {
                if let Some((lane, start)) = target {
                    let insert = ctx.input(|i| i.modifiers.command);
                    if self.proj.place_media(mid, lane, start, insert).is_some() {
                        edited = true;
                    }
                }
                self.drag_media = None;
                self.ghost = None;
            }
            ctx.request_repaint();
        }
        if edited {
            self.after_edit();
        }
        if seeked {
            self.jumped = true;
        }
        if edited || seeked {
            ctx.request_repaint(); // let the engine reposition on the very next frame
        }
        if play {
            self.toggle_play();
        }
    }
}

// ------------------------------------------------------------------ platform glass
#[cfg(windows)]
fn apply_glass(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    // acrylic blur with a light tint -> see-through frosted glass
    let _ = window_vibrancy::apply_acrylic(cc, Some(GLASS_TINT));

    // Windows 11 rounded corners (DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2)
    if let Ok(h) = cc.window_handle() {
        if let RawWindowHandle::Win32(w) = h.as_raw() {
            let pref: i32 = 2;
            unsafe {
                DwmSetWindowAttribute(
                    w.hwnd.get() as *mut core::ffi::c_void,
                    33,
                    &pref as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    }
}

#[cfg(not(windows))]
fn apply_glass(_cc: &eframe::CreationContext<'_>) {}

fn setup(ctx: &Context) {
    // Cascadia Mono / Consolas if installed, egui's built-in mono otherwise
    let mut fonts = FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\consola.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("mono".into(), FontData::from_owned(bytes).into());
            fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "mono".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
    ctx.options_mut(|o| o.zoom_with_keyboard = false);

    let mut style = (*ctx.style()).clone();
    for (_, f) in style.text_styles.iter_mut() {
        f.size = 11.0;
        f.family = FontFamily::Monospace;
    }
    style.spacing.item_spacing = vec2(8.0, 6.0);
    let v = &mut style.visuals;
    v.override_text_color = Some(TEXT);
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = Color32::TRANSPARENT;
    v.extreme_bg_color = rgba(0, 0, 0, 77);
    v.selection.bg_fill = rgba(72, 82, 102, 242);
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.bg_fill = rgba(139, 148, 158, 41);
    v.widgets.hovered.weak_bg_fill = rgba(139, 148, 158, 41);
    v.widgets.inactive.weak_bg_fill = rgba(255, 255, 255, 18);
    v.popup_shadow = Shadow::NONE;
    v.window_shadow = Shadow::NONE;
    ctx.set_style(style);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Yoru")
            .with_inner_size([1600.0, 900.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "Yoru",
        options,
        Box::new(|cc| {
            setup(&cc.egui_ctx);
            apply_glass(cc);
            Ok(Box::new(App::new()))
        }),
    )
}
