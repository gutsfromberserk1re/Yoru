//! Effect Controls: one row per parameter (stopwatch, slider, value, keyframe navigation) and a
//! value graph with draggable keyframes and per-key easing. Everything edits the selected video
//! clip's `Fx`; a slider/graph drag becomes a single undo step.

use super::*;
use crate::project::{Ease, Fx, NPARAM, PARAMS};

pub struct Inspector {
    pub tab: usize,
    sel_param: usize,
    sel_key: Option<usize>,
    graph_key: Option<usize>,
    /// Clip list from before the current edit; pushed as one undo step when the mouse is released.
    session: Option<Vec<Clip>>,
    /// Timeline time the app should jump to (keyframe navigation).
    pub goto: Option<f64>,
}

impl Inspector {
    pub fn new() -> Self {
        Self { tab: 0, sel_param: 0, sel_key: None, graph_key: None, session: None, goto: None }
    }
}

const KEY_BLUE: Color32 = Color32::from_rgb(120, 170, 255);

/// Returns true if an effect value changed this frame.
pub fn inspector_panel(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &[u32], t: f64) -> bool {
    let mut changed = false;
    glass(14.0, 8.0, 14.0, 12.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 4.0;
        tab_strip(ui, &mut ins.tab, &["Effect Controls", "Text", "Essential Graphics"]);
        ui.add_space(6.0);
        if ins.tab == 0 {
            changed = effect_controls(ui, ins, proj, sel, t);
        } else {
            placeholder(ui, "Not available yet.");
        }
    });
    changed
}

fn effect_controls(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &[u32], t: f64) -> bool {
    let clip = sel.iter().filter_map(|id| proj.clip(*id)).find(|c| c.lane.is_video()).cloned();
    let Some(clip) = clip else {
        placeholder(ui, "Select a video clip on the timeline to edit its effects.");
        return false;
    };
    let ts = clip.fx_time(t); // keyframe time = source seconds
    let snapshot = proj.clips.clone();
    let mut fx = clip.fx.clone();
    let mut changed = false;
    let mut reset = false;

    ui.horizontal(|ui| {
        ui.label(RichText::new(&clip.name).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !fx.is_identity() && ui.small_button("Reset").clicked() {
                reset = true;
            }
        });
    });
    if t < clip.start || t >= clip.end() {
        ui.label(RichText::new("Playhead is outside this clip: values shown at its nearest frame.").size(9.0).color(MUTED));
    }

    let graph_h = 190.0;
    let list_h = (ui.available_height() - graph_h - 30.0).max(110.0);
    ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
        let mut group = "";
        for i in 0..NPARAM {
            if PARAMS[i].group != group {
                group = PARAMS[i].group;
                ui.add_space(6.0);
                ui.label(RichText::new(group).color(POWDER).strong());
            }
            param_row(ui, ins, &mut fx, i, ts, &clip, &mut changed);
        }
    });

    ui.add_space(6.0);
    graph(ui, ins, &mut fx, &clip, ts, &mut changed);

    if reset {
        fx = Fx::default();
        changed = true;
    }
    if changed && fx != clip.fx {
        if ins.session.is_none() {
            ins.session = Some(snapshot);
        }
        if let Some(c) = proj.clip_mut(clip.id) {
            c.fx = fx;
        }
    }
    // one undo step per gesture: close it once the mouse is up
    if ins.session.is_some() && !ui.input(|i| i.pointer.any_down()) {
        if let Some(pre) = ins.session.take() {
            proj.push_undo(pre);
        }
    }
    changed
}

fn draw_stopwatch(p: &Painter, r: Rect, on: bool, hot: bool) {
    let col = if on { KEY_BLUE } else if hot { TEXT } else { MUTED };
    let c = r.center() + vec2(0.0, 1.0);
    p.circle_stroke(c, 5.0, Stroke::new(1.4, col));
    p.line_segment([c, c + vec2(0.0, -3.0)], Stroke::new(1.3, col));
    p.line_segment([c + vec2(-1.5, -6.5), c + vec2(1.5, -6.5)], Stroke::new(1.4, col));
}

fn diamond(c: Pos2, s: f32) -> Vec<Pos2> {
    vec![c + vec2(0.0, -s), c + vec2(s, 0.0), c + vec2(0.0, s), c + vec2(-s, 0.0)]
}

fn param_row(ui: &mut Ui, ins: &mut Inspector, fx: &mut Fx, i: usize, ts: f64, clip: &Clip, changed: &mut bool) {
    let def = &PARAMS[i];
    let animated = fx.is_animated(i);
    let on_key = fx.key_near(i, ts).is_some();
    let (prev, next) = (fx.prev_key(i, ts), fx.next_key(i, ts));

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        let (r, resp) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::click());
        draw_stopwatch(ui.painter(), r, animated, resp.hovered());
        if resp.on_hover_text("Animate this value with keyframes").clicked() {
            fx.toggle_anim(i, ts);
            ins.sel_param = i;
            *changed = true;
        }

        let picked = ins.sel_param == i;
        let name = ui.add_sized(
            [70.0, 16.0],
            Label::new(RichText::new(def.name).size(11.0).color(if picked { TEXT } else { MUTED })).sense(Sense::click()),
        );
        if name.clicked() {
            ins.sel_param = i;
        }

        let mut v = fx.value(i, ts);
        let before = v;
        let w = (ui.available_width() - 122.0).max(36.0);
        slider(ui, &mut v, def.lo, def.hi, w);
        if (v - before).abs() > 1e-4 {
            fx.set_value(i, ts, v);
            ins.sel_param = i;
            *changed = true;
        }

        let (br, _) = ui.allocate_exact_size(vec2(46.0, 18.0), Sense::hover());
        ui.painter().rect_filled(br, 4.0, rgba(8, 14, 36, 200));
        ui.painter().text(
            br.center(),
            Align2::CENTER_CENTER,
            format!("{}{}", v.round() as i32, def.suffix),
            FontId::monospace(10.0),
            TEXT,
        );

        // previous key / add-remove key / next key
        let (pr, presp) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::click());
        let (dr, dresp) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::click());
        let (nr, nresp) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::click());
        let tri = |c: Pos2, dir: f32, on: bool| {
            let col = if on { TEXT } else { rgba(139, 148, 158, 60) };
            Shape::convex_polygon(vec![c + vec2(-2.5 * dir, 0.0), c + vec2(2.5 * dir, -4.0), c + vec2(2.5 * dir, 4.0)], col, Stroke::NONE)
        };
        ui.painter().add(tri(pr.center(), -1.0, prev.is_some()));
        ui.painter().add(tri(nr.center(), 1.0, next.is_some()));
        let dc = dr.center();
        if on_key {
            ui.painter().add(Shape::convex_polygon(diamond(dc, 5.0), KEY_BLUE, Stroke::NONE));
        } else {
            let col = if animated { KEY_BLUE } else { rgba(139, 148, 158, 90) };
            ui.painter().add(Shape::closed_line(diamond(dc, 5.0), Stroke::new(1.2, col)));
        }
        if presp.clicked() {
            if let Some(k) = prev {
                ins.goto = Some(clip.start + (k - clip.src_off));
            }
        }
        if nresp.clicked() {
            if let Some(k) = next {
                ins.goto = Some(clip.start + (k - clip.src_off));
            }
        }
        if dresp.on_hover_text("Add / remove a keyframe at the playhead").clicked() {
            fx.toggle_key(i, ts);
            ins.sel_param = i;
            *changed = true;
        }
    });
}

fn graph(ui: &mut Ui, ins: &mut Inspector, fx: &mut Fx, clip: &Clip, ts: f64, changed: &mut bool) {
    let i = ins.sel_param.min(NPARAM - 1);
    let def = &PARAMS[i];
    ui.label(RichText::new(format!("Value graph  -  {}", def.name)).size(10.0).color(MUTED));
    let h = (ui.available_height() - 30.0).clamp(70.0, 170.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, rgba(8, 14, 36, 170));

    let plot = rect.shrink(10.0);
    let (t0, t1) = (clip.src_off, clip.src_off + clip.dur.max(0.1));
    let span = (def.hi - def.lo).max(1e-3);
    let x_of = |t: f64| plot.left() + ((t - t0) / (t1 - t0)) as f32 * plot.width();
    let y_of = |v: f32| plot.bottom() - (v - def.lo) / span * plot.height();
    let t_of = |x: f32| (t0 + ((x - plot.left()) / plot.width().max(1.0)) as f64 * (t1 - t0)).clamp(t0, t1);
    let v_of = |y: f32| (def.lo + (plot.bottom() - y) / plot.height().max(1.0) * span).clamp(def.lo, def.hi);

    for k in 0..=4 {
        let y = plot.top() + plot.height() * k as f32 / 4.0;
        p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, rgba(110, 125, 170, 50)));
    }
    // curve
    let n = 160;
    let pts: Vec<Pos2> = (0..=n)
        .map(|k| {
            let tt = t0 + (t1 - t0) * k as f64 / n as f64;
            pos2(x_of(tt), y_of(fx.value(i, tt)))
        })
        .collect();
    p.add(Shape::line(pts, Stroke::new(2.0, if fx.is_animated(i) { TEXT } else { rgba(201, 209, 217, 120) })));
    // playhead
    if ts >= t0 && ts <= t1 {
        let x = x_of(ts);
        p.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, rgba(255, 214, 102, 200)));
    }
    if !fx.is_animated(i) {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Click the stopwatch to animate - double-click here to add keys",
            FontId::monospace(9.0),
            rgba(139, 148, 158, 150),
        );
    }
    // keys
    for (idx, k) in fx.keys[i].iter().enumerate() {
        let c = pos2(x_of(k.t), y_of(k.v));
        let on = ins.sel_key == Some(idx);
        p.add(Shape::convex_polygon(diamond(c, if on { 6.5 } else { 5.5 }), if on { POWDER } else { KEY_BLUE }, Stroke::new(1.0, rgba(8, 14, 36, 255))));
    }

    let near = |fx: &Fx, pos: Pos2| fx.keys[i].iter().position(|k| (pos - pos2(x_of(k.t), y_of(k.v))).length() < 10.0);
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            ins.graph_key = near(fx, pos);
            if ins.graph_key.is_some() {
                ins.sel_key = ins.graph_key;
            }
        }
    }
    if resp.dragged() {
        if let (Some(k), Some(pos)) = (ins.graph_key, resp.interact_pointer_pos()) {
            if k < fx.keys[i].len() {
                let nk = fx.move_key(i, k, t_of(pos.x), v_of(pos.y));
                ins.graph_key = Some(nk);
                ins.sel_key = Some(nk);
                *changed = true;
            }
        }
    }
    if resp.drag_stopped() {
        ins.graph_key = None;
    }
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            ins.sel_key = near(fx, pos);
        }
    }
    if resp.double_clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let tt = t_of(pos.x);
            if !fx.is_animated(i) {
                fx.toggle_anim(i, tt);
            }
            fx.set_value(i, tt, v_of(pos.y));
            ins.sel_key = fx.key_near(i, tt);
            *changed = true;
        }
    }
    if resp.secondary_clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            if let Some(k) = near(fx, pos) {
                let kt = fx.keys[i][k].t;
                fx.toggle_key(i, kt);
                ins.sel_key = None;
                *changed = true;
            }
        }
    }

    // easing of the selected key (applies to the segment up to the next key)
    if let Some(k) = ins.sel_key.filter(|k| *k < fx.keys[i].len()) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for e in Ease::ALL {
                if ui.selectable_label(fx.keys[i][k].ease == e, RichText::new(e.label()).size(10.0)).clicked() {
                    fx.keys[i][k].ease = e;
                    *changed = true;
                }
            }
        });
    } else if fx.is_animated(i) {
        ui.label(RichText::new("Click a diamond to choose its easing - right-click removes it").size(9.0).color(MUTED));
    }
}
