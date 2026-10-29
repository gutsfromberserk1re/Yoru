//! Effect Controls: one row per parameter (stopwatch, slider, value, keyframe navigation) and a
//! value graph with draggable keyframes and per-key easing. Everything edits the selected video
//! clip's `Fx`; a slider/graph drag becomes a single undo step.

use super::*;
use crate::project::{AudioPreset, Ease, EffectKind, Fx, Gfx, GfxKind, Key, NPARAM, PARAMS, TransitionKind};

pub struct Inspector {
    pub tab: usize,
    sel_param: usize,
    sel_key: Option<usize>,
    graph_key: Option<usize>,
    sel_effect: Option<usize>,
    sel_effect_clip: Option<u32>,
    /// Clip list from before the current edit; pushed as one undo step when the mouse is released.
    session: Option<Vec<Clip>>,
    /// Timeline time the app should jump to (keyframe navigation).
    pub goto: Option<f64>,
}

impl Inspector {
    pub fn new() -> Self {
        Self { tab: 0, sel_param: 0, sel_key: None, graph_key: None, sel_effect: None, sel_effect_clip: None, session: None, goto: None }
    }
}

const KEY_BLUE: Color32 = Color32::from_rgb(120, 170, 255);

/// Returns true if an effect value changed this frame.
pub fn inspector_panel(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &mut Vec<u32>, t: f64) -> bool {
    let mut changed = false;
    glass(14.0, 8.0, 14.0, 12.0).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 4.0;
        tab_strip(ui, &mut ins.tab, &["Effect Controls", "Text", "Essential Graphics", "Audio"]);
        ui.add_space(6.0);
        match ins.tab {
            0 => changed = effect_controls(ui, ins, proj, sel, t),
            1 => changed = text_panel(ui, ins, proj, sel, t),
            2 => changed = graphics_panel(ui, ins, proj, sel, t),
            _ => changed = audio_panel(ui, ins, proj, sel, t),
        }
    });
    // A continuous gesture (slider drag / keyframe drag) is one undo step. Text edits and
    // discrete controls commit immediately because the pointer is already up.
    if ins.session.is_some() && !ui.input(|i| i.pointer.any_down()) {
        if let Some(pre) = ins.session.take() { proj.push_undo(pre); }
    }
    changed
}

fn effect_controls(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &[u32], t: f64) -> bool {
    let clip = sel.iter().filter_map(|id| proj.clip(*id)).find(|c| c.lane.is_video()).cloned();
    let Some(clip) = clip else {
        placeholder(ui, "Select a video clip on the timeline to edit its effects.");
        return false;
    };
    let stack_changed = if clip.media.is_some() && clip.gfx.is_none() {
        effect_stack(ui, ins, proj, clip.id, t)
    } else {
        false
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

    // Keep the controls and value graph side by side, like a proper editing inspector, instead
    // of pushing the graph below every parameter row.
    ui.columns(2, |columns| {
        let list_h = columns[0].available_height().max(140.0);
        ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(&mut columns[0], |ui| {
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
        graph(&mut columns[1], ins, &mut fx, &clip, ts, &mut changed);
    });

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
    changed || stack_changed
}

fn effect_stack(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, clip_id: u32, timeline_t: f64) -> bool {
    let Some(clip) = proj.clip(clip_id).cloned() else { return false; };
    let pre = proj.clips.clone();
    let mut effects = clip.effects.clone();
    let mut modified = false;
    let mut remove_at = None;
    if ins.sel_effect_clip != Some(clip_id) {
        ins.sel_effect_clip = Some(clip_id);
        ins.sel_effect = if effects.is_empty() { None } else { Some(effects.len() - 1) };
    }
    if !effects.is_empty() && ins.sel_effect.is_none() { ins.sel_effect = Some(effects.len() - 1); }
    // Inbound transition controls are kept with the selected clip's actual effect stack.
    ui.horizontal(|ui| {
        ui.label(RichText::new("TRANSITION IN").color(MUTED).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if clip.transition_in.is_some() && ui.small_button("Remove").clicked() {
                if let Some(c) = proj.clip_mut(clip_id) { c.transition_in = None; }
                proj.push_undo(pre.clone());
                modified = true;
            }
        });
    });
    let mut transition_kind = clip.transition_in.map(|tr| tr.kind).unwrap_or(TransitionKind::Dissolve);
    let mut transition_duration = clip.transition_in.map(|tr| tr.duration).unwrap_or(0.8);
    let mut transition_enabled = clip.transition_in.map(|tr| tr.enabled).unwrap_or(true);
    let mut apply_transition = false;
    ui.horizontal(|ui| {
        ComboBox::from_id_salt(("transition_kind", clip_id))
            .selected_text(transition_kind.label())
            .show_ui(ui, |ui| for kind in TransitionKind::ALL { ui.selectable_value(&mut transition_kind, kind, kind.label()); });
        if ui.small_button(if clip.transition_in.is_some() { "Update" } else { "+ Apply" }).clicked() { apply_transition = true; }
    });
    if clip.transition_in.is_some() {
        ui.horizontal(|ui| {
            ui.checkbox(&mut transition_enabled, "Enabled");
            ui.add(Slider::new(&mut transition_duration, 0.05..=clip.dur.min(5.0) as f32).text("Duration"));
        });
    }
    if apply_transition || clip.transition_in.map_or(false, |tr| tr.duration != transition_duration || tr.enabled != transition_enabled || tr.kind != transition_kind) {
        if ins.session.is_none() { ins.session = Some(pre.clone()); }
        if let Some(c) = proj.clip_mut(clip_id) {
            c.transition_in = Some(crate::project::Transition { kind: transition_kind, duration: transition_duration.clamp(0.05, clip.dur.min(5.0) as f32), enabled: transition_enabled });
        }
        modified = true;
    }
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("APPLIED EFFECTS").color(MUTED).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("{}", effects.len())).size(9.0).color(MUTED));
        });
    });
    if effects.is_empty() {
        ui.label(RichText::new("Drag an effect from the Effects browser onto this clip.").size(10.0).color(MUTED));
    } else {
        ScrollArea::vertical().max_height(116.0).auto_shrink([false, false]).show(ui, |ui| {
            for i in 0..effects.len() {
                ui.horizontal(|ui| {
                    let mut enabled = effects[i].enabled;
                    if ui.checkbox(&mut enabled, "").changed() { effects[i].enabled = enabled; modified = true; }
                    let label = format!("{}  {}", if effects[i].enabled { "●" } else { "○" }, effects[i].kind.label());
                    if ui.selectable_label(ins.sel_effect == Some(i), label).clicked() { ins.sel_effect = Some(i); }
                    if ui.small_button("↑").on_hover_text("Move effect earlier in the stack").clicked() && i > 0 {
                        effects.swap(i, i - 1); ins.sel_effect = Some(i - 1); modified = true;
                    }
                    if ui.small_button("↓").on_hover_text("Move effect later in the stack").clicked() && i + 1 < effects.len() {
                        effects.swap(i, i + 1); ins.sel_effect = Some(i + 1); modified = true;
                    }
                    if ui.small_button("×").on_hover_text("Remove effect").clicked() {
                        remove_at = Some(i); modified = true;
                    }
                });
            }
        });
        if let Some(i) = remove_at {
            if i < effects.len() { effects.remove(i); }
            ins.sel_effect = if effects.is_empty() { None } else { Some(i.min(effects.len() - 1)) };
        }
        if let Some(idx) = ins.sel_effect.filter(|i| *i < effects.len()) {
            let ef = &mut effects[idx];
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(ef.kind.label()).color(POWDER).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Reset").clicked() { ef.params = ef.kind.defaults(); ef.keys = Default::default(); modified = true; }
                });
            });
            let local_time = (timeline_t - clip.start).max(0.0);
            for (pi, def) in ef.kind.params().iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized([76.0, 18.0], Label::new(RichText::new(def.name).size(10.0).color(MUTED)));
                    let mut v = ef.param_at(pi, local_time);
                    let before = v;
                    let slider_w = (ui.available_width() - 98.0).max(45.0);
                    ui.add_sized([slider_w, 18.0], Slider::new(&mut v, def.min..=def.max).show_value(false));
                    ui.add_sized([58.0, 18.0], DragValue::new(&mut v).clamp_range(def.min..=def.max).speed(((def.max-def.min)/500.0).max(0.01)).suffix(def.suffix));
                    if (v - before).abs() > 1e-4 { ef.set_param_at(pi, local_time, v); modified = true; }
                    let keyed = ef.key_near(pi, local_time).is_some();
                    if ui.small_button(if keyed { "◆" } else { "◇" }).on_hover_text(if keyed { "Remove keyframe at the playhead" } else { "Add keyframe at the playhead" }).clicked() {
                        ef.toggle_key(pi, local_time);
                        modified = true;
                    }
                });
                if let Some(k) = ef.key_near(pi, local_time) {
                    ui.horizontal(|ui| {
                        ui.add_space(78.0);
                        ui.label(RichText::new("Key easing").size(9.0).color(MUTED));
                        let mut ease = ef.keys[pi][k].ease;
                        ComboBox::from_id_salt(("effect_key_ease", clip_id, idx, pi))
                            .selected_text(ease.label())
                            .show_ui(ui, |ui| for e in Ease::ALL { ui.selectable_value(&mut ease, e, e.label()); });
                        if ease != ef.keys[pi][k].ease { ef.keys[pi][k].ease = ease; modified = true; }
                    });
                }
            }
            if ef.kind == EffectKind::Flash {
                ui.label(RichText::new("Flash begins at the timeline position where you dropped it.").size(9.0).color(MUTED));
            }
        }
    }
    if modified && effects != clip.effects {
        if ins.session.is_none() { ins.session = Some(pre); }
        if let Some(c) = proj.clip_mut(clip_id) { c.effects = effects; }
    }
    modified
}

fn selected_graphic(proj: &Project, sel: &[u32]) -> Option<(u32, crate::project::Gfx)> {
    sel.iter().find_map(|id| proj.clip(*id).and_then(|c| c.gfx.clone().map(|g| (*id, g))))
}

fn add_graphic_button(ui: &mut Ui, proj: &mut Project, sel: &mut Vec<u32>, t: f64, kind: GfxKind) -> bool {
    let (name, gfx) = match kind {
        GfxKind::Text => ("Title", Gfx::title("Your title")),
        GfxKind::Rect => ("Rectangle", Gfx::rect()),
        GfxKind::Ellipse => ("Ellipse", Gfx::ellipse()),
    };
    if ui.button(format!("+ {name}")).clicked() {
        let id = proj.add_graphic(t, gfx, name);
        sel.clear();
        sel.push(id);
        return true;
    }
    false
}

fn set_text_track(fx: &mut Fx, index: usize, keys: &[(f64, f32, Ease)]) {
    if index >= NPARAM { return; }
    fx.keys[index].clear();
    for &(t, v, ease) in keys {
        fx.keys[index].push(Key { t: t.max(0.0), v: v.clamp(PARAMS[index].lo, PARAMS[index].hi), ease });
    }
}

fn apply_text_animation(clip: &mut Clip, preset: &str) {
    let base = clip.fx.base;
    let start = clip.src_off.max(0.0);
    let end = start + clip.dur.max(0.1);
    let d = clip.dur.min(0.7).max(0.18);
    let x = base[0];
    let y = base[1];
    let scale = base[2];
    let opacity = base[4];
    match preset {
        "Fade In" => set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::EaseOut), (start + d, opacity, Ease::EaseOut)]),
        "Rise Up" => {
            set_text_track(&mut clip.fx, 1, &[(start, (y + 90.0).clamp(0.0,1080.0), Ease::EaseOut), (start + d, y, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::EaseOut), (start + d, opacity, Ease::EaseOut)]);
        }
        "Slide Left" => {
            set_text_track(&mut clip.fx, 0, &[(start, (x - 180.0).clamp(0.0,1920.0), Ease::EaseOut), (start + d, x, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Slide Right" => {
            set_text_track(&mut clip.fx, 0, &[(start, (x + 180.0).clamp(0.0,1920.0), Ease::EaseOut), (start + d, x, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Drop In" => {
            set_text_track(&mut clip.fx, 1, &[(start, (y - 130.0).clamp(0.0,1080.0), Ease::EaseIn), (start + d, y, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Pop In" => {
            set_text_track(&mut clip.fx, 2, &[(start, (scale * 0.78).clamp(1.0,400.0), Ease::EaseOut), (start + d, scale, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Fade Out" => set_text_track(&mut clip.fx, 4, &[(end - d, opacity, Ease::Linear), (end, 0.0, Ease::EaseIn)]),
        "Float In" => {
            set_text_track(&mut clip.fx, 1, &[(start, (y + 28.0).clamp(0.0,1080.0), Ease::EaseOut), (start + d, y, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::EaseOut), (start + d, opacity, Ease::EaseOut)]);
        }
        "Zoom In" => {
            set_text_track(&mut clip.fx, 2, &[(start, (scale * 0.55).clamp(0.0,400.0), Ease::EaseOut), (start + d, scale, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Soft Zoom" => {
            set_text_track(&mut clip.fx, 2, &[(start, (scale * 0.94).clamp(0.0,400.0), Ease::EaseOut), (start + d, scale, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::EaseOut), (start + d, opacity, Ease::EaseOut)]);
        }
        "Bounce In" => {
            set_text_track(&mut clip.fx, 2, &[(start, (scale * 0.72).clamp(0.0,400.0), Ease::EaseOut), (start + d * 0.72, (scale * 1.12).clamp(0.0,400.0), Ease::EaseOut), (start + d, scale, Ease::EaseInOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d * 0.55, opacity, Ease::EaseOut)]);
        }
        "Slam In" => {
            set_text_track(&mut clip.fx, 2, &[(start, (scale * 1.38).clamp(0.0,400.0), Ease::EaseIn), (start + d, scale, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 1, &[(start, (y - 45.0).clamp(0.0,1080.0), Ease::EaseIn), (start + d, y, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d * 0.55, opacity, Ease::EaseOut)]);
        }
        "Rotate In" => {
            let r = base[3];
            set_text_track(&mut clip.fx, 3, &[(start, (r - 35.0).clamp(-180.0,180.0), Ease::EaseOut), (start + d, r, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::EaseOut)]);
        }
        "Drift Up" => set_text_track(&mut clip.fx, 1, &[(start, (y + 45.0).clamp(0.0,1080.0), Ease::EaseOut), (start + d, y, Ease::EaseOut)]),
        "Drift Down" => set_text_track(&mut clip.fx, 1, &[(start, (y - 45.0).clamp(0.0,1080.0), Ease::EaseOut), (start + d, y, Ease::EaseOut)]),
        "Scale Pulse" => set_text_track(&mut clip.fx, 2, &[(start, scale, Ease::EaseOut), (start + d * 0.5, (scale * 1.10).clamp(0.0,400.0), Ease::EaseInOut), (start + d, scale, Ease::EaseOut)]),
        "Soft Reveal" => {
            // A gentle left-to-right entrance built from transform/opacity keys, not per-character typing.
            set_text_track(&mut clip.fx, 0, &[(start, (x - 30.0).clamp(0.0,1920.0), Ease::Linear), (start + d, x, Ease::EaseOut)]);
            set_text_track(&mut clip.fx, 4, &[(start, 0.0, Ease::Linear), (start + d, opacity, Ease::Linear)]);
        }
        _ => {}
    }
}

fn text_panel(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &mut Vec<u32>, _t: f64) -> bool {
    let Some((id, mut gfx)) = selected_graphic(proj, sel) else {
        placeholder(ui, "Select a text graphic, or create one in Essential Graphics.");
        return false;
    };
    if gfx.kind != GfxKind::Text {
        ui.label(RichText::new("The selected item is a shape. Use Essential Graphics for its appearance.").color(MUTED));
        return false;
    }
    ui.label(RichText::new("TEXT").color(MUTED).strong());
    let pre = proj.clips.clone();
    let mut changed = false;
    ui.label("Content");
    changed |= ui.add(egui::TextEdit::multiline(&mut gfx.text).desired_rows(3).desired_width(f32::INFINITY)).changed();
    ui.horizontal(|ui| {
        ui.label("Font size");
        changed |= ui.add(egui::Slider::new(&mut gfx.size, 8.0..=240.0)).changed();
    });
    ui.horizontal(|ui| {
        ui.label("Font");
        changed |= ui.selectable_value(&mut gfx.mono, false, "Sans").changed();
        changed |= ui.selectable_value(&mut gfx.mono, true, "Mono").changed();
    });
    ui.horizontal(|ui| {
        ui.label("Alignment");
        changed |= ui.selectable_value(&mut gfx.align, 0, "Left").changed();
        changed |= ui.selectable_value(&mut gfx.align, 1, "Center").changed();
        changed |= ui.selectable_value(&mut gfx.align, 2, "Right").changed();
    });
    changed |= ui.checkbox(&mut gfx.shadow, "Drop shadow").changed();
    ui.separator();
    ui.label(RichText::new("TEXT ANIMATION PRESETS").color(MUTED).strong());
    ui.label(RichText::new("Presets create editable transform keyframes on this title clip.").size(9.0).color(MUTED));
    let mut apply_anim: Option<&'static str> = None;
    ui.horizontal_wrapped(|ui| {
        for name in ["Fade In", "Rise Up", "Slide Left", "Slide Right", "Drop In", "Pop In", "Float In", "Fade Out", "Zoom In", "Soft Zoom", "Bounce In", "Slam In", "Rotate In", "Drift Up", "Drift Down", "Scale Pulse", "Soft Reveal"] {
            if ui.small_button(name).clicked() { apply_anim = Some(name); }
        }
    });
    if changed {
        if ins.session.is_none() { ins.session = Some(pre.clone()); }
        if let Some(c) = proj.clip_mut(id) { c.gfx = Some(gfx); }
    }
    if let Some(preset) = apply_anim {
        if ins.session.is_none() { ins.session = Some(pre.clone()); }
        if let Some(c) = proj.clip_mut(id) { apply_text_animation(c, preset); }
        changed = true;
    }
    changed
}

fn graphics_panel(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &mut Vec<u32>, t: f64) -> bool {
    let mut created = false;
    ui.label(RichText::new("CREATE GRAPHIC").color(MUTED).strong());
    ui.horizontal(|ui| {
        created |= add_graphic_button(ui, proj, sel, t, GfxKind::Text);
        created |= add_graphic_button(ui, proj, sel, t, GfxKind::Rect);
        created |= add_graphic_button(ui, proj, sel, t, GfxKind::Ellipse);
    });
    ui.separator();
    ui.label(RichText::new("GRAPHIC LAYERS").color(MUTED).strong());
    let graphics: Vec<(u32, String, bool)> = proj.clips.iter().filter(|c| c.gfx.is_some())
        .map(|c| (c.id, c.name.clone(), sel.contains(&c.id))).collect();
    ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
        for (id, name, selected) in graphics {
            if ui.selectable_label(selected, format!("◇  {name}")).clicked() {
                sel.clear(); sel.push(id);
            }
        }
    });
    ui.separator();
    let Some((id, mut gfx)) = selected_graphic(proj, sel) else {
        placeholder(ui, "Select a graphic to edit its appearance.");
        return created;
    };
    let pre = proj.clips.clone();
    let mut changed = false;
    ui.label(RichText::new(match gfx.kind { GfxKind::Text => "TEXT APPEARANCE", GfxKind::Rect => "RECTANGLE", GfxKind::Ellipse => "ELLIPSE" }).color(MUTED).strong());
    // Width and height affect the geometry of a shape. Text is laid out from its content and
    // font size, so showing canvas dimensions for text would be misleading (and do nothing).
    if gfx.kind != GfxKind::Text {
        ui.horizontal(|ui| {
            ui.label("Width");
            changed |= ui.add(egui::DragValue::new(&mut gfx.w).clamp_range(1.0..=1920.0).speed(2.0)).changed();
        });
        ui.horizontal(|ui| {
            ui.label("Height");
            changed |= ui.add(egui::DragValue::new(&mut gfx.h).clamp_range(1.0..=1080.0).speed(2.0)).changed();
        });
    }
    let mut col = Color32::from_rgba_unmultiplied(gfx.color[0], gfx.color[1], gfx.color[2], gfx.color[3]);
    ui.horizontal(|ui| { ui.label("Fill"); changed |= ui.color_edit_button_srgba(&mut col).changed(); });
    gfx.color = col.to_array();
    let mut outcol = Color32::from_rgba_unmultiplied(gfx.outline_color[0], gfx.outline_color[1], gfx.outline_color[2], gfx.outline_color[3]);
    ui.horizontal(|ui| { ui.label("Outline"); changed |= ui.color_edit_button_srgba(&mut outcol).changed(); });
    gfx.outline_color = outcol.to_array();
    ui.horizontal(|ui| {
        ui.label("Outline width");
        changed |= ui.add(egui::Slider::new(&mut gfx.outline, 0.0..=24.0)).changed();
    });
    if gfx.kind == GfxKind::Text {
        let mut bg = Color32::from_rgba_unmultiplied(gfx.bg[0], gfx.bg[1], gfx.bg[2], gfx.bg[3]);
        ui.horizontal(|ui| { ui.label("Text background"); changed |= ui.color_edit_button_srgba(&mut bg).changed(); });
        gfx.bg = bg.to_array();
        ui.horizontal(|ui| { ui.label("Padding"); changed |= ui.add(egui::Slider::new(&mut gfx.pad, 0.0..=80.0)).changed(); });
        changed |= ui.checkbox(&mut gfx.shadow, "Drop shadow").changed();
    }
    if changed {
        if ins.session.is_none() { ins.session = Some(pre); }
        if let Some(c) = proj.clip_mut(id) { c.gfx = Some(gfx); }
    }
    created || changed
}

fn audio_panel(ui: &mut Ui, ins: &mut Inspector, proj: &mut Project, sel: &[u32], timeline_t: f64) -> bool {
    let Some(clip) = sel.iter().find_map(|id| proj.clip(*id).filter(|c| !c.lane.is_video()).cloned()) else {
        placeholder(ui, "Select an audio clip on A1, A2 or A3.");
        return false;
    };
    let pre = proj.clips.clone();
    let local_t = (timeline_t - clip.start).clamp(0.0, clip.dur.max(0.0));
    let (mut gain, mut fade_in, mut fade_out) = (clip.audio_gain_at(local_t), clip.fade_in, clip.fade_out);
    let mut changed = false;
    let mut gain_changed = false;
    let mut audio_preset = clip.audio_preset;
    let mut apply_audio_preset = false;
    ui.label(RichText::new(&clip.name).color(TEXT).strong());
    ui.horizontal(|ui| {
        ui.label("Audio preset");
        ComboBox::from_id_salt(("audio_preset", clip.id))
            .selected_text(audio_preset.label())
            .show_ui(ui, |ui| for preset in AudioPreset::ALL { ui.selectable_value(&mut audio_preset, preset, preset.label()); });
        if ui.small_button("Apply").clicked() { apply_audio_preset = true; changed = true; }
    });
    ui.label(RichText::new("Audio tone presets render on export; live monitor filtering is not available yet.").size(9.0).color(MUTED));
    ui.horizontal(|ui| {
        ui.label("Volume");
        let slider_changed = ui.add(egui::Slider::new(&mut gain, 0.0..=400.0).suffix("%")).changed();
        gain_changed |= slider_changed;
        changed |= slider_changed;
        let keyed = clip.audio_key_near(local_t).is_some();
        if ui.small_button(if keyed { "◆" } else { "◇" }).on_hover_text(if keyed { "Remove gain keyframe at playhead" } else { "Add gain keyframe at playhead" }).clicked() {
            if let Some(c) = proj.clip_mut(clip.id) { c.toggle_audio_key(local_t); }
            changed = true;
        }
    });
    if let Some(k) = clip.audio_key_near(local_t) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Gain key easing").size(10.0).color(MUTED));
            let mut ease = clip.audio_keys[k].ease;
            ComboBox::from_id_salt(("audio_gain_ease", clip.id, k))
                .selected_text(ease.label())
                .show_ui(ui, |ui| for e in Ease::ALL { ui.selectable_value(&mut ease, e, e.label()); });
            if ease != clip.audio_keys[k].ease {
                if let Some(c) = proj.clip_mut(clip.id) { if let Some(key) = c.audio_keys.get_mut(k) { key.ease = ease; } }
                changed = true;
            }
        });
    }
    ui.horizontal(|ui| { ui.label("Fade in"); changed |= ui.add(egui::DragValue::new(&mut fade_in).clamp_range(0.0..=clip.dur).speed(0.05).suffix(" s")).changed(); });
    ui.horizontal(|ui| { ui.label("Fade out"); changed |= ui.add(egui::DragValue::new(&mut fade_out).clamp_range(0.0..=clip.dur).speed(0.05).suffix(" s")).changed(); });
    ui.label(RichText::new("Gain can be keyframed. Fades are linear; final export mixes all visible audio clips.").size(10.0).color(MUTED));
    if changed {
        if ins.session.is_none() { ins.session = Some(pre); }
        if let Some(c) = proj.clip_mut(clip.id) {
            if gain_changed { c.set_audio_gain_at(local_t, gain); }
            if apply_audio_preset { c.audio_preset = audio_preset; }
            c.fade_in = fade_in;
            c.fade_out = fade_out;
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
            if ins.sel_param != i { ins.sel_key = None; }
            ins.sel_param = i;
        }

        let mut v = fx.value(i, ts);
        let before = v;
        let w = (ui.available_width() - 138.0).max(36.0);
        slider(ui, &mut v, def.lo, def.hi, w);

        let drag_speed = match i {
            0 | 1 => 0.5, // position: pixel-ish units per pointer point
            2 | 4 | 6 | 7 => 0.2,
            _ => 0.1,
        };
        let value_resp = ui
            .add_sized(
                [62.0, 18.0],
                egui::DragValue::new(&mut v)
                    .clamp_range(def.lo..=def.hi)
                    .speed(drag_speed)
                    .fixed_decimals(0)
                    .suffix(def.suffix),
            )
            .on_hover_text("Click to type a value; drag horizontally to scrub it");
        if value_resp.clicked() {
            // Make a single click enter numeric-edit mode. A drag still scrubs the value.
            ui.memory_mut(|memory| memory.request_focus(value_resp.id));
        }
        if value_resp.changed() {
            ins.sel_param = i;
        }
        if (v - before).abs() > 1e-4 {
            fx.set_value(i, ts, v);
            ins.sel_param = i;
            *changed = true;
        }

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
            ins.sel_key = fx.key_near(i, ts);
            *changed = true;
        }
    });
}

fn graph(ui: &mut Ui, ins: &mut Inspector, fx: &mut Fx, clip: &Clip, ts: f64, changed: &mut bool) {
    let i = ins.sel_param.min(NPARAM - 1);
    let def = &PARAMS[i];
    ui.label(RichText::new(format!("Value graph  -  {}", def.name)).size(10.0).color(MUTED));
    let h = (ui.available_height() - 58.0).clamp(110.0, 360.0);
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
        ui.label(RichText::new("SEGMENT INTERPOLATION").size(9.0).color(MUTED));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for e in Ease::ALL {
                let short = match e {
                    Ease::Linear => "Linear",
                    Ease::EaseIn => "In",
                    Ease::EaseOut => "Out",
                    Ease::EaseInOut => "In/Out",
                    Ease::Hold => "Hold",
                };
                if ui.selectable_label(fx.keys[i][k].ease == e, RichText::new(short).size(10.0))
                    .on_hover_text(e.label()).clicked() {
                    fx.keys[i][k].ease = e;
                    *changed = true;
                }
            }
        });
    } else if fx.is_animated(i) {
        ui.label(RichText::new("Click a diamond to choose its easing - right-click removes it").size(9.0).color(MUTED));
    }
}
