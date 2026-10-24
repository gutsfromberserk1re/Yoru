//! Video editor UI prototype - Rust / egui port (v5.0)
//!
//! * Panels: solid French Blue (#394988), rounded, 1px border.
//! * Everything else (title bar, gaps between panels, window background):
//!   transparent frosted-glass (Windows acrylic blur, Windows 11 rounded corners).
//!
//! Run:  cargo run --release

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use eframe::egui::{self, ecolor::Hsva, epaint::CubicBezierShape, *};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

mod media;

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
const PANEL_BG: Color32 = Color32::from_rgb(0x39, 0x49, 0x88);
const DOT_COLORS: [Color32; 3] = [
    Color32::from_rgb(0x48, 0x52, 0x66),
    Color32::from_rgb(0x5E, 0x6B, 0x82),
    Color32::from_rgb(0x8A, 0x99, 0xB3),
];

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}
fn panel_border() -> Color32 {
    rgba(110, 125, 170, 217)
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

struct Param {
    name: &'static str,
    lo: f32,
    hi: f32,
    val: f32,
    suffix: &'static str,
}

fn param_row(ui: &mut Ui, p: &mut Param) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (r, _) = ui.allocate_exact_size(vec2(84.0, 18.0), Sense::hover());
        ui.painter()
            .text(r.left_center(), Align2::LEFT_CENTER, p.name, FontId::monospace(11.0), MUTED);
        let w = (ui.available_width() - 70.0).max(20.0);
        slider(ui, &mut p.val, p.lo, p.hi, w);
        let (r, _) = ui.allocate_exact_size(vec2(60.0, 20.0), Sense::hover());
        ui.painter().rect_filled(r, 4.0, rgba(0, 0, 0, 77));
        ui.painter().text(
            r.center(),
            Align2::CENTER_CENTER,
            format!("{}{}", p.val as i32, p.suffix),
            FontId::monospace(11.0),
            TEXT,
        );
    });
}

// ------------------------------------------------------------------ timeline
const RULER: f32 = 28.0;
const PAD: f32 = 12.0;
const LEFT: f32 = 6.0;
const LABEL: f32 = 15.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Conn,
    Main,
    Audio,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Ripple,
    Slip,
    Rect,
    Razor,
    Zoom,
    Hand,
}

impl Tool {
    fn ico(self) -> Ico {
        match self {
            Tool::Select => Ico::Select,
            Tool::Ripple => Ico::Ripple,
            Tool::Slip => Ico::Slip,
            Tool::Rect => Ico::Rect,
            Tool::Razor => Ico::Razor,
            Tool::Zoom => Ico::Zoom,
            Tool::Hand => Ico::Hand,
        }
    }
}

const TOOLS: [(Tool, &str); 7] = [
    (Tool::Select, "Select (A)"),
    (Tool::Ripple, "Trim (T)"),
    (Tool::Slip, "Position (P)"),
    (Tool::Rect, "Range (R)"),
    (Tool::Razor, "Blade (B)"),
    (Tool::Zoom, "Zoom (Z)"),
    (Tool::Hand, "Hand (H)"),
];

#[derive(Clone)]
struct Clip {
    lane: Lane,
    start: f64,
    dur: f64,
    name: &'static str,
    seed: u32,
    src_off: f64,
}

struct Timeline {
    pps: f64,
    scroll: f64,
    tool: Tool,
    sel: Option<usize>,
    scrub: bool,
    panning: bool,
    last_t: f64,
    clips: Vec<Clip>,
}

impl Timeline {
    fn new() -> Self {
        let story = [
            ("Cabana", 0.0, 14.0),
            ("Mountain gold", 14.0, 17.0),
            ("Jungle forest", 31.0, 16.0),
            ("Hammock", 47.0, 19.0),
            ("Waterfall", 66.0, 22.0),
            ("Beach", 88.0, 23.0),
        ];
        let mut clips: Vec<Clip> = story
            .iter()
            .enumerate()
            .map(|(i, &(n, s, d))| Clip {
                lane: Lane::Main,
                start: s,
                dur: d,
                name: n,
                seed: i as u32 + 1,
                src_off: 0.0,
            })
            .collect();
        clips.push(Clip { lane: Lane::Conn, start: 12.0, dur: 18.0, name: "Title_01", seed: 11, src_off: 0.0 });
        clips.push(Clip { lane: Lane::Conn, start: 55.0, dur: 30.0, name: "Overlay.png", seed: 12, src_off: 0.0 });
        clips.push(Clip { lane: Lane::Audio, start: 0.0, dur: duration(), name: "hip hop bluegrass", seed: 0, src_off: 0.0 });
        Self { pps: 10.0, scroll: 0.0, tool: Tool::Select, sel: None, scrub: false, panning: false, last_t: 0.0, clips }
    }

    fn x_of(&self, r: Rect, t: f64) -> f32 {
        r.left() + LEFT + ((t - self.scroll) * self.pps) as f32
    }
    fn t_of(&self, r: Rect, x: f32) -> f64 {
        (x - r.left() - LEFT) as f64 / self.pps + self.scroll
    }

    fn lane(&self, lane: Lane, h: f32) -> (f32, f32) {
        let avail = (h - RULER - 2.0 * PAD).max(120.0);
        let conn = (avail * 0.20).clamp(40.0, 64.0);
        let main = (avail * 0.36).clamp(64.0, 112.0);
        let aud = (avail * 0.17).clamp(34.0, 58.0);
        let yc = RULER + PAD;
        let ym = yc + conn + 8.0;
        let ya = ym + main + 8.0;
        match lane {
            Lane::Conn => (yc, conn),
            Lane::Main => (ym, main),
            Lane::Audio => (ya, aud),
        }
    }

    fn zoom_at(&mut self, f: f64, anchor: f64) {
        let dx = (anchor - self.scroll) * self.pps;
        self.pps = (self.pps * f).clamp(2.0, 200.0);
        self.scroll = (anchor - dx / self.pps).max(0.0);
    }

    fn split(&mut self, idx: usize, t: f64) -> bool {
        let c = self.clips[idx].clone();
        let end = c.start + c.dur;
        if c.start + 0.05 < t && t < end - 0.05 {
            let mut new = c.clone();
            new.start = t;
            new.dur = end - t;
            new.src_off = c.src_off + (t - c.start);
            self.clips[idx].dur = t - c.start;
            self.clips.push(new);
            return true;
        }
        false
    }

    fn cut_at(&mut self, t: f64) -> bool {
        let n = self.clips.len();
        let mut any = false;
        for i in 0..n {
            any |= self.split(i, t);
        }
        any
    }

    fn clip_at(&self, rect: Rect, pos: Pos2) -> Option<usize> {
        let t = self.t_of(rect, pos.x);
        let y = pos.y - rect.top();
        for lane in [Lane::Conn, Lane::Main, Lane::Audio] {
            let (ly, lh) = self.lane(lane, rect.height());
            if y >= ly && y < ly + lh {
                for (i, c) in self.clips.iter().enumerate().rev() {
                    if c.lane == lane && c.start <= t && t <= c.start + c.dur {
                        return Some(i);
                    }
                }
            }
        }
        None
    }

    /// Returns (edited, seeked).
    fn show(&mut self, ui: &mut Ui, rect: Rect, t: &mut f64) -> (bool, bool) {
        let (mut edited, mut seeked) = (false, false);
        let (hover, pressed, down, delta, scroll_y, zoom) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.delta(),
                i.raw_scroll_delta.y,
                i.zoom_delta(),
            )
        });
        let over = ui.rect_contains_pointer(rect);

        // press
        if over && pressed {
            if let Some(pos) = hover {
                let mut handled = false;
                if pos.y - rect.top() >= RULER {
                    if let Some(i) = self.clip_at(rect, pos) {
                        if self.tool == Tool::Razor {
                            let tt = self.t_of(rect, pos.x);
                            edited |= self.split(i, tt);
                        } else {
                            self.sel = Some(i);
                        }
                        handled = true;
                    } else {
                        self.sel = None;
                    }
                }
                if !handled {
                    if self.tool == Tool::Hand {
                        self.panning = true;
                    } else {
                        self.scrub = true;
                        *t = self.t_of(rect, pos.x).clamp(0.0, duration());
                        seeked = true;
                    }
                }
            }
        }
        // drag
        if self.scrub || self.panning {
            if down {
                if self.scrub {
                    if let Some(p) = hover {
                        *t = self.t_of(rect, p.x).clamp(0.0, duration());
                        seeked = true;
                    }
                } else {
                    self.scroll = (self.scroll - delta.x as f64 / self.pps).max(0.0);
                }
            } else {
                self.scrub = false;
                self.panning = false;
            }
        }
        // wheel
        if over {
            if zoom != 1.0 {
                if let Some(p) = hover {
                    let a = self.t_of(rect, p.x);
                    self.zoom_at(zoom as f64, a);
                }
            } else if scroll_y != 0.0 {
                self.scroll = (self.scroll - scroll_y as f64 * 0.8 / self.pps).max(0.0);
            }
        }
        // follow playhead when it moves out of view (playback / keyboard seek)
        if !self.scrub && (*t - self.last_t).abs() > 1e-9 {
            let vw = (rect.width() - LEFT) as f64 / self.pps;
            if *t < self.scroll || *t > self.scroll + vw {
                self.scroll = (*t - vw * 0.1).max(0.0);
            }
        }
        self.last_t = *t;

        self.paint(ui, rect, *t, if over && !self.scrub { hover } else { None });
        (edited, seeked)
    }

    fn paint(&self, ui: &Ui, rect: Rect, t: f64, hover: Option<Pos2>) {
        let p = ui.painter_at(rect);
        let font = FontId::monospace(8.0);

        // ruler
        p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width(), RULER)), 0.0, rgba(40, 48, 70, 180));
        p.rect_filled(
            Rect::from_min_size(pos2(rect.left(), rect.top() + RULER - 1.0), vec2(rect.width(), 1.0)),
            0.0,
            rgba(110, 125, 170, 100),
        );
        let step = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0]
            .into_iter()
            .find(|s| s * self.pps >= 90.0)
            .unwrap_or(600.0);
        let mut tt = (self.scroll / step).floor() * step;
        let ry = rect.top() + RULER;
        loop {
            let x = self.x_of(rect, tt);
            if x > rect.right() {
                break;
            }
            p.line_segment([pos2(x, ry - 10.0), pos2(x, ry - 1.0)], Stroke::new(1.0, rgba(139, 148, 158, 140)));
            let s = tt as u64;
            p.text(
                pos2(x + 4.0, rect.top() + (RULER - 6.0) / 2.0),
                Align2::LEFT_CENTER,
                format!("{:02}:{:02}:{:02}:00", s / 3600, s / 60 % 60, s % 60),
                font.clone(),
                rgba(139, 148, 158, 200),
            );
            let sub = Stroke::new(1.0, rgba(110, 125, 170, 100));
            for k in 1..5 {
                let tx = x + k as f32 * step as f32 * self.pps as f32 / 5.0;
                p.line_segment([pos2(tx, ry - 5.0), pos2(tx, ry - 1.0)], sub);
            }
            tt += step;
        }

        // clips
        for (i, c) in self.clips.iter().enumerate() {
            self.draw_clip(ui, &p, rect, c, self.sel == Some(i), &font);
        }

        // hover line
        if let Some(h) = hover {
            p.line_segment(
                [pos2(h.x, rect.top() + RULER), pos2(h.x, rect.bottom())],
                Stroke::new(1.0, rgba(201, 209, 217, 120)),
            );
        }

        // playhead
        let x = self.x_of(rect, t);
        if x >= rect.left() && x <= rect.right() {
            p.line_segment([pos2(x, rect.top() + 12.0), pos2(x, rect.bottom())], Stroke::new(1.5, TEXT));
            p.add(Shape::convex_polygon(
                vec![
                    pos2(x - 5.0, rect.top() + 3.0),
                    pos2(x + 5.0, rect.top() + 3.0),
                    pos2(x, rect.top() + 11.0),
                ],
                TEXT,
                Stroke::NONE,
            ));
        }
    }

    fn draw_clip(&self, ui: &Ui, p: &Painter, rect: Rect, c: &Clip, selected: bool, font: &FontId) {
        let (y, hh) = self.lane(c.lane, rect.height());
        let y = rect.top() + y;
        let x0 = self.x_of(rect, c.start);
        let x1 = self.x_of(rect, c.start + c.dur);
        if x1 < rect.left() || x0 > rect.right() {
            return; // culled
        }
        let r = Rect::from_min_max(pos2(x0 + 0.5, y), pos2((x1 - 0.5).max(x0 + 1.5), y + hh));
        let p = p.with_clip_rect(r.intersect(p.clip_rect()));

        match c.lane {
            Lane::Audio => {
                p.rect_filled(r, 0.0, rgba(40, 50, 70, 200));
                let mid = y + LABEL + (hh - LABEL) / 2.0;
                let amp = (hh - LABEL - 6.0) / 2.0;
                let w = wave();
                let mut m = Mesh::default();
                let mut x = x0.max(rect.left());
                let xe = x1.min(rect.right());
                while x < xe {
                    let tt = (x - x0) as f64 / self.pps + c.src_off;
                    let a = w[((tt * 20.0) as usize).min(w.len() - 1)] * amp;
                    m.add_colored_rect(
                        Rect::from_min_max(pos2(x, mid - a), pos2(x + 1.0, mid + a)),
                        VISTA,
                    );
                    x += 2.0;
                }
                p.add(Shape::mesh(m));
            }
            _ if c.name.starts_with("Title") => {
                let mut m = Mesh::default();
                push_rect_gradient(&mut m, r, rgba(50, 64, 92, 255), rgba(30, 38, 56, 255));
                p.add(Shape::mesh(m));
            }
            _ => {
                let th = hh - LABEL;
                let tw = (th * 16.0 / 9.0).max(24.0);
                let ty = y + LABEL;
                let mut i = ((rect.left() - x0).max(0.0) / tw) as u32;
                let mut x = x0 + i as f32 * tw;
                let mut m = Mesh::default();
                while x < x1.min(rect.right()) {
                    let hue = thumb_hue(c.seed, i);
                    push_rect_gradient(
                        &mut m,
                        Rect::from_min_size(pos2(x, ty), vec2(tw + 1.0, th)),
                        hsv(hue, 35.0, 55.0),
                        hsv(hue, 45.0, 25.0),
                    );
                    x += tw;
                    i += 1;
                }
                p.add(Shape::mesh(m));
            }
        }
        p.rect_filled(
            Rect::from_min_size(pos2(x0, y), vec2(x1 - x0, LABEL)),
            0.0,
            rgba(40, 48, 70, 230),
        );
        if x1 - x0 > 40.0 {
            let g = elided(ui, c.name, font.clone(), TEXT, x1 - x0 - 12.0);
            p.galley(pos2(x0 + 6.0, y + (LABEL - g.size().y) / 2.0), g, TEXT);
        }
        if selected {
            ui.painter_at(rect).rect_stroke(r, 4.0, Stroke::new(2.0, TEXT));
        }
    }
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

fn monitor_panel(
    ui: &mut Ui,
    project: &str,
    t: f64,
    playing: bool,
    fit: &mut usize,
    video: Option<(TextureId, f32)>,
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

struct Inspector {
    tab: usize,
    sections: Vec<(&'static str, Vec<Param>)>,
}

impl Inspector {
    fn new() -> Self {
        let p = |name, lo, hi, val, suffix| Param { name, lo, hi, val, suffix };
        Self {
            tab: 0,
            sections: vec![
                (
                    "Transform",
                    vec![
                        p("Position X", 0.0, 1920.0, 960.0, ""),
                        p("Position Y", 0.0, 1080.0, 540.0, ""),
                        p("Scale", 0.0, 400.0, 100.0, "%"),
                        p("Rotation", -180.0, 180.0, 0.0, "\u{00B0}"),
                    ],
                ),
                ("Compositing", vec![p("Opacity", 0.0, 100.0, 100.0, "%")]),
                ("Speed", vec![p("Rate", 10.0, 400.0, 100.0, "%")]),
            ],
        }
    }
}

fn graph_widget(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let p = ui.painter_at(rect);
    let (w, h) = (rect.width(), rect.height());
    let at = |fx: f32, fy: f32| pos2(rect.left() + w * fx, rect.top() + h * fy);

    p.rect_filled(rect, 8.0, rgba(30, 35, 48, 180));
    let grid = Stroke::new(1.0, rgba(110, 125, 170, 80));
    for i in 1..5 {
        let x = rect.left() + w * i as f32 / 5.0;
        let y = rect.top() + 34.0 + (h - 52.0) * i as f32 / 5.0;
        p.line_segment([pos2(x, rect.top() + 34.0), pos2(x, rect.bottom() - 18.0)], grid);
        p.line_segment([pos2(rect.left() + 16.0, y), pos2(rect.right() - 16.0, y)], grid);
    }
    p.text(
        pos2(rect.left() + 16.0, rect.top() + 19.0),
        Align2::LEFT_CENTER,
        "Value graph",
        FontId::monospace(9.0),
        MUTED,
    );

    let (a, b) = (at(0.10, 0.80), at(0.90, 0.24));
    let (c1, c2) = (at(0.45, 0.80), at(0.55, 0.24));
    let dash = Stroke::new(1.0, rgba(139, 148, 158, 120));
    p.extend(Shape::dashed_line(&[a, c1], dash, 4.0, 3.0));
    p.extend(Shape::dashed_line(&[b, c2], dash, 4.0, 3.0));
    p.add(CubicBezierShape::from_points_stroke(
        [a, c1, c2, b],
        false,
        Color32::TRANSPARENT,
        Stroke::new(2.0, TEXT),
    ));
    for pt in [c1, c2] {
        p.circle_filled(pt, 4.0, VISTA);
    }
    for pt in [a, b] {
        p.add(Shape::convex_polygon(
            vec![
                pos2(pt.x, pt.y - 5.0),
                pos2(pt.x + 5.0, pt.y),
                pos2(pt.x, pt.y + 5.0),
                pos2(pt.x - 5.0, pt.y),
            ],
            TEXT,
            Stroke::NONE,
        ));
    }
}

fn placeholder(ui: &mut Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new(text).color(MUTED).size(13.0));
    });
}

fn inspector_panel(ui: &mut Ui, ins: &mut Inspector) {
    glass(14.0, 8.0, 14.0, 12.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 4.0;
        tab_strip(ui, &mut ins.tab, &["Effect Controls", "Text", "Essential Graphics"]);
        match ins.tab {
            0 => {
                ui.add_space(8.0);
                let avail = ui.available_rect_before_wrap();
                let lw = (avail.width() - 14.0) * 5.0 / 9.0;
                let left = Rect::from_min_size(avail.min, vec2(lw, avail.height()));
                let right = Rect::from_min_max(pos2(left.right() + 14.0, avail.top()), avail.max);
                ui.allocate_ui_at_rect(left, |ui| {
                    ui.spacing_mut().item_spacing.y = 7.0;
                    ui.horizontal(|ui| {
                        ui.label("2026-10-13_06-45-06.mp4");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(fmt_tc(duration())).color(MUTED));
                        });
                    });
                    for (name, params) in ins.sections.iter_mut() {
                        ui.add_space(6.0);
                        ui.label(RichText::new(*name).color(POWDER).strong());
                        for p in params.iter_mut() {
                            param_row(ui, p);
                        }
                    }
                });
                ui.allocate_ui_at_rect(right, graph_widget);
            }
            1 => placeholder(ui, "Text"),
            _ => placeholder(ui, "Essential Graphics"),
        }
    });
}

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
    search: String,
    fx_sel: Option<&'static str>,
}

fn browser_panel(ui: &mut Ui, b: &mut Browser) {
    glass(12.0, 8.0, 12.0, 12.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 8.0;
        tab_strip(ui, &mut b.tab, &["Clips", "Effects", "Media"]);
        match b.tab {
            0 => thumb_grid(ui, &mut b.sel),
            1 => {
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
            _ => placeholder(ui, "No media locations added."),
        }
    });
}

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

/// Returns (edited, seeked).
fn timeline_panel(
    ui: &mut Ui,
    tl: &mut Timeline,
    t: &mut f64,
    project: &str,
    levels: (f32, f32),
) -> (bool, bool) {
    let mut out = (false, false);
    glass(12.0, 8.0, 12.0, 10.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (tool, tip) in TOOLS {
                let (_, resp) = ui.allocate_exact_size(vec2(30.0, 28.0), Sense::click());
                paint_icon_btn(ui, &resp, tool.ico(), tl.tool == tool, 18.0);
                if resp.on_hover_text(tip).clicked() {
                    tl.tool = tool;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(fmt_tc(duration())).color(MUTED));
                ui.label(project);
            });
        });
        let full = ui.available_rect_before_wrap();
        let meter = Rect::from_min_max(pos2(full.right() - 36.0, full.top()), full.max);
        let view = Rect::from_min_max(full.min, pos2(meter.left() - 6.0, full.bottom()));
        ui.allocate_rect(full, Sense::hover());
        out = tl.show(ui, view, t);
        level_meter(ui, meter, levels.0, levels.1);
    });
    out
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
                0 => p.line_segment([o + vec2(5.5, 9.0), o + vec2(12.5, 9.0)], st),
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
    inspector: Inspector,
    browser: Browser,
    player: Option<media::Player>,
    tex: Option<TextureHandle>,
    media_label: String,
}

impl App {
    fn new() -> Self {
        Self {
            project: PROJECT_NAME.into(),
            dirty: false,
            t: 0.0,
            rate: 0,
            last: Instant::now(),
            levels: (0.0, 0.0),
            fit: 0,
            timeline: Timeline::new(),
            inspector: Inspector::new(),
            browser: Browser { tab: 0, sel: 0, search: String::new(), fx_sel: None },
            player: None,
            tex: None,
            media_label: String::new(),
        }
    }

    /// User-initiated jump: moves the playhead and repositions the decoder.
    fn seek(&mut self, t: f64) {
        self.set_time(t);
        if let Some(p) = self.player.as_mut() {
            p.seek(self.t);
        }
    }

    /// Clock-driven move (playback tick): never touches the decoder.
    fn set_time(&mut self, t: f64) {
        let t = t.clamp(0.0, duration());
        if (self.rate > 0 && t >= duration()) || (self.rate < 0 && t <= 0.0) {
            self.set_rate(0);
        }
        self.t = t;
        if self.rate != 0 && self.player.is_none() {
            let w = wave();
            let i = (t * 20.0) as usize % w.len();
            self.levels = (w[i], w[(i + 5) % w.len()]);
        }
    }

    fn set_rate(&mut self, r: i32) {
        self.rate = r;
        if let Some(p) = self.player.as_mut() {
            p.set_playing(r == 1, self.t); // decode-ahead + audio only at 1x; other speeds scrub
        }
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

    fn handle_keys(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let k = |key: Key, m: Modifiers| ctx.input_mut(|i| i.consume_key(m, key));
        let (none, cmd) = (Modifiers::NONE, Modifiers::COMMAND);

        if k(Key::B, cmd) || k(Key::K, cmd) {
            if self.timeline.cut_at(self.t) {
                self.dirty = true;
            }
        }
        if k(Key::S, cmd) {
            self.dirty = false;
        }
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
            self.seek(self.t + 1.0 / FPS);
        }
        if k(Key::ArrowLeft, none) {
            self.seek(self.t - 1.0 / FPS);
        }
        if k(Key::Home, none) {
            self.seek(0.0);
        }
        if k(Key::End, none) {
            self.seek(duration());
        }
        for (key, tool) in [
            (Key::A, Tool::Select),
            (Key::T, Tool::Ripple),
            (Key::P, Tool::Slip),
            (Key::R, Tool::Rect),
            (Key::B, Tool::Razor),
            (Key::C, Tool::Razor),
            (Key::Z, Tool::Zoom),
            (Key::H, Tool::Hand),
        ] {
            if k(key, none) {
                self.timeline.tool = tool;
            }
        }
        if k(Key::Equals, none) || k(Key::Plus, none) {
            let t = self.t;
            self.timeline.zoom_at(1.25, t);
        }
        if k(Key::Minus, none) {
            let t = self.t;
            self.timeline.zoom_at(0.8, t);
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
            // sound card = master clock at 1x; wall clock otherwise (no audio / shuttle)
            let audio_t = if self.rate == 1 { self.player.as_ref().and_then(|p| p.clock()) } else { None };
            let nt = audio_t.map_or(self.t + self.rate as f64 * dt, |c| c.max(self.t));
            if self.rate == 1 {
                self.set_time(nt);
            } else {
                self.seek(nt);
            }
            if self.rate != 0 {
                if let Some((l, r)) = self.player.as_ref().and_then(|p| p.peaks()) {
                    self.levels = (
                        peak_to_meter(l).max(self.levels.0 - 0.05),
                        peak_to_meter(r).max(self.levels.1 - 0.05),
                    );
                }
            }
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        self.handle_keys(ctx);

        // drag a video file onto the window to open it
        for f in ctx.input(|i| i.raw.dropped_files.clone()) {
            if let Some(path) = f.path {
                match media::Player::open(&path, ctx.clone()) {
                    Ok(p) => {
                        self.set_rate(0);
                        let label = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let d = if p.info.duration > 0.5 { p.info.duration } else { DEFAULT_DURATION };
                        set_duration(d);
                        let name: &'static str = Box::leak(label.clone().into_boxed_str()); // tiny, once per import
                        let mut clips = vec![Clip { lane: Lane::Main, start: 0.0, dur: d, name, seed: 1, src_off: 0.0 }];
                        if p.has_audio() {
                            clips.push(Clip { lane: Lane::Audio, start: 0.0, dur: d, name, seed: 0, src_off: 0.0 });
                        }
                        self.timeline.clips = clips;
                        self.timeline.sel = None;
                        self.timeline.scroll = 0.0;
                        self.timeline.pps = (900.0 / d).clamp(2.0, 200.0);
                        self.media_label = label;
                        self.player = Some(p);
                        self.tex = None;
                        self.dirty = true;
                        self.seek(0.0);
                    }
                    Err(e) => self.media_label = format!("Cannot open: {e}"),
                }
            }
        }
        // pull decoded frames; upload to the GPU only when the picture changed
        if let Some(pl) = self.player.as_mut() {
            if pl.poll(self.t) {
                if let Some(f) = &pl.current {
                    // alpha is always 255 after the RGBA conversion, so premultiplied == straight
                    let img = ColorImage::from_rgba_premultiplied([f.w, f.h], &f.rgba);
                    match &mut self.tex {
                        Some(t) => t.set(img, TextureOptions::LINEAR),
                        None => self.tex = Some(ctx.load_texture("video", img, TextureOptions::LINEAR)),
                    }
                }
            }
        }
        let video = self
            .tex
            .as_ref()
            .zip(self.player.as_ref())
            .map(|(t, p)| (t.id(), p.info.w as f32 / p.info.h as f32));
        let label = if self.media_label.is_empty() { self.project.clone() } else { self.media_label.clone() };

        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let radius = if maximized { 0.0 } else { WINDOW_RADIUS };

        // window glass: faint sheen + crisp border over the acrylic backdrop
        {
            let bg = ctx.layer_painter(LayerId::background());
            let r = ctx.screen_rect().shrink(0.5);
            bg.rect_filled(r, radius, rgba(255, 255, 255, 6));
            bg.rect_stroke(r, radius, Stroke::new(1.2, rgba(110, 125, 170, 180)));
        }

        let title = if self.dirty { format!("{} *", self.project) } else { self.project.clone() };
        TopBottomPanel::top("title")
            .exact_height(36.0)
            .frame(Frame::none())
            .show_separator_line(false)
            .show(ctx, |ui| title_bar(ui, ctx, &title, maximized));

        let (mut edited, mut seeked, mut play) = (false, false, false);
        {
            let App { project, t, rate, levels, fit, timeline, inspector, browser, .. } = self;
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
                        .show_inside(ui, |ui| browser_panel(ui, browser));
                    CentralPanel::default().frame(Frame::none()).show_inside(ui, |ui| {
                        let r = timeline_panel(ui, timeline, t, project, *levels);
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
                        .show_inside(ui, |ui| inspector_panel(ui, inspector));
                    CentralPanel::default().frame(Frame::none()).show_inside(ui, |ui| {
                        play |= monitor_panel(ui, &label, *t, playing, fit, video);
                    });
                });
        }
        if edited {
            self.dirty = true;
        }
        if seeked {
            self.seek(self.t);
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
            .with_title(PROJECT_NAME)
            .with_inner_size([1600.0, 900.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        PROJECT_NAME,
        options,
        Box::new(|cc| {
            setup(&cc.egui_ctx);
            apply_glass(cc);
            Ok(Box::new(App::new()))
        }),
    )
}
