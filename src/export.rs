//! Offline export pipeline using a bundled FFmpeg command-line build.
//! The UI stays responsive while FFmpeg renders in a worker thread.
use crate::project::{Clip, EffectInstance, EffectKind, GfxKind, Project, TransitionKind};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
}
impl Default for ExportSettings {
    fn default() -> Self { Self { width: 1920, height: 1080, fps: 30, bitrate_mbps: 12 } }
}

#[derive(Clone, Debug)]
pub struct ExportStatus {
    pub progress: f32,
    pub message: String,
    pub done: bool,
    pub success: Option<bool>,
    pub error: Option<String>,
}

pub struct ExportJob {
    pub status: Arc<Mutex<ExportStatus>>,
    cancelled: Arc<AtomicBool>,
}
impl ExportJob {
    pub fn cancel(&self) { self.cancelled.store(true, Ordering::Relaxed); }
    pub fn is_done(&self) -> bool { self.status.lock().map(|s| s.done).unwrap_or(true) }
}

fn ffmpeg_path() -> OsString {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
            if candidate.is_file() { return candidate.into_os_string(); }
        }
    }
    let local = PathBuf::from(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if local.is_file() { return local.into_os_string(); }
    OsString::from(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" })
}

fn num(v: f64) -> String { format!("{:.6}", if v.is_finite() { v } else { 0.0 }) }
fn fnum(v: f32) -> String { num(v as f64) }
fn alpha(c: [u8; 4]) -> String { format!("0x{:02X}{:02X}{:02X}@{:.4}", c[0], c[1], c[2], c[3] as f32 / 255.0) }

/// Escapes user text for a single-quoted FFmpeg filter argument.
fn filter_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            ':' => out.push_str("\\:"),
            ',' => out.push_str("\\,"),
            '%' => out.push_str("\\%"),
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {},
            _ => out.push(ch),
        }
    }
    out
}

fn ease_expr(ease: crate::project::Ease, u: &str) -> String {
    use crate::project::Ease;
    match ease {
        Ease::Linear => format!("({u})"),
        Ease::EaseIn => format!("pow(({u}),2)"),
        Ease::EaseOut => format!("(1-pow(1-({u}),2))"),
        Ease::EaseInOut => format!("(pow(({u}),2)*(3-2*({u})))"),
        Ease::Hold => "0".to_string(),
    }
}

/// FFmpeg expression for one clip-transform parameter; keyframe times are in source time,
/// so subtract the clip's source in-point to express them in the trimmed stream's local clock.
fn clip_param_expr(clip: &Clip, index: usize, source_offset: f64) -> String {
    let Some(keys) = clip.fx.keys.get(index) else { return fnum(clip.fx.base.get(index).copied().unwrap_or(0.0)); };
    if keys.is_empty() { return fnum(clip.fx.base[index]); }
    let local: Vec<(f64, f32, crate::project::Ease)> = keys.iter()
        .map(|k| ((k.t - source_offset).max(0.0), k.v, k.ease)).collect();
    if local.len() == 1 { return fnum(local[0].1); }
    let mut expr = fnum(local[local.len() - 1].1);
    for i in (0..local.len() - 1).rev() {
        let (ta, va, ease) = local[i];
        let (tb, vb, _) = local[i + 1];
        let u = format!("((T-{ta})/{})", num((tb - ta).max(1e-6)));
        let eased = ease_expr(ease, &u);
        let ramp = format!("({}+({}-{})*{})", fnum(va), fnum(vb), fnum(va), eased);
        expr = format!("if(lt(T,{tb}),{ramp},{expr})");
    }
    format!("if(lt(T,{}),{},{expr})", num(local[0].0), fnum(local[0].1))
}

fn effect_param_expr(effect: &EffectInstance, index: usize) -> String {
    let keys = &effect.keys[index];
    if keys.is_empty() { return fnum(effect.params[index]); }
    if keys.len() == 1 { return fnum(keys[0].v); }
    let mut expr = fnum(keys[keys.len() - 1].v);
    for i in (0..keys.len() - 1).rev() {
        let a = keys[i];
        let b = keys[i + 1];
        let u = format!("((T-{})/{})", num(a.t), num((b.t - a.t).max(1e-6)));
        let eased = ease_expr(a.ease, &u);
        let ramp = format!("({}+({}-{})*{})", fnum(a.v), fnum(b.v), fnum(a.v), eased);
        expr = format!("if(lt(T,{}),{ramp},{expr})", num(b.t));
    }
    format!("if(lt(T,{}),{},{expr})", num(keys[0].t), fnum(keys[0].v))
}

fn audio_gain_expr(clip: &Clip) -> String {
    let keys = &clip.audio_keys;
    if keys.is_empty() { return fnum(clip.audio_gain); }
    if keys.len() == 1 { return fnum(keys[0].v); }
    let mut expr = fnum(keys[keys.len() - 1].v);
    for i in (0..keys.len() - 1).rev() {
        let a = keys[i];
        let b = keys[i + 1];
        let u = format!("((T-{})/{})", num(a.t), num((b.t - a.t).max(1e-6)));
        let eased = ease_expr(a.ease, &u);
        let ramp = format!("({}+({}-{})*{})", fnum(a.v), fnum(b.v), fnum(a.v), eased);
        expr = format!("if(lt(T,{}),{ramp},{expr})", num(b.t));
    }
    format!("if(lt(T,{}),{},{expr})", num(keys[0].t), fnum(keys[0].v))
}

fn filters_for_effect(effect: &EffectInstance, clip_duration: f64) -> Vec<String> {
    if !effect.enabled { return vec![]; }
    let p = |i| effect_param_expr(effect, i);
    let kind = effect.kind;
    match kind {
        EffectKind::Wave => {
            let disp = format!("{}*sin(X/W*{}*2*PI+T*{}*2*PI)", p(0), p(1), p(2));
            vec![format!("geq=r='r(X,Y+{disp})':g='g(X,Y+{disp})':b='b(X,Y+{disp})':a='alpha(X,Y+{disp})'")]
        }
        EffectKind::Ripple => {
            let radius = "max(sqrt((X-W/2)*(X-W/2)+(Y-H/2)*(Y-H/2)),1)";
            let radial = format!("sin(({radius}/max(hypot(W,H)/2,1))*{}*2*PI-T*{}*2*PI)*{}*(1-min(({radius}/max(hypot(W,H)/2,1)),1))", p(1), p(2), p(0));
            let dx = format!("((X-W/2)/({radius})*({radial}))");
            let dy = format!("((Y-H/2)/({radius})*({radial}))");
            vec![format!("geq=r='r(X-{dx},Y-{dy})':g='g(X-{dx},Y-{dy})':b='b(X-{dx},Y-{dy})':a='alpha(X-{dx},Y-{dy})'")]
        }
        EffectKind::Twirl => {
            let radius_expr = effect_param_expr(effect, 1);
            let rad = format!("(min(W,H)*({radius_expr})/100/2)");
            let dx = "(X-W/2)"; let dy = "(Y-H/2)";
            let dist = format!("hypot({dx},{dy})");
            let angle = format!("({}*PI/180*pow(max(0,1-({dist})/max({rad},1)),2))", p(0));
            let sx = format!("(W/2+{dx}*cos({angle})+{dy}*sin({angle}))");
            let sy = format!("(H/2-{dx}*sin({angle})+{dy}*cos({angle}))");
            vec![format!("geq=r='r({sx},{sy})':g='g({sx},{sy})':b='b({sx},{sy})':a='alpha({sx},{sy})'")]
        }
        EffectKind::GaussianBlur => vec![format!("gblur=sigma={}", num(effect.param_at(0, 0.0) as f64))],
        EffectKind::LensBlur => vec![format!("boxblur=luma_radius={}:luma_power=1:chroma_radius={}:chroma_power=1", num(effect.param_at(0, 0.0) as f64), num((effect.params[0] * 0.5) as f64))],
        EffectKind::MotionBlur => {
            let frames = (effect.params[0] / 8.0).round().clamp(2.0, 3.0) as u32;
            let weights = if frames == 2 { "1 1" } else { "1 1 1" };
            vec![format!("tmix=frames={frames}:weights='{weights}'")]
        }
        EffectKind::Glow => vec![format!("gblur=sigma={}:steps=2", num((effect.params[1] * effect.params[0].max(0.1)) as f64)), format!("eq=brightness={}:eval=frame", num(((effect.params[0] - 1.0) * 0.08) as f64))],
        EffectKind::Sharpen => vec![format!("unsharp=5:5:{}:5:5:0", num(effect.params[0] as f64))],
        EffectKind::RgbSplit => vec![format!("chromashift=cbh={}:crh={}", num(effect.params[0] as f64), num(-(effect.params[0] as f64)))],
        EffectKind::Flash => {
            let strength = p(0).replace('T', "t");
            let fade_dur = format!("max(({}),0.02)", p(1).replace('T', "t"));
            let start = p(2).replace('T', "t");
            let progress = format!("((t-({start}))/({fade_dur}))");
            vec![format!("eq=brightness='if(between(t,({start}),(({start})+({fade_dur}))),(({strength})/100)*(1-({progress})),0)':eval=frame")]
        }
        EffectKind::FadeIn => {
            let factor = format!("min(max(T/max(({}),0.02),0),1)", p(0));
            vec![format!("geq=r='r(X,Y)*({factor})':g='g(X,Y)*({factor})':b='b(X,Y)*({factor})':a='alpha(X,Y)'")]
        }
        EffectKind::FadeOut => {
            let factor = format!("min(max(({}-T)/max(({}),0.02),0),1)", num(clip_duration), p(0));
            vec![format!("geq=r='r(X,Y)*({factor})':g='g(X,Y)*({factor})':b='b(X,Y)*({factor})':a='alpha(X,Y)'")]
        }
    }
}

fn transform_filters(clip: &Clip, settings: &ExportSettings) -> Vec<String> {
    let t0 = clip.src_off;
    let position_x = clip_param_expr(clip, 0, t0);
    let position_y = clip_param_expr(clip, 1, t0);
    let scale = clip.fx.base[2].clamp(1.0, 400.0) / 100.0;
    let rotation = clip.fx.base[3];
    let brightness = clip_param_expr(clip, 5, t0);
    let contrast = clip_param_expr(clip, 6, t0);
    let saturation = clip_param_expr(clip, 7, t0);
    let mut out = vec![format!("scale={}:{}:force_original_aspect_ratio=decrease:force_divisible_by=2", settings.width, settings.height)];
    if (scale - 1.0).abs() > 0.001 {
        out.push(format!("scale=w='max(2,iw*{})':h='max(2,ih*{})':eval=init", fnum(scale), fnum(scale)));
    }
    if rotation.abs() > 0.01 {
        out.push(format!("rotate={}:ow=rotw(iw):oh=roth(ih):fillcolor=black@0", num(rotation as f64 * std::f64::consts::PI / 180.0)));
    }
    if clip.fx.base[5] != 0.0 || clip.fx.base[6] != 100.0 || clip.fx.base[7] != 100.0 {
        out.push(format!("eq=brightness='({}-0)/100':contrast='({}/100)':saturation='({}/100)':eval=frame", brightness.replace('T', "t"), contrast.replace('T', "t"), saturation.replace('T', "t")));
    }
    if clip.fx.base[4] < 99.99 || !clip.fx.keys[4].is_empty() {
        let opacity = clip_param_expr(clip, 4, t0).replace('T', "t");
        out.push(format!("geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='alpha(X,Y)*min(max(({opacity})/100,0),1)'"));
    }
    // Position is handled by the overlay filter; these are stored in clip_local_position.
    let _ = (position_x, position_y);
    out
}

fn graphic_filter(clip: &Clip, duration: f64, settings: &ExportSettings, label: &str) -> String {
    let g = clip.gfx.as_ref().expect("graphic clip");
    let v = clip.fx.eval(clip.src_off);
    let cx = v[0] / 1920.0 * settings.width as f32;
    let cy = v[1] / 1080.0 * settings.height as f32;
    let scale = (v[2] / 100.0).max(0.0);
    let opacity = (v[4] / 100.0).clamp(0.0, 1.0);
    let w = (g.w * scale * settings.width as f32 / 1920.0).max(1.0);
    let h = (g.h * scale * settings.height as f32 / 1080.0).max(1.0);
    let x = cx - w * 0.5;
    let y = cy - h * 0.5;
    let mut chain = format!("color=c=black@0.0:s={}x{}:r={}:d={},format=rgba", settings.width, settings.height, settings.fps, num(duration));
    match g.kind {
        GfxKind::Rect | GfxKind::Ellipse => {
            chain.push_str(&format!(",drawbox=x={}:y={}:w={}:h={}:color={}:t=fill", num(x as f64), num(y as f64), num(w as f64), num(h as f64), alpha(g.color)));
            if g.outline > 0.0 {
                let ow = (g.outline * scale * settings.width as f32 / 1920.0).max(1.0);
                chain.push_str(&format!(",drawbox=x={}:y={}:w={}:h={}:color={}:t={}", num(x as f64), num(y as f64), num(w as f64), num(h as f64), alpha(g.outline_color), (ow.round().max(1.0) as u32)));
            }
            if g.kind == GfxKind::Ellipse {
                chain.push_str(&format!(",geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lte(pow((X-{})/{}/2,2)+pow((Y-{})/{}/2,2),1),alpha(X,Y),0)'", num(cx as f64), num(w as f64), num(cy as f64), num(h as f64)));
            }
        }
        GfxKind::Text => {
            let font_size = (g.size * scale * settings.height as f32 / 1080.0).max(1.0);
            let font = "C\\:/Windows/Fonts/arial.ttf";
            if g.bg[3] > 0 {
                chain.push_str(&format!(",drawbox=x={}:y={}:w={}:h={}:color={}:t=fill", num(x as f64), num(y as f64), num(w as f64), num(h as f64), alpha(g.bg)));
            }
            let x_expr = match g.align { 0 => format!("{}", num(x as f64)), 2 => format!("{}-text_w", num(cx as f64 + w * 0.5)), _ => format!("{}-text_w/2", num(cx as f64)) };
            chain.push_str(&format!(",drawtext=fontfile='{}':text='{}':fontcolor={}:fontsize={}:x={}:y='{}-text_h/2':borderw={}:bordercolor={}:shadowx={}:shadowy={}", font, filter_text(&g.text), alpha(g.color), num(font_size as f64), x_expr, num(cy as f64), num(g.outline as f64), alpha(g.outline_color), if g.shadow { "2" } else { "0" }, if g.shadow { "2" } else { "0" }));
        }
    }
    if opacity < 0.999 { chain.push_str(&format!(",colorchannelmixer=aa={}", num(opacity as f64))); }
    chain.push_str(&format!(",setpts=PTS-STARTPTS+{}/TB[{}]", num(clip.start), label));
    chain
}

/// Build an FFmpeg filter graph and input argument list. Inputs are one-per-timeline clip, which
/// avoids FFmpeg input-pad reuse problems when a media file is cut into many timeline segments.
pub fn build_args(project: &Project, output: &Path, settings: &ExportSettings, script_path: &Path) -> Result<(Vec<OsString>, String, f64), String> {
    let width = settings.width.clamp(320, 7680) & !1;
    let height = settings.height.clamp(180, 4320) & !1;
    let fps = settings.fps.clamp(12, 120);
    let duration = project.end().max(0.1);
    let video_clips: Vec<Clip> = project.clips.iter().filter(|c| c.lane.is_video() && !project.hidden[c.lane.slot()] && (c.media.is_some() || c.gfx.is_some()) && c.dur > 0.0).cloned().collect();
    if !video_clips.iter().any(|c| c.media.is_some()) { return Err("Add at least one video clip before exporting.".into()); }

    let mut all_clips: Vec<Clip> = project.clips.iter().filter(|c| c.media.is_some() && c.dur > 0.0).cloned().collect();
    all_clips.sort_by(|a, b| a.id.cmp(&b.id));
    let mut inputs: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut args: Vec<OsString> = vec!["-hide_banner".into(), "-y".into(), "-loglevel".into(), "error".into(), "-stats_period".into(), "0.25".into(), "-progress".into(), "pipe:1".into(), "-nostats".into()];
    for clip in &all_clips {
        let m = project.media(clip.media.unwrap()).ok_or_else(|| format!("Media for clip '{}' is missing from the project bin.", clip.name))?;
        if !m.path.is_file() { return Err(format!("Media file for '{}' was not found: {}", clip.name, m.path.display())); }
        if output == m.path.as_path() || output.canonicalize().ok().is_some_and(|o| m.path.canonicalize().ok().is_some_and(|i| i == o)) {
            return Err("Choose an output file that is different from your source media.".into());
        }
        inputs.insert(clip.id, inputs.len());
        args.push("-i".into());
        args.push(m.path.as_os_str().to_owned());
    }

    let settings = ExportSettings { width, height, fps, bitrate_mbps: settings.bitrate_mbps.clamp(2, 80) };
    let mut graph: Vec<String> = Vec::new();
    graph.push(format!("color=c=black:s={}x{}:r={}:d={},format=rgba[base0]", width, height, fps, num(duration)));
    let mut prev = "base0".to_string();
    let mut ordered_video = video_clips.clone();
    ordered_video.sort_by(|a, b| a.lane.slot().cmp(&b.lane.slot()).then_with(|| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal)));
    let mut layer_count = 0usize;
    for clip in &ordered_video {
        let stream = format!("vclip{}", clip.id);
        if clip.gfx.is_some() && clip.media.is_none() {
            graph.push(graphic_filter(clip, clip.dur, &settings, &stream));
        } else if let Some(index) = inputs.get(&clip.id).copied() {
            let local_t = clip.src_off;
            let mut filters = vec![format!("trim=start={}:duration={}", num(clip.src_off), num(clip.dur)), "setpts=PTS-STARTPTS".to_string(), format!("fps={fps}"), "format=rgba".to_string()];
            for effect in &clip.effects {
                filters.extend(filters_for_effect(effect, clip.dur));
            }
            filters.extend(transform_filters(clip, &settings));
            if let Some(tr) = clip.transition_in.filter(|tr| tr.enabled) {
                let d = num((tr.duration as f64).min(clip.dur));
                match tr.kind {
                    TransitionKind::Dissolve => filters.push(format!("fade=t=in:st=0:d={d}:alpha=1")),
                    TransitionKind::DipToBlack => filters.push(format!("fade=t=in:st=0:d={d}:color=black")),
                    TransitionKind::WipeLeft => filters.push(format!("geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lte(X,W*min(T/{d},1)),alpha(X,Y),0)'")),
                    TransitionKind::Flash => filters.push(format!("fade=t=in:st=0:d={d}:color=white")),
                }
            }
            let extend = ordered_video.iter().filter(|n| n.lane == clip.lane && n.start >= clip.end() - 0.03 && n.start <= clip.end() + 0.03)
                .filter_map(|n| n.transition_in.filter(|tr| tr.enabled).map(|tr| tr.duration as f64)).fold(0.0, f64::max);
            if extend > 0.0 { filters.push(format!("tpad=stop_mode=clone:stop_duration={}", num(extend))); }
            filters.push(format!("setpts=PTS+{}/TB", num(clip.start)));
            graph.push(format!("[{}:v:0]{}[{}]", index, filters.join(","), stream));
            let _ = local_t;
        } else { continue; }
        let next = format!("vcomp{}", layer_count);
        let extend = ordered_video.iter().filter(|n| n.lane == clip.lane && n.start >= clip.end() - 0.03 && n.start <= clip.end() + 0.03)
            .filter_map(|n| n.transition_in.filter(|tr| tr.enabled).map(|tr| tr.duration as f64)).fold(0.0, f64::max);
        let end = clip.end() + extend;
        let (x, y) = if clip.gfx.is_some() {
            ("0".to_string(), "0".to_string())
        } else {
            let px = clip_param_expr(clip, 0, clip.src_off);
            let py = clip_param_expr(clip, 1, clip.src_off);
            (format!("(main_w-overlay_w)/2+(({px})-960)/960*main_w/2", px=px.replace('T', "t")), format!("(main_h-overlay_h)/2+(({py})-540)/540*main_h/2", py=py.replace('T', "t")))
        };
        graph.push(format!("[{}][{}]overlay=x='{}':y='{}':eval=frame:eof_action=pass:shortest=0:enable='between(t,{}, {})'[{}]", prev, stream, x, y, num(clip.start), num(end), next));
        prev = next;
        layer_count += 1;
    }
    if layer_count == 0 { return Err("No renderable video clips were found.".into()); }
    graph.push(format!("[{}]format=yuv420p[vout]", prev));

    let audio_clips: Vec<Clip> = project.clips.iter().filter(|c| !c.lane.is_video() && !project.hidden[c.lane.slot()] && c.media.is_some() && c.dur > 0.0).cloned().collect();
    let mut audio_labels = Vec::new();
    for clip in &audio_clips {
        let Some(index) = inputs.get(&clip.id).copied() else { continue };
        let m = project.media(clip.media.unwrap());
        if !m.map_or(false, |m| m.has_audio) { continue; }
        let label = format!("aud{}", clip.id);
        let gain_expr = audio_gain_expr(clip).replace('T', "t");
        let mut f = vec![format!("atrim=start={}:duration={}", num(clip.src_off), num(clip.dur)), "asetpts=PTS-STARTPTS".to_string(), format!("volume=volume='(({gain_expr})/100)':eval=frame")];
        if clip.fade_in > 0.0 { f.push(format!("afade=t=in:st=0:d={}", num(clip.fade_in.min(clip.dur)))); }
        if clip.fade_out > 0.0 { f.push(format!("afade=t=out:st={}:d={}", num((clip.dur - clip.fade_out).max(0.0)), num(clip.fade_out.min(clip.dur)))); }
        f.push(format!("adelay=delays={}:all=1", (clip.start.max(0.0) * 1000.0).round() as u64));
        graph.push(format!("[{}:a:0]{}[{}]", index, f.join(","), label));
        audio_labels.push(format!("[{}]", label));
    }
    if audio_labels.is_empty() {
        graph.push(format!("anullsrc=r=48000:cl=stereo:d={}[aout]", num(duration)));
    } else {
        graph.push(format!("{}amix=inputs={}:duration=longest:normalize=0,aresample=48000,alimiter=limit=0.95[aout]", audio_labels.join(""), audio_labels.len()));
    }

    args.push("-filter_complex_script".into());
    args.push(script_path.as_os_str().to_owned());
    args.push("-map".into()); args.push("[vout]".into());
    args.push("-map".into()); args.push("[aout]".into());
    args.extend(["-t".into(), num(duration).into(), "-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-b:v".into(), format!("{}M", settings.bitrate_mbps).into(), "-pix_fmt".into(), "yuv420p".into(), "-r".into(), fps.to_string().into(), "-c:a".into(), "aac".into(), "-b:a".into(), "320k".into(), "-movflags".into(), "+faststart".into(), output.as_os_str().to_owned()]);
    Ok((args, graph.join(";\n"), duration))
}

fn temp_output_path(output: &Path, tag: &str) -> PathBuf {
    let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("yoru-export");
    output.with_file_name(format!("{stem}.{tag}.mp4"))
}

fn finish_output(temp: &Path, final_path: &Path) -> Result<(), String> {
    let backup = final_path.with_extension("yoru-previous-output.bak");
    let had_previous = final_path.exists();
    if had_previous {
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(final_path, &backup).map_err(|e| format!("Could not stage previous output: {e}"))?;
    }
    match std::fs::rename(temp, final_path) {
        Ok(()) => { if had_previous { let _ = std::fs::remove_file(backup); } Ok(()) }
        Err(e) => {
            if had_previous { let _ = std::fs::rename(&backup, final_path); }
            Err(format!("Could not finalize output file: {e}"))
        }
    }
}

fn spawn_job(
    args: Vec<OsString>,
    working_output: PathBuf,
    final_output: PathBuf,
    duration: f64,
    script_cleanup: Option<PathBuf>,
    running_message: &'static str,
) -> Result<ExportJob, String> {
    let status = Arc::new(Mutex::new(ExportStatus { progress: 0.0, message: running_message.into(), done: false, success: None, error: None }));
    let cancelled = Arc::new(AtomicBool::new(false));
    let st = status.clone();
    let cancel = cancelled.clone();
    let ffmpeg = ffmpeg_path();
    let spawn_fail_output = working_output.clone();
    let spawn_fail_script = script_cleanup.clone();
    std::thread::Builder::new().name("yoru-ffmpeg-job".into()).spawn(move || {
        let mut child = match Command::new(&ffmpeg).args(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Ok(mut s) = st.lock() {
                    s.done = true; s.success = Some(false);
                    s.error = Some(format!("Could not start FFmpeg ({}): {e}. Ensure ffmpeg.exe is beside Yoru.", Path::new(&ffmpeg).display()));
                    s.message = "FFmpeg job failed".into();
                }
                let _ = std::fs::remove_file(&working_output);
                if let Some(path) = script_cleanup { let _ = std::fs::remove_file(path); }
                return;
            }
        };
        let error_text = Arc::new(Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let err = error_text.clone();
            let _ = std::thread::Builder::new().name("yoru-ffmpeg-log".into()).spawn(move || {
                let mut text = String::new(); let _ = stderr.read_to_string(&mut text);
                if let Ok(mut slot) = err.lock() { *slot = text.chars().rev().take(6000).collect::<String>().chars().rev().collect(); }
            });
        }
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                if cancel.load(Ordering::Relaxed) { let _ = child.kill(); break; }
                let Ok(line) = line else { break };
                if let Some(v) = line.strip_prefix("out_time_ms=").and_then(|v| v.parse::<f64>().ok()) {
                    let pct = (v / (duration.max(0.1) * 1_000_000.0)).clamp(0.0, 1.0) as f32;
                    if let Ok(mut s) = st.lock() { s.progress = pct; s.message = format!("{} {:.0}%", running_message.trim_end_matches('…'), pct * 100.0); }
                } else if line == "progress=end" {
                    if let Ok(mut s) = st.lock() { s.progress = 1.0; s.message = "Finalizing output…".into(); }
                }
            }
        }
        let result = child.wait();
        let process_ok = result.map(|r| r.success()).unwrap_or(false) && !cancel.load(Ordering::Relaxed);
        let log = error_text.lock().map(|s| s.clone()).unwrap_or_default();
        let (success, error) = if process_ok {
            match finish_output(&working_output, &final_output) { Ok(()) => (true, None), Err(e) => (false, Some(e)) }
        } else if cancel.load(Ordering::Relaxed) {
            (false, None)
        } else {
            (false, Some(if log.trim().is_empty() { "FFmpeg exited unsuccessfully. Check the source media and selected settings.".into() } else { log }))
        };
        if !success { let _ = std::fs::remove_file(&working_output); }
        if let Ok(mut s) = st.lock() {
            s.done = true; s.success = Some(success);
            if success { s.progress = 1.0; s.message = "Complete".into(); }
            else if cancel.load(Ordering::Relaxed) { s.message = "Cancelled".into(); }
            else { s.message = "Failed".into(); s.error = error; }
        }
        if let Some(path) = script_cleanup { let _ = std::fs::remove_file(path); }
    }).map_err(|e| {
        let _ = std::fs::remove_file(&spawn_fail_output);
        if let Some(path) = spawn_fail_script { let _ = std::fs::remove_file(path); }
        format!("Could not start FFmpeg worker: {e}")
    })?;
    Ok(ExportJob { status, cancelled })
}

pub fn start(project: &Project, output: &Path, settings: &ExportSettings) -> Result<ExportJob, String> {
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(parent).map_err(|e| format!("Could not create output folder: {e}"))?; }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let script = std::env::temp_dir().join(format!("yoru-export-{}-{stamp}.ffscript", std::process::id()));
    let (mut args, graph, duration) = build_args(project, output, settings, &script)?;
    std::fs::write(&script, graph).map_err(|e| format!("Could not prepare export filter graph: {e}"))?;
    let working_output = temp_output_path(output, &format!("yoru-{}", std::process::id()));
    if let Some(last) = args.last_mut() { *last = working_output.as_os_str().to_owned(); }
    spawn_job(args, working_output, output.to_path_buf(), duration, Some(script), "Rendering…")
}

/// Create a low-resolution H.264/AAC proxy in the background. The preview engine may use it,
/// while export deliberately continues to reference each source's original path.
pub fn start_proxy(source: &Path, output: &Path, duration: f64) -> Result<ExportJob, String> {
    if !source.is_file() { return Err(format!("Source media does not exist: {}", source.display())); }
    if source.canonicalize().ok().is_some_and(|s| output.canonicalize().ok().is_some_and(|o| s == o)) {
        return Err("Proxy output cannot overwrite the source media.".into());
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(parent).map_err(|e| format!("Could not create proxy folder: {e}"))?; }
    let working_output = temp_output_path(output, &format!("proxy-{}", std::process::id()));
    let args: Vec<OsString> = vec![
        "-hide_banner".into(), "-y".into(), "-loglevel".into(), "error".into(),
        "-stats_period".into(), "0.25".into(), "-progress".into(), "pipe:1".into(), "-nostats".into(),
        "-i".into(), source.as_os_str().to_owned(), "-map".into(), "0:v:0".into(), "-map".into(), "0:a?".into(),
        "-vf".into(), "scale=w='min(960,iw)':h=-2:force_divisible_by=2".into(),
        "-c:v".into(), "libx264".into(), "-preset".into(), "ultrafast".into(), "-crf".into(), "28".into(),
        "-c:a".into(), "aac".into(), "-b:a".into(), "128k".into(), "-movflags".into(), "+faststart".into(),
        "-t".into(), num(duration.max(0.1)).into(), working_output.as_os_str().to_owned(),
    ];
    spawn_job(args, working_output, output.to_path_buf(), duration.max(0.1), None, "Generating proxy…")
}
