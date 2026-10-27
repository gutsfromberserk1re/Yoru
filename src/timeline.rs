//! Premiere-style multi-track timeline.
//!
//!   * six tracks (V3 V2 V1 / A1 A2 A3) with lock + visibility/mute buttons
//!   * selection: click, Shift-click, marquee; linked clips (video + audio) select together
//!   * Selection tool: drag to move (also between tracks, with overwrite), drag edges to trim
//!   * Ripple (B), Rolling (N), Slip (Y), Razor (C), Zoom (Z), Hand (H)
//!   * magnetic snapping to clip edges + playhead (toggle with S)
//!   * smooth animated zoom, edge auto-scroll while dragging, follow-playhead

use super::*;
use crate::project::{nearest, DragOp, Lane, NTRACKS, NV};

const RULER: f32 = 28.0;
const HEAD: f32 = 76.0; // track header column
const LABEL: f32 = 15.0;
const EDGE: f32 = 7.0; // trim handle width (px)
const SNAP_PX: f64 = 9.0;
const MIN_PPS: f64 = 0.4;
const MAX_PPS: f64 = 600.0;
/// Top-to-bottom display order.
const ORDER: [Lane; NTRACKS] = [Lane::V(2), Lane::V(1), Lane::V(0), Lane::A(0), Lane::A(1), Lane::A(2)];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Ripple,
    Slip,
    Rect, // Rolling edit (keeps the icon slot of the old Range tool)
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
    (Tool::Select, "Selection (V)"),
    (Tool::Ripple, "Ripple Edit (B)"),
    (Tool::Slip, "Slip (Y)"),
    (Tool::Rect, "Rolling Edit (N)"),
    (Tool::Razor, "Razor (C)"),
    (Tool::Zoom, "Zoom (Z)"),
    (Tool::Hand, "Hand (H)"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Zone {
    Body,
    Left,
    Right,
}

/// An in-flight mouse drag on a clip. `pre` is both the undo snapshot and the origin every frame
/// is recomputed from (so dragging never accumulates error).
struct Drag {
    zone: Zone,
    id: u32,
    right: Option<u32>, // rolling edit: the clip on the other side of the cut
    grab_t: f64,
    grab_y: f32,
    pre: Vec<Clip>,
    sel: Vec<u32>,
    tool: Tool,
    moved: bool,
    changed: bool,
}

/// Translucent preview of media being dragged in from the browser.
pub struct Ghost {
    pub lane: Lane,
    pub start: f64,
    pub dur: f64,
    pub name: String,
}

pub struct Timeline {
    pub pps: f64,
    pub scroll: f64,
    pps_target: f64,
    zoom_anchor: Option<(f64, f32)>, // (time under the anchor, x offset from the content's left edge)
    pub tool: Tool,
    pub sel: Vec<u32>,
    pub snap: bool,
    scrub: bool,
    panning: bool,
    marquee: Option<(Pos2, Vec<u32>)>,
    last_t: f64,
    drag: Option<Drag>,
    snap_line: Option<f64>,
    /// Rect of the last frame (the main loop uses it for drops coming from the browser).
    pub view: Rect,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            pps: 10.0,
            scroll: 0.0,
            pps_target: 10.0,
            zoom_anchor: None,
            tool: Tool::Select,
            sel: Vec::new(),
            snap: true,
            scrub: false,
            panning: false,
            marquee: None,
            last_t: 0.0,
            drag: None,
            snap_line: None,
            view: Rect::NOTHING,
        }
    }

    // ---------------------------------------------------------------- geometry
    fn x_of(&self, r: Rect, t: f64) -> f32 {
        r.left() + HEAD + ((t - self.scroll) * self.pps) as f32
    }
    fn t_of(&self, r: Rect, x: f32) -> f64 {
        (x - r.left() - HEAD) as f64 / self.pps + self.scroll
    }

    /// (y offset from the rect top, height) per track slot.
    fn rows(&self, h: f32) -> [(f32, f32); NTRACKS] {
        let avail = (h - RULER - 8.0).max(60.0);
        let ah = ((avail - 6.0) / (NV as f32 * 1.45 + 3.0)).clamp(18.0, 36.0);
        let vh = ah * 1.45;
        let mut out = [(0.0, 0.0); NTRACKS];
        let mut y = RULER + 3.0;
        for (i, lane) in ORDER.iter().enumerate() {
            let hh = if lane.is_video() { vh } else { ah };
            out[lane.slot()] = (y, hh);
            y += hh + 1.0;
            if i == NV - 1 {
                y += 4.0; // gap between the video and audio groups
            }
        }
        out
    }

    fn lane_rect(&self, rect: Rect, lane: Lane) -> Rect {
        let (y, hh) = self.rows(rect.height())[lane.slot()];
        Rect::from_min_size(pos2(rect.left() + HEAD, rect.top() + y), vec2(rect.width() - HEAD, hh))
    }

    /// Track under screen y. `strict`: only inside the row; otherwise the nearest row.
    fn lane_at(&self, rect: Rect, y: f32, strict: bool) -> Option<Lane> {
        let rows = self.rows(rect.height());
        let yr = y - rect.top();
        let mut best: Option<(f32, Lane)> = None;
        for lane in ORDER {
            let (ly, lh) = rows[lane.slot()];
            if yr >= ly && yr < ly + lh {
                return Some(lane);
            }
            let d = (yr - (ly + lh * 0.5)).abs();
            if best.map_or(true, |b| d < b.0) {
                best = Some((d, lane));
            }
        }
        if strict {
            None
        } else {
            best.map(|b| b.1)
        }
    }

    fn clip_rect(&self, rect: Rect, c: &Clip) -> Rect {
        let lr = self.lane_rect(rect, c.lane);
        Rect::from_min_max(pos2(self.x_of(rect, c.start), lr.top()), pos2(self.x_of(rect, c.end()), lr.bottom()))
    }

    /// Clip under the pointer and which part of it (body / trim handles).
    fn hit(&self, proj: &Project, rect: Rect, pos: Pos2) -> Option<(usize, Zone)> {
        let lane = self.lane_at(rect, pos.y, true)?;
        for (i, c) in proj.clips.iter().enumerate().rev() {
            if c.lane != lane {
                continue;
            }
            let (x0, x1) = (self.x_of(rect, c.start), self.x_of(rect, c.end()));
            if pos.x >= x0 - 2.0 && pos.x <= x1 + 2.0 {
                let edge = if x1 - x0 > 28.0 { EDGE } else { (x1 - x0) * 0.2 };
                let zone = if pos.x - x0 < edge {
                    Zone::Left
                } else if x1 - pos.x < edge {
                    Zone::Right
                } else {
                    Zone::Body
                };
                return Some((i, zone));
            }
        }
        None
    }

    // ---------------------------------------------------------------- zoom / scroll
    fn zoom_to(&mut self, pps: f64, anchor_t: f64, anchor_dx: f32) {
        self.pps_target = pps.clamp(MIN_PPS, MAX_PPS);
        self.zoom_anchor = Some((anchor_t, anchor_dx));
    }
    /// Immediate zoom around a time (wheel / pinch).
    fn zoom_now(&mut self, f: f64, anchor_t: f64) {
        let dx = (anchor_t - self.scroll) * self.pps;
        self.pps = (self.pps * f).clamp(MIN_PPS, MAX_PPS);
        self.pps_target = self.pps;
        self.zoom_anchor = None;
        self.scroll = (anchor_t - dx / self.pps).max(0.0);
    }
    /// Animated zoom step (keyboard / buttons).
    pub fn zoom_step(&mut self, f: f64, anchor_t: f64) {
        let dx = ((anchor_t - self.scroll) * self.pps) as f32;
        self.zoom_to(self.pps_target * f, anchor_t, dx);
    }
    /// Zoom so the whole project fits.
    pub fn fit(&mut self, total: f64) {
        let w = (self.view.width() - HEAD - 30.0).max(100.0) as f64;
        self.zoom_to(w / total.max(1.0), 0.0, 0.0);
    }

    // ---------------------------------------------------------------- snapping
    fn snap_thr(&self) -> f64 {
        SNAP_PX / self.pps
    }
    fn snap_edit(&mut self, proj: &Project, v: f64, exclude: &[u32], playhead: f64) -> f64 {
        self.snap_line = None;
        if !self.snap {
            return v;
        }
        let mut tg = proj.snap_targets(exclude);
        tg.push(playhead);
        match nearest(&tg, v, self.snap_thr()) {
            Some(x) => {
                self.snap_line = Some(x);
                x
            }
            None => v,
        }
    }

    fn scrub_to(&mut self, proj: &Project, rect: Rect, x: f32, t: &mut f64) {
        let mut tt = self.t_of(rect, x).clamp(0.0, duration());
        self.snap_line = None;
        if self.snap {
            if let Some(s) = nearest(&proj.snap_targets(&[]), tt, self.snap_thr()) {
                tt = s;
                self.snap_line = Some(s);
            }
        }
        *t = tt.clamp(0.0, duration());
    }

    /// Where a media drag from the browser would land: (video track, start time).
    pub fn drop_target(&self, pos: Pos2, proj: &Project, playhead: f64) -> Option<(Lane, f64)> {
        let rect = self.view;
        if !rect.contains(pos) || pos.x < rect.left() + HEAD || pos.y < rect.top() + RULER {
            return None;
        }
        let lane = self.lane_at(rect, pos.y, false)?;
        let lane = if lane.is_video() { lane } else { lane.partner() };
        let mut t = self.t_of(rect, pos.x).max(0.0);
        if self.snap {
            let mut tg = proj.snap_targets(&[]);
            tg.push(playhead);
            if let Some(s) = nearest(&tg, t, self.snap_thr()) {
                t = s;
            }
        }
        Some((lane, t))
    }

    // ---------------------------------------------------------------- interaction
    fn header_buttons(r: Rect) -> (Rect, Rect) {
        let s = 15.0f32.min(r.height() - 4.0).max(10.0);
        let cy = r.center().y;
        (
            Rect::from_center_size(pos2(r.left() + 38.0, cy), vec2(s, s)),
            Rect::from_center_size(pos2(r.left() + 58.0, cy), vec2(s, s)),
        )
    }

    /// Lock / eye / mute buttons. True if something changed.
    fn header_click(&self, rect: Rect, pos: Pos2, proj: &mut Project) -> bool {
        let rows = self.rows(rect.height());
        for lane in ORDER {
            let (y, hh) = rows[lane.slot()];
            let r = Rect::from_min_size(pos2(rect.left(), rect.top() + y), vec2(HEAD, hh));
            if r.contains(pos) {
                let (lock, eye) = Self::header_buttons(r);
                if lock.contains(pos) {
                    proj.locked[lane.slot()] ^= true;
                    return true;
                }
                if eye.contains(pos) {
                    proj.hidden[lane.slot()] ^= true;
                    return true;
                }
            }
        }
        false
    }

    /// Applies one frame of a drag (snapping included). True if the project differs from `d.pre`.
    fn drag_step(&mut self, proj: &mut Project, d: &Drag, pos: Pos2, rect: Rect, playhead: f64) -> bool {
        let Some(o) = d.pre.iter().find(|c| c.id == d.id).cloned() else { return false };
        let tt = self.t_of(rect, pos.x);
        let ids = proj.group_ids(d.id);
        self.snap_line = None;

        let op = if let Some(right) = d.right {
            let nt = self.snap_edit(proj, tt, &ids, playhead);
            DragOp::Roll { new_t: nt, right }
        } else {
            match (d.zone, d.tool) {
                (Zone::Body, Tool::Slip) => DragOp::Slip { delta: tt - d.grab_t },
                (Zone::Body, _) => {
                    let mv = proj.move_set(d.id, &d.sel);
                    let mut ns = (tt - (d.grab_t - o.start)).max(0.0);
                    if self.snap {
                        let mut tg = proj.snap_targets(&mv);
                        tg.push(playhead);
                        let thr = self.snap_thr();
                        let head = nearest(&tg, ns, thr).map(|x| (x - ns, x));
                        let tail = nearest(&tg, ns + o.dur, thr).map(|x| (x - (ns + o.dur), x));
                        let pick = match (head, tail) {
                            (Some(a), Some(b)) => Some(if a.0.abs() <= b.0.abs() { a } else { b }),
                            (a, None) => a,
                            (None, b) => b,
                        };
                        if let Some((delta, line)) = pick {
                            ns += delta;
                            self.snap_line = Some(line);
                        }
                    }
                    DragOp::Move { new_start: ns, new_lane: self.lane_at(rect, pos.y, false) }
                }
                (Zone::Left, tool) => {
                    let ns = self.snap_edit(proj, tt, &ids, playhead);
                    DragOp::TrimL { new_start: ns, ripple: tool == Tool::Ripple }
                }
                (Zone::Right, tool) => {
                    let ne = self.snap_edit(proj, tt, &ids, playhead);
                    DragOp::TrimR { new_end: ne, ripple: tool == Tool::Ripple }
                }
            }
        };
        proj.apply_drag(&d.pre, d.id, &d.sel, op)
    }

    /// Mouse press inside the track area (not ruler / header).
    fn press_tracks(&mut self, proj: &mut Project, rect: Rect, pos: Pos2, mods: Modifiers, playhead: f64) -> bool {
        let mut edited = false;
        let tt = self.t_of(rect, pos.x);
        match self.tool {
            Tool::Hand => self.panning = true,
            Tool::Zoom => {
                let f = if mods.alt { 0.5 } else { 2.0 };
                self.zoom_to(self.pps_target * f, tt, pos.x - rect.left() - HEAD);
            }
            Tool::Razor => {
                if let Some((i, _)) = self.hit(proj, rect, pos) {
                    let id = proj.clips[i].id;
                    let ts = self.snap_edit(proj, tt, &[], playhead);
                    edited = if mods.shift { proj.cut_at(ts) } else { proj.split(id, ts) };
                }
            }
            _ => match self.hit(proj, rect, pos) {
                Some((i, _)) if proj.is_locked(proj.clips[i].lane) => {}
                Some((i, zone)) => {
                    let id = proj.clips[i].id;
                    let grp = if mods.alt { vec![id] } else { proj.group_ids(id) };
                    if mods.shift {
                        if self.sel.contains(&id) {
                            self.sel.retain(|s| !grp.contains(s));
                        } else {
                            for g in grp {
                                if !self.sel.contains(&g) {
                                    self.sel.push(g);
                                }
                            }
                        }
                    } else if !self.sel.contains(&id) {
                        self.sel = grp;
                    }
                    if !self.sel.contains(&id) {
                        return edited; // shift-click deselected it: nothing to drag
                    }
                    // which edit does this press start?
                    let c = proj.clips[i].clone();
                    let mut grabbed = id;
                    let mut right = None;
                    let draggable = match self.tool {
                        Tool::Select | Tool::Slip => true,
                        Tool::Ripple => zone != Zone::Body,
                        Tool::Rect => {
                            // rolling: needs a neighbour touching on the grabbed side
                            let nb = match zone {
                                Zone::Left => proj.clips.iter().find(|x| x.lane == c.lane && (x.end() - c.start).abs() < 1e-6),
                                Zone::Right => proj.clips.iter().find(|x| x.lane == c.lane && (x.start - c.end()).abs() < 1e-6),
                                Zone::Body => None,
                            };
                            match (zone, nb) {
                                (Zone::Left, Some(l)) => {
                                    grabbed = l.id;
                                    right = Some(id);
                                    true
                                }
                                (Zone::Right, Some(r)) => {
                                    right = Some(r.id);
                                    true
                                }
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    if draggable {
                        let zone = if self.tool == Tool::Slip { Zone::Body } else { zone };
                        self.drag = Some(Drag {
                            zone,
                            id: grabbed,
                            right,
                            grab_t: tt,
                            grab_y: pos.y,
                            pre: proj.clips.clone(),
                            sel: self.sel.clone(),
                            tool: self.tool,
                            moved: false,
                            changed: false,
                        });
                    }
                }
                None => {
                    let base = if mods.shift { self.sel.clone() } else { Vec::new() };
                    if !mods.shift {
                        self.sel.clear();
                    }
                    self.marquee = Some((pos, base));
                }
            },
        }
        edited
    }

    /// Returns (edited, seeked).
    pub fn show(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        t: &mut f64,
        proj: &mut Project,
        assets: &Assets,
        ghost: Option<&Ghost>,
    ) -> (bool, bool) {
        self.view = rect;
        let (mut edited, mut seeked) = (false, false);
        let (hover, pressed, down, delta, scroll, zoom, mods, dt) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.delta(),
                i.smooth_scroll_delta,
                i.zoom_delta(),
                i.modifiers,
                i.stable_dt as f64,
            )
        });
        let over = ui.rect_contains_pointer(rect);
        let content_left = rect.left() + HEAD;

        // animated zoom
        if (self.pps_target - self.pps).abs() > self.pps * 0.003 {
            let k = 1.0 - (-dt * 16.0).exp();
            self.pps += (self.pps_target - self.pps) * k;
            if let Some((at, dx)) = self.zoom_anchor {
                self.scroll = (at - dx as f64 / self.pps).max(0.0);
            }
            ui.ctx().request_repaint();
        } else if self.pps != self.pps_target {
            self.pps = self.pps_target;
            if let Some((at, dx)) = self.zoom_anchor.take() {
                self.scroll = (at - dx as f64 / self.pps).max(0.0);
            }
        }

        // ---- press
        if over && pressed {
            if let Some(pos) = hover {
                let yrel = pos.y - rect.top();
                if pos.x < content_left {
                    if yrel >= RULER && self.header_click(rect, pos, proj) {
                        seeked = true; // visibility changes what the monitor shows
                    }
                } else if yrel < RULER {
                    self.scrub = true;
                    self.scrub_to(proj, rect, pos.x, t);
                    seeked = true;
                } else {
                    edited |= self.press_tracks(proj, rect, pos, mods, *t);
                }
            }
        }

        // ---- clip drag (move / trim / slip / roll): recomputed from the pre-drag state every frame
        if let Some(mut d) = self.drag.take() {
            if down {
                if let Some(p) = hover {
                    let tt = self.t_of(rect, p.x);
                    if d.moved || (tt - d.grab_t).abs() * self.pps > 3.0 || (p.y - d.grab_y).abs() > 6.0 {
                        d.moved = true; // dead zone so a plain click never edits
                        d.changed = self.drag_step(proj, &d, p, rect, *t);
                    }
                }
                self.drag = Some(d);
            } else {
                self.snap_line = None;
                if d.changed {
                    if d.zone == Zone::Body && d.tool == Tool::Select {
                        let mv = proj.move_set(d.id, &d.sel);
                        proj.resolve_overwrite(&mv);
                    }
                    proj.push_undo(d.pre);
                    edited = true;
                }
            }
        }

        // ---- marquee
        if let Some((start, base)) = self.marquee.take() {
            if down {
                if let Some(p) = hover {
                    let mr = Rect::from_two_pos(start, p);
                    let mut sel = base.clone();
                    for c in &proj.clips {
                        if !proj.is_locked(c.lane) && self.clip_rect(rect, c).intersects(mr) {
                            for g in proj.group_ids(c.id) {
                                if !sel.contains(&g) {
                                    sel.push(g);
                                }
                            }
                        }
                    }
                    self.sel = sel;
                }
                self.marquee = Some((start, base));
            }
        }

        // ---- ruler scrub / hand pan
        if self.scrub || self.panning {
            if down {
                if self.scrub {
                    if let Some(p) = hover {
                        self.scrub_to(proj, rect, p.x, t);
                        seeked = true;
                    }
                } else {
                    self.scroll = (self.scroll - delta.x as f64 / self.pps).max(0.0);
                }
            } else {
                self.scrub = false;
                self.panning = false;
                self.snap_line = None;
            }
        }

        // ---- edge auto-scroll while dragging near the left/right edge
        if down && (self.scrub || self.drag.is_some() || self.marquee.is_some()) {
            if let Some(p) = hover {
                let speed = 900.0 * dt / self.pps;
                if p.x < content_left + 26.0 {
                    self.scroll = (self.scroll - speed).max(0.0);
                    ui.ctx().request_repaint();
                } else if p.x > rect.right() - 26.0 {
                    self.scroll += speed;
                    ui.ctx().request_repaint();
                }
            }
        }

        // ---- wheel: Alt+wheel / Ctrl+wheel / pinch zoom, plain wheel pans
        if over {
            if mods.alt && scroll.y != 0.0 {
                let a = hover.map_or(*t, |p| self.t_of(rect, p.x));
                self.zoom_now((scroll.y as f64 * 0.006).exp(), a);
            } else if zoom != 1.0 {
                let a = hover.map_or(*t, |p| self.t_of(rect, p.x));
                self.zoom_now(zoom as f64, a);
            } else if scroll != Vec2::ZERO {
                let d = if scroll.x.abs() > scroll.y.abs() { scroll.x } else { scroll.y };
                self.scroll = (self.scroll - d as f64 * 0.8 / self.pps).max(0.0);
            }
        }

        // ---- follow the playhead when it leaves the view (playback / keyboard seek)
        if !self.scrub && (*t - self.last_t).abs() > 1e-9 {
            let vw = (rect.width() - HEAD) as f64 / self.pps;
            if *t < self.scroll || *t > self.scroll + vw {
                self.scroll = (*t - vw * 0.1).max(0.0);
            }
        }
        self.last_t = *t;

        // ---- cursor feedback
        if let Some(d) = &self.drag {
            ui.ctx().set_cursor_icon(if d.zone == Zone::Body && d.tool != Tool::Slip {
                CursorIcon::Grabbing
            } else {
                CursorIcon::ResizeHorizontal
            });
        } else if self.panning {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        } else if over && !self.scrub {
            if let Some(p) = hover.filter(|p| p.x >= content_left && p.y - rect.top() >= RULER) {
                let icon = match self.tool {
                    Tool::Razor => Some(CursorIcon::Crosshair),
                    Tool::Zoom => Some(CursorIcon::ZoomIn),
                    Tool::Hand => Some(CursorIcon::Grab),
                    Tool::Select | Tool::Ripple | Tool::Rect => match self.hit(proj, rect, p) {
                        Some((_, z)) if z != Zone::Body => Some(CursorIcon::ResizeHorizontal),
                        _ => None,
                    },
                    Tool::Slip => None,
                };
                if let Some(i) = icon {
                    ui.ctx().set_cursor_icon(i);
                }
            }
        }

        self.paint(ui, rect, *t, if over && !self.scrub { hover } else { None }, proj, assets, ghost);
        (edited, seeked)
    }

    // ---------------------------------------------------------------- painting
    #[allow(clippy::too_many_arguments)]
    fn paint(&self, ui: &Ui, rect: Rect, t: f64, hover: Option<Pos2>, proj: &Project, assets: &Assets, ghost: Option<&Ghost>) {
        let p_all = ui.painter_at(rect);
        let content = Rect::from_min_max(pos2(rect.left() + HEAD, rect.top()), rect.max);
        let p = p_all.with_clip_rect(content);
        let font = FontId::monospace(8.0);

        // ---- track rows + headers
        let rows = self.rows(rect.height());
        for lane in ORDER {
            let (y, hh) = rows[lane.slot()];
            let lr = Rect::from_min_size(pos2(content.left(), rect.top() + y), vec2(content.width(), hh));
            let locked = proj.locked[lane.slot()];
            let hidden = proj.hidden[lane.slot()];
            p.rect_filled(lr, 0.0, if lane.is_video() { rgba(255, 255, 255, 8) } else { rgba(255, 255, 255, 5) });
            p.line_segment([lr.left_bottom(), lr.right_bottom()], Stroke::new(1.0, rgba(110, 125, 170, 40)));

            let hr = Rect::from_min_size(pos2(rect.left(), rect.top() + y), vec2(HEAD, hh));
            p_all.rect_filled(hr, 0.0, rgba(8, 14, 36, 150));
            p_all.text(
                pos2(hr.left() + 8.0, hr.center().y),
                Align2::LEFT_CENTER,
                lane.name(),
                FontId::monospace(10.0),
                if hidden { MUTED } else { TEXT },
            );
            let (lock, eye) = Self::header_buttons(hr);
            for (r, on, ch) in [(lock, locked, "L"), (eye, hidden, if lane.is_video() { "E" } else { "M" })] {
                p_all.rect_filled(r, 3.0, if on { POWDER } else { rgba(255, 255, 255, 16) });
                p_all.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    ch,
                    FontId::monospace(8.0),
                    if on { rgba(8, 14, 36, 255) } else { MUTED },
                );
            }
        }
        p_all.line_segment(
            [pos2(content.left(), rect.top() + RULER), pos2(content.left(), rect.bottom())],
            Stroke::new(1.0, rgba(110, 125, 170, 70)),
        );

        // ---- ruler
        p.rect_filled(Rect::from_min_size(content.min, vec2(content.width(), RULER)), 0.0, rgba(40, 48, 70, 180));
        p.rect_filled(
            Rect::from_min_size(pos2(content.left(), rect.top() + RULER - 1.0), vec2(content.width(), 1.0)),
            0.0,
            rgba(110, 125, 170, 100),
        );
        let step = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
            .into_iter()
            .find(|s| s * self.pps >= 90.0)
            .unwrap_or(1800.0);
        let mut tt = (self.scroll / step).floor() * step;
        let ry = rect.top() + RULER;
        loop {
            let x = self.x_of(rect, tt);
            if x > rect.right() {
                break;
            }
            p.line_segment([pos2(x, ry - 10.0), pos2(x, ry - 1.0)], Stroke::new(1.0, rgba(139, 148, 158, 140)));
            p.text(
                pos2(x + 4.0, rect.top() + (RULER - 6.0) / 2.0),
                Align2::LEFT_CENTER,
                fmt_tc(tt),
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

        // ---- clips
        for c in &proj.clips {
            self.draw_clip(ui, &p, rect, c, proj, &font, assets);
        }

        // ---- drag-in preview from the browser
        if let Some(g) = ghost {
            let lr = self.lane_rect(rect, g.lane);
            let r = Rect::from_min_max(
                pos2(self.x_of(rect, g.start), lr.top()),
                pos2(self.x_of(rect, g.start + g.dur), lr.bottom()),
            );
            p.rect_filled(r, 3.0, rgba(180, 200, 255, 55));
            p.rect_stroke(r, 3.0, Stroke::new(1.5, TEXT));
            p.text(r.left_top() + vec2(6.0, 3.0), Align2::LEFT_TOP, &g.name, font.clone(), TEXT);
        }

        // ---- marquee
        if let Some((start, _)) = &self.marquee {
            if let Some(h) = hover.or_else(|| ui.input(|i| i.pointer.hover_pos())) {
                let r = Rect::from_two_pos(*start, h);
                p.rect_filled(r, 0.0, rgba(255, 255, 255, 18));
                p.rect_stroke(r, 0.0, Stroke::new(1.0, rgba(201, 209, 217, 170)));
            }
        }

        // ---- snap guide
        if let Some(st) = self.snap_line {
            let x = self.x_of(rect, st);
            p.line_segment(
                [pos2(x, rect.top() + RULER), pos2(x, rect.bottom())],
                Stroke::new(1.0, rgba(255, 214, 102, 220)),
            );
        }

        // ---- hover guide (razor shows where the cut will land)
        if let Some(h) = hover {
            if h.x >= content.left() && h.y - rect.top() >= RULER {
                let razor = self.tool == Tool::Razor;
                p.line_segment(
                    [pos2(h.x, rect.top() + RULER), pos2(h.x, rect.bottom())],
                    Stroke::new(1.0, if razor { rgba(255, 120, 120, 200) } else { rgba(201, 209, 217, 90) }),
                );
            }
        }

        // ---- playhead
        let x = self.x_of(rect, t);
        if x >= content.left() - 6.0 && x <= rect.right() + 6.0 {
            p.line_segment([pos2(x, rect.top() + 12.0), pos2(x, rect.bottom())], Stroke::new(1.5, TEXT));
            p.add(Shape::convex_polygon(
                vec![pos2(x - 5.0, rect.top() + 3.0), pos2(x + 5.0, rect.top() + 3.0), pos2(x, rect.top() + 11.0)],
                TEXT,
                Stroke::NONE,
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_clip(&self, ui: &Ui, p: &Painter, rect: Rect, c: &Clip, proj: &Project, font: &FontId, assets: &Assets) {
        let lr = self.clip_rect(rect, c);
        let (x0, x1, y, hh) = (lr.left(), lr.right(), lr.top(), lr.height());
        if x1 < rect.left() + HEAD || x0 > rect.right() {
            return; // culled
        }
        let r = Rect::from_min_max(pos2(x0 + 0.5, y), pos2((x1 - 0.5).max(x0 + 1.5), y + hh));
        let p = p.with_clip_rect(r.intersect(p.clip_rect()));
        let vis_l = x0.max(rect.left() + HEAD);
        let vis_r = x1.min(rect.right());

        if !c.lane.is_video() {
            p.rect_filled(r, 0.0, rgba(40, 50, 70, 200));
            let mid = y + LABEL + (hh - LABEL) / 2.0;
            let amp = ((hh - LABEL - 4.0) / 2.0).max(2.0);
            let w = wave();
            let mut m = Mesh::default();
            let mut x = vis_l;
            while x < vis_r {
                let tt = (x - x0) as f64 / self.pps + c.src_off;
                let a = match c.media {
                    Some(mid) => assets
                        .peak(mid, tt, tt + 2.0 / self.pps)
                        .map_or(1.0, |pk| pk.min(1.0).powf(0.6) * amp),
                    None => w[((tt * 20.0) as usize).min(w.len() - 1)] * amp,
                }
                .max(0.6);
                m.add_colored_rect(Rect::from_min_max(pos2(x, mid - a), pos2(x + 1.0, mid + a)), VISTA);
                x += 2.0;
            }
            p.add(Shape::mesh(m));
        } else if c.name.starts_with("Title") {
            let mut m = Mesh::default();
            push_rect_gradient(&mut m, r, rgba(50, 64, 92, 255), rgba(30, 38, 56, 255));
            p.add(Shape::mesh(m));
        } else {
            let th = (hh - LABEL).max(8.0);
            let asp = c.media.and_then(|mid| assets.aspect(mid)).unwrap_or(16.0 / 9.0);
            let tw = (th * asp).max(24.0);
            let ty = y + LABEL;
            let mut i = ((vis_l - x0).max(0.0) / tw) as u32;
            let mut x = x0 + i as f32 * tw;
            let mut m = Mesh::default();
            while x < vis_r {
                let tile = Rect::from_min_size(pos2(x, ty), vec2(tw + 1.0, th));
                let src_t = c.src_off + ((x + tw * 0.5 - x0) as f64 / self.pps);
                match c.media.and_then(|mid| assets.thumb_at(mid, src_t)) {
                    Some((tex, _)) => {
                        p.image(tex.id(), tile, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        let hue = thumb_hue(c.seed, i);
                        push_rect_gradient(&mut m, tile, hsv(hue, 35.0, 55.0), hsv(hue, 45.0, 25.0));
                    }
                }
                x += tw;
                i += 1;
            }
            p.add(Shape::mesh(m));
        }

        // label strip
        p.rect_filled(Rect::from_min_size(pos2(x0, y), vec2(x1 - x0, LABEL)), 0.0, rgba(40, 48, 70, 230));
        let mut room = x1 - x0 - 12.0;
        if !c.fx.is_identity() && c.lane.is_video() && x1 - x0 > 60.0 {
            p.text(pos2(x1 - 6.0, y + LABEL * 0.5), Align2::RIGHT_CENTER, "fx", font.clone(), POWDER);
            room -= 16.0;
        }
        if x1 - x0 > 40.0 {
            let g = elided(ui, &c.name, font.clone(), TEXT, room.max(10.0));
            p.galley(pos2(x0 + 6.0, y + (LABEL - g.size().y) / 2.0), g, TEXT);
        }

        // track state overlays
        if proj.hidden[c.lane.slot()] {
            p.rect_filled(r, 0.0, rgba(0, 0, 0, 120));
        }
        if proj.locked[c.lane.slot()] {
            p.rect_filled(r, 0.0, rgba(0, 0, 0, 70));
        }
        if self.sel.contains(&c.id) {
            ui.painter_at(Rect::from_min_max(pos2(rect.left() + HEAD, rect.top()), rect.max))
                .rect_stroke(r, 3.0, Stroke::new(2.0, TEXT));
        }
    }
}

// ------------------------------------------------------------------ toolbar + panel
fn pill(ui: &mut Ui, text: &str, on: bool) -> Response {
    let w = text.chars().count() as f32 * 6.6 + 20.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click());
    let bg = if on {
        rgba(180, 200, 255, 70)
    } else if resp.hovered() {
        rgba(255, 255, 255, 26)
    } else {
        rgba(255, 255, 255, 12)
    };
    ui.painter().rect_filled(r, 11.0, bg);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, FontId::monospace(10.0), if on { TEXT } else { MUTED });
    resp
}

/// Returns (edited, seeked).
pub fn timeline_panel(
    ui: &mut Ui,
    tl: &mut Timeline,
    t: &mut f64,
    project: &str,
    levels: (f32, f32),
    proj: &mut Project,
    assets: &Assets,
    ghost: Option<&Ghost>,
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
            ui.add_space(8.0);
            if pill(ui, "Snap", tl.snap).on_hover_text("Snapping (S)").clicked() {
                tl.snap = !tl.snap;
            }
            if pill(ui, "Fit", false).on_hover_text("Zoom to fit (\\)").clicked() {
                tl.fit(proj.end().max(1.0));
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
        out = tl.show(ui, view, t, proj, assets, ghost);
        level_meter(ui, meter, levels.0, levels.1);
    });
    out
}
