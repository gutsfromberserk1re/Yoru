//! Project model: media pool, multi-track clips, keyframed effects, edit operations, undo/redo.
//! Pure data + logic (no egui, no ffmpeg) so it stays easy to reason about and to test.
//!
//! Undo model: every edit snapshots the clip list *before* the change. Clip lists are tiny
//! (hundreds of entries), so a snapshot is cheaper and far less bug-prone than per-edit inverses,
//! and a whole mouse drag (or slider drag) collapses into one undo step.

use std::path::PathBuf;

const EPS: f64 = 1e-6;
pub const MIN_DUR: f64 = 0.1;
const UNDO_LIMIT: usize = 200;

pub const NV: usize = 3; // video tracks V1..V3
pub const NA: usize = 3; // audio tracks A1..A3
pub const NTRACKS: usize = NV + NA;

// ---------------------------------------------------------------- tracks
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    V(u8),
    A(u8),
}

impl Lane {
    pub const V1: Lane = Lane::V(0);
    pub const A1: Lane = Lane::A(0);

    pub fn is_video(self) -> bool {
        matches!(self, Lane::V(_))
    }
    /// 0..NTRACKS: V1..V3, then A1..A3.
    pub fn slot(self) -> usize {
        match self {
            Lane::V(n) => (n as usize).min(NV - 1),
            Lane::A(n) => NV + (n as usize).min(NA - 1),
        }
    }
    pub fn from_slot(s: usize) -> Lane {
        if s < NV {
            Lane::V(s as u8)
        } else {
            Lane::A((s - NV).min(NA - 1) as u8)
        }
    }
    /// V(n) <-> A(n)
    pub fn partner(self) -> Lane {
        match self {
            Lane::V(n) => Lane::A(n),
            Lane::A(n) => Lane::V(n),
        }
    }
    pub fn name(self) -> String {
        match self {
            Lane::V(n) => format!("V{}", n + 1),
            Lane::A(n) => format!("A{}", n + 1),
        }
    }
    fn token(self) -> String {
        self.name()
    }
    fn parse(s: &str) -> Option<Lane> {
        match s {
            // v1/v2 files: 0 = connected (V2), 1 = main (V1), 2 = audio (A1)
            "0" => Some(Lane::V(1)),
            "1" => Some(Lane::V(0)),
            "2" => Some(Lane::A(0)),
            _ => {
                let n: u8 = s.get(1..)?.parse().ok()?;
                if n == 0 || n as usize > NV.max(NA) {
                    return None;
                }
                match s.as_bytes().first()? {
                    b'V' => Some(Lane::V((n - 1).min(NV as u8 - 1))),
                    b'A' => Some(Lane::A((n - 1).min(NA as u8 - 1))),
                    _ => None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------- effects with keyframes
pub const NPARAM: usize = 8;

pub struct ParamDef {
    pub name: &'static str,
    pub group: &'static str,
    pub lo: f32,
    pub hi: f32,
    pub def: f32,
    pub suffix: &'static str,
}

/// Index order is the storage order: 0 PosX, 1 PosY, 2 Scale, 3 Rotation, 4 Opacity,
/// 5 Brightness, 6 Contrast, 7 Saturation.
pub const PARAMS: [ParamDef; NPARAM] = [
    ParamDef { name: "Position X", group: "Transform", lo: 0.0, hi: 1920.0, def: 960.0, suffix: "" },
    ParamDef { name: "Position Y", group: "Transform", lo: 0.0, hi: 1080.0, def: 540.0, suffix: "" },
    ParamDef { name: "Scale", group: "Transform", lo: 0.0, hi: 400.0, def: 100.0, suffix: "%" },
    ParamDef { name: "Rotation", group: "Transform", lo: -180.0, hi: 180.0, def: 0.0, suffix: "\u{00B0}" },
    ParamDef { name: "Opacity", group: "Compositing", lo: 0.0, hi: 100.0, def: 100.0, suffix: "%" },
    ParamDef { name: "Brightness", group: "Color", lo: -100.0, hi: 100.0, def: 0.0, suffix: "" },
    ParamDef { name: "Contrast", group: "Color", lo: 0.0, hi: 200.0, def: 100.0, suffix: "%" },
    ParamDef { name: "Saturation", group: "Color", lo: 0.0, hi: 200.0, def: 100.0, suffix: "%" },
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ease {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    Hold,
}

impl Ease {
    pub const ALL: [Ease; 5] = [Ease::Linear, Ease::EaseIn, Ease::EaseOut, Ease::EaseInOut, Ease::Hold];
    pub fn label(self) -> &'static str {
        match self {
            Ease::Linear => "Linear",
            Ease::EaseIn => "Ease in",
            Ease::EaseOut => "Ease out",
            Ease::EaseInOut => "Ease in/out",
            Ease::Hold => "Hold",
        }
    }
    fn id(self) -> u8 {
        Ease::ALL.iter().position(|e| *e == self).unwrap_or(0) as u8
    }
    fn from_id(i: u8) -> Ease {
        Ease::ALL.get(i as usize).copied().unwrap_or(Ease::Linear)
    }
    /// Shape of the segment that starts at a key with this easing (u in 0..1).
    pub fn apply(self, u: f32) -> f32 {
        match self {
            Ease::Linear => u,
            Ease::EaseIn => u * u,
            Ease::EaseOut => 1.0 - (1.0 - u) * (1.0 - u),
            Ease::EaseInOut => u * u * (3.0 - 2.0 * u),
            Ease::Hold => 0.0,
        }
    }
}

/// A keyframe. `t` is in *source* seconds of the clip (so trimming/splitting never disturbs it);
/// `ease` shapes the segment from this key to the next.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Key {
    pub t: f64,
    pub v: f32,
    pub ease: Ease,
}

const KEY_EPS: f64 = 1.0 / 60.0;

#[derive(Clone, PartialEq, Debug)]
pub struct Fx {
    pub base: [f32; NPARAM],
    pub keys: [Vec<Key>; NPARAM],
}

fn defaults() -> [f32; NPARAM] {
    std::array::from_fn(|i| PARAMS[i].def)
}

impl Default for Fx {
    fn default() -> Self {
        Fx { base: defaults(), keys: Default::default() }
    }
}

fn eval_keys(base: f32, keys: &[Key], t: f64) -> f32 {
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return base };
    if t <= first.t {
        return first.v;
    }
    if t >= last.t {
        return last.v;
    }
    let k = keys.partition_point(|k| k.t <= t).saturating_sub(1);
    let (a, b) = (&keys[k], &keys[(k + 1).min(keys.len() - 1)]);
    let u = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
    a.v + (b.v - a.v) * a.ease.apply(u.clamp(0.0, 1.0))
}

impl Fx {
    pub fn is_identity(&self) -> bool {
        self.keys.iter().all(|k| k.is_empty()) && self.base == defaults()
    }
    pub fn is_animated(&self, i: usize) -> bool {
        !self.keys[i].is_empty()
    }
    pub fn value(&self, i: usize, t: f64) -> f32 {
        eval_keys(self.base[i], &self.keys[i], t)
    }
    pub fn eval(&self, t: f64) -> [f32; NPARAM] {
        std::array::from_fn(|i| self.value(i, t))
    }
    pub fn key_near(&self, i: usize, t: f64) -> Option<usize> {
        self.keys[i].iter().position(|k| (k.t - t).abs() < KEY_EPS)
    }
    pub fn prev_key(&self, i: usize, t: f64) -> Option<f64> {
        self.keys[i].iter().rev().find(|k| k.t < t - KEY_EPS).map(|k| k.t)
    }
    pub fn next_key(&self, i: usize, t: f64) -> Option<f64> {
        self.keys[i].iter().find(|k| k.t > t + KEY_EPS).map(|k| k.t)
    }

    fn insert_key(&mut self, i: usize, key: Key) -> usize {
        let pos = self.keys[i].partition_point(|k| k.t < key.t);
        self.keys[i].insert(pos, key);
        pos
    }

    /// Slider edit: writes the base value, or (if animated) the key at `t`, creating it if needed.
    pub fn set_value(&mut self, i: usize, t: f64, v: f32) {
        let v = v.clamp(PARAMS[i].lo, PARAMS[i].hi);
        if self.keys[i].is_empty() {
            self.base[i] = v;
            return;
        }
        match self.key_near(i, t) {
            Some(k) => self.keys[i][k].v = v,
            None => {
                self.insert_key(i, Key { t, v, ease: Ease::Linear });
            }
        }
    }

    /// Stopwatch: start animating (first key at `t`) or stop (keys removed, value frozen at `t`).
    pub fn toggle_anim(&mut self, i: usize, t: f64) {
        if self.keys[i].is_empty() {
            let v = self.base[i];
            self.keys[i].push(Key { t, v, ease: Ease::Linear });
        } else {
            self.base[i] = self.value(i, t);
            self.keys[i].clear();
        }
    }

    /// Diamond button: add a key at `t`, or remove the one there.
    pub fn toggle_key(&mut self, i: usize, t: f64) {
        if self.keys[i].is_empty() {
            self.toggle_anim(i, t);
            return;
        }
        match self.key_near(i, t) {
            Some(k) => {
                if self.keys[i].len() == 1 {
                    self.base[i] = self.keys[i][0].v;
                    self.keys[i].clear();
                } else {
                    self.keys[i].remove(k);
                }
            }
            None => {
                let v = self.value(i, t);
                self.insert_key(i, Key { t, v, ease: Ease::Linear });
            }
        }
    }

    /// Graph drag: moves key `k`; returns its new index (order may change).
    pub fn move_key(&mut self, i: usize, k: usize, t: f64, v: f32) -> usize {
        let ease = self.keys[i][k].ease;
        self.keys[i].remove(k);
        self.insert_key(i, Key { t: t.max(0.0), v: v.clamp(PARAMS[i].lo, PARAMS[i].hi), ease })
    }
}

// ---------------------------------------------------------------- non-destructive clip effects
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffectKind {
    Wave, Ripple, Twirl, GaussianBlur, LensBlur, MotionBlur, Glow, Sharpen, RgbSplit,
    Flash, FadeIn, FadeOut,
    // New kinds are appended so IDs in existing YORU projects remain stable.
    Vignette, Pixelate, Sepia, Grayscale, Invert, Posterize, FilmGrain, Scanlines,
    // YORU 8+ additions are append-only so persisted effect IDs remain stable.
    Mirror, EdgeDetect, ColorTemperature, VHS,
}

impl EffectKind {
    pub const ALL: [EffectKind; 24] = [
        EffectKind::Wave, EffectKind::Ripple, EffectKind::Twirl,
        EffectKind::GaussianBlur, EffectKind::LensBlur, EffectKind::MotionBlur,
        EffectKind::Glow, EffectKind::Sharpen, EffectKind::RgbSplit,
        EffectKind::Flash, EffectKind::FadeIn, EffectKind::FadeOut,
        EffectKind::Vignette, EffectKind::Pixelate, EffectKind::Sepia,
        EffectKind::Grayscale, EffectKind::Invert, EffectKind::Posterize,
        EffectKind::FilmGrain, EffectKind::Scanlines,
        EffectKind::Mirror, EffectKind::EdgeDetect, EffectKind::ColorTemperature, EffectKind::VHS,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Wave => "Wave Warp", Self::Ripple => "Ripple", Self::Twirl => "Twirl",
            Self::GaussianBlur => "Gaussian Blur", Self::LensBlur => "Lens Blur",
            Self::MotionBlur => "Directional Motion Blur", Self::Glow => "Glow",
            Self::Sharpen => "Sharpen", Self::RgbSplit => "RGB Split",
            Self::Flash => "Flash", Self::FadeIn => "Fade In", Self::FadeOut => "Fade Out",
            Self::Vignette => "Vignette", Self::Pixelate => "Pixelate",
            Self::Sepia => "Sepia Tone", Self::Grayscale => "Black & White",
            Self::Invert => "Invert Colors", Self::Posterize => "Posterize",
            Self::FilmGrain => "Film Grain", Self::Scanlines => "Scanlines",
            Self::Mirror => "Mirror", Self::EdgeDetect => "Edge Detect",
            Self::ColorTemperature => "Color Temperature", Self::VHS => "VHS / Tape",
        }
    }
    pub fn category(self) -> &'static str {
        match self {
            Self::Wave | Self::Ripple | Self::Twirl | Self::RgbSplit => "Distortion",
            Self::GaussianBlur | Self::LensBlur | Self::MotionBlur | Self::Sharpen => "Blur & Sharpen",
            Self::Glow | Self::Vignette | Self::FilmGrain | Self::Scanlines => "Light & Texture",
            Self::Flash | Self::FadeIn | Self::FadeOut => "Transitions",
            Self::Pixelate | Self::Mirror => "Stylize",
            Self::Sepia | Self::Grayscale | Self::Invert | Self::Posterize | Self::ColorTemperature => "Color Looks",
            Self::EdgeDetect | Self::VHS => "Retro & Detail",
        }
    }
    pub fn id(self) -> u8 { Self::ALL.iter().position(|e| *e == self).unwrap_or(0) as u8 }
    pub fn from_id(id: u8) -> Option<Self> { Self::ALL.get(id as usize).copied() }
    pub fn from_label(label: &str) -> Option<Self> { Self::ALL.into_iter().find(|e| e.label() == label) }
    pub fn params(self) -> &'static [EffectParamDef] {
        match self {
            Self::Wave => &WAVE_PARAMS,
            Self::Ripple => &RIPPLE_PARAMS,
            Self::Twirl => &TWIRL_PARAMS,
            Self::GaussianBlur => &BLUR_PARAMS,
            Self::LensBlur => &LENS_PARAMS,
            Self::MotionBlur => &MOTION_PARAMS,
            Self::Glow => &GLOW_PARAMS,
            Self::Sharpen => &SHARPEN_PARAMS,
            Self::RgbSplit => &RGB_PARAMS,
            Self::Flash => &FLASH_PARAMS,
            Self::FadeIn | Self::FadeOut => &FADE_PARAMS,
            Self::Vignette => &VIGNETTE_PARAMS,
            Self::Pixelate => &PIXELATE_PARAMS,
            Self::Sepia | Self::Grayscale | Self::Invert => &AMOUNT_PARAMS,
            Self::Posterize => &POSTERIZE_PARAMS,
            Self::FilmGrain => &GRAIN_PARAMS,
            Self::Scanlines => &SCANLINE_PARAMS,
            Self::Mirror => &MIRROR_PARAMS,
            Self::EdgeDetect => &EDGE_PARAMS,
            Self::ColorTemperature => &TEMPERATURE_PARAMS,
            Self::VHS => &VHS_PARAMS,
        }
    }
    pub fn defaults(self) -> [f32; 4] {
        let mut p = [0.0; 4];
        for (i, def) in self.params().iter().enumerate() { p[i] = def.default; }
        p
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EffectParamDef {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub suffix: &'static str,
}
static WAVE_PARAMS: [EffectParamDef; 3] = [
    EffectParamDef { name: "Height", min: 0.0, max: 120.0, default: 18.0, suffix: " px" },
    EffectParamDef { name: "Frequency", min: 0.5, max: 30.0, default: 8.0, suffix: "" },
    EffectParamDef { name: "Speed", min: -10.0, max: 10.0, default: 1.0, suffix: " Hz" },
];
static RIPPLE_PARAMS: [EffectParamDef; 3] = [
    EffectParamDef { name: "Strength", min: 0.0, max: 100.0, default: 24.0, suffix: " px" },
    EffectParamDef { name: "Rings", min: 0.5, max: 20.0, default: 5.0, suffix: "" },
    EffectParamDef { name: "Speed", min: -10.0, max: 10.0, default: 1.0, suffix: " Hz" },
];
static TWIRL_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Angle", min: -720.0, max: 720.0, default: 180.0, suffix: "°" },
    EffectParamDef { name: "Radius", min: 5.0, max: 100.0, default: 55.0, suffix: "%" },
];
static BLUR_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Radius", min: 0.0, max: 48.0, default: 8.0, suffix: " px" }];
static LENS_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Radius", min: 0.0, max: 40.0, default: 12.0, suffix: " px" },
    EffectParamDef { name: "Iris", min: 3.0, max: 12.0, default: 7.0, suffix: " blades" },
];
static MOTION_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Length", min: 0.0, max: 80.0, default: 18.0, suffix: " px" },
    EffectParamDef { name: "Angle", min: -180.0, max: 180.0, default: 0.0, suffix: "°" },
];
static GLOW_PARAMS: [EffectParamDef; 3] = [
    EffectParamDef { name: "Intensity", min: 0.0, max: 3.0, default: 1.0, suffix: "×" },
    EffectParamDef { name: "Radius", min: 0.0, max: 48.0, default: 12.0, suffix: " px" },
    EffectParamDef { name: "Threshold", min: 0.0, max: 255.0, default: 180.0, suffix: "" },
];
static SHARPEN_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Amount", min: 0.0, max: 3.0, default: 1.0, suffix: "×" },
    EffectParamDef { name: "Radius", min: 1.0, max: 12.0, default: 1.0, suffix: " px" },
];
static RGB_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Offset", min: 0.0, max: 40.0, default: 8.0, suffix: " px" },
    EffectParamDef { name: "Angle", min: -180.0, max: 180.0, default: 0.0, suffix: "°" },
];
static FLASH_PARAMS: [EffectParamDef; 3] = [
    EffectParamDef { name: "Intensity", min: 0.0, max: 100.0, default: 100.0, suffix: "%" },
    EffectParamDef { name: "Duration", min: 0.02, max: 2.0, default: 0.25, suffix: " s" },
    EffectParamDef { name: "Start", min: 0.0, max: 3600.0, default: 0.0, suffix: " s" },
];
static FADE_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Duration", min: 0.02, max: 10.0, default: 1.0, suffix: " s" }];
static VIGNETTE_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Strength", min: 0.0, max: 1.0, default: 0.55, suffix: "" },
    EffectParamDef { name: "Radius", min: 0.1, max: 1.0, default: 0.68, suffix: "" },
];
static PIXELATE_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Block size", min: 2.0, max: 64.0, default: 12.0, suffix: " px" }];
static AMOUNT_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Amount", min: 0.0, max: 100.0, default: 100.0, suffix: "%" }];
static POSTERIZE_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Levels", min: 2.0, max: 32.0, default: 6.0, suffix: "" }];
static GRAIN_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Amount", min: 0.0, max: 50.0, default: 10.0, suffix: "" },
    EffectParamDef { name: "Grain size", min: 1.0, max: 4.0, default: 1.0, suffix: "×" },
];
static SCANLINE_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Intensity", min: 0.0, max: 100.0, default: 30.0, suffix: "%" },
    EffectParamDef { name: "Spacing", min: 2.0, max: 12.0, default: 3.0, suffix: " px" },
];
static MIRROR_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Axis", min: 0.0, max: 1.0, default: 0.0, suffix: " (0=H, 1=V)" }];
static EDGE_PARAMS: [EffectParamDef; 1] = [EffectParamDef { name: "Amount", min: 0.0, max: 100.0, default: 75.0, suffix: "%" }];
static TEMPERATURE_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Temperature", min: -100.0, max: 100.0, default: 15.0, suffix: "" },
    EffectParamDef { name: "Tint", min: -100.0, max: 100.0, default: 0.0, suffix: "" },
];
static VHS_PARAMS: [EffectParamDef; 2] = [
    EffectParamDef { name: "Amount", min: 0.0, max: 100.0, default: 45.0, suffix: "%" },
    EffectParamDef { name: "Tracking", min: 0.0, max: 100.0, default: 25.0, suffix: "%" },
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EffectKey {
    pub t: f64,
    pub v: f32,
    pub ease: Ease,
}

#[derive(Clone, PartialEq, Debug)]
pub struct EffectInstance {
    pub kind: EffectKind,
    pub enabled: bool,
    pub params: [f32; 4],
    /// Per-parameter animation in clip-local seconds. Empty tracks use the static parameter.
    pub keys: [Vec<EffectKey>; 4],
}
impl EffectInstance {
    pub fn new(kind: EffectKind, local_time: f64) -> Self {
        let mut params = kind.defaults();
        if kind == EffectKind::Flash { params[2] = local_time.max(0.0) as f32; }
        Self { kind, enabled: true, params, keys: Default::default() }
    }

    pub fn param_at(&self, index: usize, t: f64) -> f32 {
        if index >= 4 { return 0.0; }
        let keys = &self.keys[index];
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return self.params[index] };
        if t <= first.t { return first.v; }
        if t >= last.t { return last.v; }
        let k = keys.partition_point(|key| key.t <= t).saturating_sub(1);
        let (a, b) = (&keys[k], &keys[(k + 1).min(keys.len() - 1)]);
        let u = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
        a.v + (b.v - a.v) * a.ease.apply(u.clamp(0.0, 1.0))
    }

    pub fn evaluated_params(&self, t: f64) -> [f32; 4] {
        std::array::from_fn(|i| self.param_at(i, t))
    }

    pub fn key_near(&self, index: usize, t: f64) -> Option<usize> {
        self.keys.get(index)?.iter().position(|k| (k.t - t).abs() <= 1.0 / 60.0)
    }

    pub fn toggle_key(&mut self, index: usize, t: f64) {
        if index >= 4 { return; }
        if let Some(k) = self.key_near(index, t) {
            self.keys[index].remove(k);
        } else {
            let key = EffectKey { t: t.max(0.0), v: self.param_at(index, t), ease: Ease::Linear };
            let at = self.keys[index].partition_point(|k| k.t < key.t);
            self.keys[index].insert(at, key);
        }
    }

    pub fn set_param_at(&mut self, index: usize, t: f64, value: f32) {
        if index >= 4 { return; }
        let def = self.kind.params().get(index);
        let v = def.map_or(value, |d| value.clamp(d.min, d.max));
        if self.keys[index].is_empty() {
            self.params[index] = v;
        } else if let Some(k) = self.key_near(index, t) {
            self.keys[index][k].v = v;
        } else {
            let at = self.keys[index].partition_point(|k| k.t < t);
            self.keys[index].insert(at, EffectKey { t: t.max(0.0), v, ease: Ease::Linear });
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransitionKind { Dissolve, DipToBlack, WipeLeft, Flash, WipeRight, WipeUp, WipeDown, CircleIris, DiagonalWipe, DiamondIris }
impl TransitionKind {
    pub const ALL: [TransitionKind; 10] = [Self::Dissolve, Self::DipToBlack, Self::WipeLeft, Self::Flash, Self::WipeRight, Self::WipeUp, Self::WipeDown, Self::CircleIris, Self::DiagonalWipe, Self::DiamondIris];
    pub fn label(self) -> &'static str {
        match self {
            Self::Dissolve => "Cross Dissolve", Self::DipToBlack => "Dip to Black", Self::WipeLeft => "Wipe Left",
            Self::Flash => "Flash Transition", Self::WipeRight => "Wipe Right", Self::WipeUp => "Wipe Up",
            Self::WipeDown => "Wipe Down", Self::CircleIris => "Circle Iris",
            Self::DiagonalWipe => "Diagonal Wipe", Self::DiamondIris => "Diamond Iris",
        }
    }
    pub fn id(self) -> u8 { Self::ALL.iter().position(|v| *v == self).unwrap_or(0) as u8 }
    pub fn from_id(id: u8) -> Option<Self> { Self::ALL.get(id as usize).copied() }
    pub fn from_label(s: &str) -> Option<Self> { Self::ALL.into_iter().find(|v| v.label() == s) }
}
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration: f32,
    pub enabled: bool,
}
impl Transition {
    pub fn new(kind: TransitionKind) -> Self { Self { kind, duration: 0.8, enabled: true } }
}

// Non-destructive audio processing presets. They are applied by the export mixer; raw source media is unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioPreset { Neutral, Voice, MusicBed, Cinematic, Telephone, LoFi, Underwater, Podcast, BassBoost, BrightVoice, Reverb, Muffled }
impl AudioPreset {
    pub const ALL: [AudioPreset; 12] = [Self::Neutral, Self::Voice, Self::MusicBed, Self::Cinematic, Self::Telephone, Self::LoFi, Self::Underwater, Self::Podcast, Self::BassBoost, Self::BrightVoice, Self::Reverb, Self::Muffled];
    pub fn label(self) -> &'static str {
        match self {
            Self::Neutral => "Neutral", Self::Voice => "Voice Clear", Self::MusicBed => "Music Bed",
            Self::Cinematic => "Cinematic", Self::Telephone => "Telephone / Radio",
            Self::LoFi => "Lo-Fi", Self::Underwater => "Underwater",
            Self::Podcast => "Podcast / Spoken Word", Self::BassBoost => "Bass Boost",
            Self::BrightVoice => "Bright Voice", Self::Reverb => "Room Reverb", Self::Muffled => "Muffled / Distant",
        }
    }
    pub fn id(self) -> u8 { Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as u8 }
    pub fn from_id(id: u8) -> Option<Self> { Self::ALL.get(id as usize).copied() }
}

// ---------------------------------------------------------------- title / shape graphics
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GfxKind { Text, Rect, Ellipse }

#[derive(Clone, PartialEq, Debug)]
pub struct Gfx {
    pub kind: GfxKind,
    pub text: String,
    pub w: f32,
    pub h: f32,
    pub color: [u8; 4],
    pub outline: f32,
    pub outline_color: [u8; 4],
    pub mono: bool,
    pub size: f32,
    pub pad: f32,
    pub bg: [u8; 4],
    pub align: u8,
    pub shadow: bool,
}

fn unescape_gfx(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

impl Gfx {
    pub fn title(text: impl Into<String>) -> Self {
        Self { kind: GfxKind::Text, text: text.into(), w: 640.0, h: 160.0,
            color: [245, 247, 255, 255], outline: 0.0, outline_color: [0, 0, 0, 255],
            mono: false, size: 64.0, pad: 16.0, bg: [0, 0, 0, 0], align: 1, shadow: true }
    }
    pub fn rect() -> Self {
        Self { kind: GfxKind::Rect, text: String::new(), w: 480.0, h: 240.0,
            color: [60, 120, 240, 210], outline: 3.0, outline_color: [245, 247, 255, 255],
            mono: false, size: 48.0, pad: 0.0, bg: [0, 0, 0, 0], align: 1, shadow: false }
    }
    pub fn ellipse() -> Self { Self { kind: GfxKind::Ellipse, ..Self::rect() } }
}

// ---------------------------------------------------------------- clips & media
#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u32,
    /// 0 = unlinked. Clips sharing a non-zero link (video + its audio) are edited together.
    pub link: u32,
    pub lane: Lane,
    pub start: f64,
    pub dur: f64,
    /// Where in the source file this clip begins.
    pub src_off: f64,
    /// Length of the underlying source (INFINITY for placeholders): limits how far you can trim.
    pub src_len: f64,
    pub name: String,
    pub seed: u32,
    pub media: Option<u32>,
    pub fx: Fx,
    /// Ordered, non-destructive video effects applied before the clip transform.
    pub effects: Vec<EffectInstance>,
    /// Optional inbound transition applied over the start of this clip.
    pub transition_in: Option<Transition>,
    /// Optional title/shape overlay rendered over the clip.
    pub gfx: Option<Gfx>,
    /// Audio controls stored as percent gain and seconds of linear fades.
    pub audio_gain: f32,
    pub audio_preset: AudioPreset,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Clip-local gain automation (seconds from the start of this timeline clip).
    pub audio_keys: Vec<EffectKey>,
}

impl Clip {
    pub fn audio_gain_at(&self, local_t: f64) -> f32 {
        let keys = &self.audio_keys;
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return self.audio_gain };
        if local_t <= first.t { return first.v; }
        if local_t >= last.t { return last.v; }
        let k = keys.partition_point(|key| key.t <= local_t).saturating_sub(1);
        let a = keys[k];
        let b = keys[(k + 1).min(keys.len() - 1)];
        let u = ((local_t - a.t) / (b.t - a.t).max(1e-9)) as f32;
        a.v + (b.v - a.v) * a.ease.apply(u.clamp(0.0, 1.0))
    }

    pub fn audio_key_near(&self, local_t: f64) -> Option<usize> {
        self.audio_keys.iter().position(|k| (k.t - local_t).abs() <= 1.0 / 60.0)
    }

    pub fn toggle_audio_key(&mut self, local_t: f64) {
        if let Some(k) = self.audio_key_near(local_t) {
            self.audio_keys.remove(k);
        } else {
            let key = EffectKey { t: local_t.max(0.0), v: self.audio_gain_at(local_t), ease: Ease::Linear };
            let at = self.audio_keys.partition_point(|k| k.t < key.t);
            self.audio_keys.insert(at, key);
        }
    }

    pub fn set_audio_gain_at(&mut self, local_t: f64, value: f32) {
        let value = value.clamp(0.0, 400.0);
        if self.audio_keys.is_empty() {
            self.audio_gain = value;
        } else if let Some(k) = self.audio_key_near(local_t) {
            self.audio_keys[k].v = value;
        } else {
            let key = EffectKey { t: local_t.max(0.0), v: value, ease: Ease::Linear };
            let at = self.audio_keys.partition_point(|k| k.t < key.t);
            self.audio_keys.insert(at, key);
        }
    }

    pub fn end(&self) -> f64 {
        self.start + self.dur
    }
    /// Timeline time -> the clip's keyframe time (source seconds).
    pub fn fx_time(&self, t: f64) -> f64 {
        self.src_off + (t - self.start)
    }
}

#[derive(Clone)]
pub struct Media {
    pub id: u32,
    pub path: PathBuf,
    pub name: String,
    pub duration: f64,
    pub has_audio: bool,
    pub seed: u32,
    /// Optional low-resolution proxy used for interactive preview only. Export uses `path`.
    pub proxy_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
pub enum DragOp {
    /// `new_lane` applies to the grabbed clip only (same kind: video<->video, audio<->audio).
    Move { new_start: f64, new_lane: Option<Lane> },
    Slip { delta: f64 },
    TrimL { new_start: f64, ripple: bool },
    TrimR { new_end: f64, ripple: bool },
    /// Rolling edit: moves the cut between the grabbed clip and `right`.
    Roll { new_t: f64, right: u32 },
}

pub struct Project {
    pub media: Vec<Media>,
    pub clips: Vec<Clip>,
    /// Per track slot (see `Lane::slot`): clips can't be edited / the track is skipped.
    pub locked: [bool; NTRACKS],
    pub hidden: [bool; NTRACKS],
    next_id: u32,
    undo: Vec<Vec<Clip>>,
    redo: Vec<Vec<Clip>>,
}

/// Nearest target within `thr` of `v`.
pub fn nearest(targets: &[f64], v: f64, thr: f64) -> Option<f64> {
    targets
        .iter()
        .copied()
        .filter(|x| (x - v).abs() <= thr)
        .min_by(|a, b| (a - v).abs().partial_cmp(&(b - v).abs()).unwrap_or(std::cmp::Ordering::Equal))
}

/// Free space around a group on its lanes: (end of previous neighbour, start of next neighbour).
fn bounds(clips: &[Clip], ids: &[u32]) -> (f64, f64) {
    let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
    for m in clips.iter().filter(|c| ids.contains(&c.id)) {
        for x in clips.iter().filter(|x| !ids.contains(&x.id) && x.lane == m.lane) {
            if x.end() <= m.start + EPS {
                lo = lo.max(x.end());
            } else if x.start >= m.end() - EPS {
                hi = hi.min(x.start);
            }
        }
    }
    (lo, hi)
}

impl Project {
    fn empty() -> Self {
        Project {
            media: vec![],
            clips: vec![],
            locked: [false; NTRACKS],
            hidden: [false; NTRACKS],
            next_id: 0,
            undo: vec![],
            redo: vec![],
        }
    }

    /// A clean project for normal application startup.
    pub fn new() -> Self {
        Self::empty()
    }

    /// Demo timeline retained for development tests and sample fixtures only.
    pub fn demo() -> Self {
        let mut p = Project::empty();
        let story = [
            ("Cabana", 0.0, 14.0),
            ("Mountain gold", 14.0, 17.0),
            ("Jungle forest", 31.0, 16.0),
            ("Hammock", 47.0, 19.0),
            ("Waterfall", 66.0, 22.0),
            ("Beach", 88.0, 23.0),
        ];
        for (i, &(n, s, d)) in story.iter().enumerate() {
            p.push_placeholder(Lane::V1, s, d, n, i as u32 + 1);
        }
        p.push_placeholder(Lane::V(1), 12.0, 18.0, "Title_01", 11);
        p.push_placeholder(Lane::V(1), 55.0, 30.0, "Overlay.png", 12);
        p.push_placeholder(Lane::A1, 0.0, 111.0, "hip hop bluegrass", 0);
        p
    }

    fn push_placeholder(&mut self, lane: Lane, start: f64, dur: f64, name: &str, seed: u32) {
        let id = self.alloc();
        self.clips.push(Clip {
            id,
            link: 0,
            lane,
            start,
            dur,
            src_off: 0.0,
            src_len: f64::INFINITY,
            name: name.to_string(),
            seed,
            media: None,
            fx: Fx::default(),
            effects: Vec::new(),
            transition_in: None,
            gfx: None, audio_gain: 100.0, audio_preset: AudioPreset::Neutral, fade_in: 0.0, fade_out: 0.0, audio_keys: Vec::new(),
        });
    }

    /// Creates an editable title/shape overlay clip at the playhead and records one undo step.
    pub fn add_graphic(&mut self, t: f64, gfx: Gfx, name: &str) -> u32 {
        let pre = self.clips.clone();
        let id = self.alloc();
        let dur = 5.0;
        self.clips.push(Clip {
            id, link: 0, lane: Lane::V(2), start: t.max(0.0), dur, src_off: 0.0,
            src_len: f64::INFINITY, name: name.to_string(), seed: id % 9 + 1,
            media: None, fx: Fx::default(), effects: Vec::new(), transition_in: None, gfx: Some(gfx),
            audio_gain: 100.0, audio_preset: AudioPreset::Neutral, fade_in: 0.0, fade_out: 0.0, audio_keys: Vec::new(),
        });
        self.push_undo(pre);
        id
    }

    /// Applies a non-destructive effect to a video clip and records one undo step.
    pub fn apply_effect(&mut self, clip_id: u32, kind: EffectKind, local_time: f64) -> bool {
        let Some(c) = self.clip(clip_id) else { return false };
        if !c.lane.is_video() || c.gfx.is_some() || c.media.is_none() { return false; }
        let pre = self.clips.clone();
        if let Some(c) = self.clip_mut(clip_id) {
            c.effects.push(EffectInstance::new(kind, local_time));
        }
        self.push_undo(pre);
        true
    }

    pub fn remove_effect(&mut self, clip_id: u32, index: usize) -> bool {
        let Some(c) = self.clip(clip_id) else { return false };
        if index >= c.effects.len() { return false; }
        let pre = self.clips.clone();
        if let Some(c) = self.clip_mut(clip_id) { c.effects.remove(index); }
        self.push_undo(pre);
        true
    }

    pub fn apply_transition(&mut self, clip_id: u32, kind: TransitionKind, duration: f32) -> bool {
        let Some(c) = self.clip(clip_id) else { return false };
        if !c.lane.is_video() || c.gfx.is_some() || c.media.is_none() { return false; }
        let pre = self.clips.clone();
        if let Some(c) = self.clip_mut(clip_id) {
            c.transition_in = Some(Transition { kind, duration: duration.clamp(0.05, 5.0).min(c.dur as f32), enabled: true });
        }
        self.push_undo(pre);
        true
    }

    pub fn remove_transition(&mut self, clip_id: u32) -> bool {
        if !self.clip(clip_id).map_or(false, |c| c.transition_in.is_some()) { return false; }
        let pre = self.clips.clone();
        if let Some(c) = self.clip_mut(clip_id) { c.transition_in = None; }
        self.push_undo(pre);
        true
    }

    pub fn audio_gain_at(&self, t: f64) -> f32 {
        let Some(v) = self.video_clip_at(t) else { return 1.0 };
        let Some(a) = self.clips.iter().find(|a| a.link != 0 && a.link == v.link && !a.lane.is_video()) else { return 1.0 };
        if self.hidden[a.lane.slot()] { return 0.0; }
        let mut g = (a.audio_gain_at(t - a.start) / 100.0).clamp(0.0, 4.0);
        if a.fade_in > 0.0 { g *= ((t - a.start) / a.fade_in).clamp(0.0, 1.0) as f32; }
        if a.fade_out > 0.0 { g *= ((a.end() - t) / a.fade_out).clamp(0.0, 1.0) as f32; }
        g
    }

    fn alloc(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    // ---------------------------------------------------------------- queries
    pub fn end(&self) -> f64 {
        self.clips.iter().map(Clip::end).fold(0.0, f64::max)
    }
    pub fn clip(&self, id: u32) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }
    pub fn clip_mut(&mut self, id: u32) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }
    fn index_of(&self, id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == id)
    }
    pub fn media(&self, id: u32) -> Option<&Media> {
        self.media.iter().find(|m| m.id == id)
    }
    pub fn is_locked(&self, lane: Lane) -> bool {
        self.locked[lane.slot()]
    }

    /// The clip plus everything linked to it.
    pub fn group_ids(&self, id: u32) -> Vec<u32> {
        let Some(c) = self.clip(id) else { return vec![] };
        if c.link == 0 {
            return vec![id];
        }
        self.clips.iter().filter(|x| x.link == c.link).map(|x| x.id).collect()
    }

    /// Union of the groups of every id (no duplicates, locked tracks excluded).
    pub fn expand(&self, ids: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for &i in ids {
            for g in self.group_ids(i) {
                if !out.contains(&g) && self.clip(g).map_or(false, |c| !self.is_locked(c.lane)) {
                    out.push(g);
                }
            }
        }
        out
    }

    /// Topmost visible video clip under `t` (what the monitor shows).
    pub fn video_clip_at(&self, t: f64) -> Option<&Clip> {
        self.clips
            .iter()
            .filter(|c| c.lane.is_video() && c.media.is_some() && !self.hidden[c.lane.slot()] && c.start <= t && t < c.end())
            .max_by_key(|c| c.lane.slot())
    }
    pub fn next_video_clip_after(&self, t: f64) -> Option<&Clip> {
        self.clips
            .iter()
            .filter(|c| c.lane.is_video() && c.media.is_some() && !self.hidden[c.lane.slot()] && c.start > t)
            .min_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal))
    }
    /// True when the audio track linked to the active video clip is muted.
    pub fn audio_muted_at(&self, t: f64) -> bool {
        let Some(v) = self.video_clip_at(t) else { return false };
        if v.link == 0 {
            return false;
        }
        self.clips
            .iter()
            .find(|a| a.link == v.link && !a.lane.is_video())
            .map_or(false, |a| self.hidden[a.lane.slot()])
    }

    /// Snap targets: project start plus every clip edge not in `exclude`.
    pub fn snap_targets(&self, exclude: &[u32]) -> Vec<f64> {
        let mut v = vec![0.0];
        for c in self.clips.iter().filter(|c| !exclude.contains(&c.id)) {
            v.push(c.start);
            v.push(c.end());
        }
        v
    }

    // ---------------------------------------------------------------- undo / redo
    pub fn push_undo(&mut self, pre: Vec<Clip>) {
        self.undo.push(pre);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        let cur = std::mem::replace(&mut self.clips, prev);
        self.redo.push(cur);
        true
    }
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        let cur = std::mem::replace(&mut self.clips, next);
        self.undo.push(cur);
        true
    }

    // ---------------------------------------------------------------- media pool
    /// Registers a file (deduplicated by path) and returns its id.
    pub fn add_media(&mut self, path: PathBuf, name: String, duration: f64, has_audio: bool) -> u32 {
        if let Some(m) = self.media.iter().find(|m| m.path == path) {
            return m.id;
        }
        let id = self.alloc();
        self.media.push(Media { id, path, name, duration, has_audio, seed: id % 9 + 1, proxy_path: None });
        id
    }

    /// Store a preview proxy for this source. Final export always uses the original media path.
    pub fn set_proxy_path(&mut self, media_id: u32, path: Option<PathBuf>) -> bool {
        let Some(m) = self.media.iter_mut().find(|m| m.id == media_id) else { return false };
        m.proxy_path = path;
        true
    }

    /// Appends the media after the last clip on V1.
    pub fn append_media(&mut self, media_id: u32) -> bool {
        let at = self.clips.iter().filter(|c| c.lane == Lane::V1 && c.media.is_some()).map(Clip::end).fold(0.0, f64::max);
        self.place_media(media_id, Lane::V1, at, false).is_some()
    }

    /// Drops media on a video track at `start`: overwrite (default) or insert (ripples later clips).
    /// The audio goes to the matching audio track and stays linked. Returns the video clip id.
    pub fn place_media(&mut self, media_id: u32, lane: Lane, start: f64, insert: bool) -> Option<u32> {
        if !lane.is_video() || self.is_locked(lane) {
            return None;
        }
        let m = self.media(media_id)?;
        let (name, seed, audio) = (m.name.clone(), m.seed, m.has_audio);
        let known = m.duration > MIN_DUR;
        let dur = if known { m.duration } else { 10.0 };
        let src_len = if known { dur } else { f64::INFINITY };
        let start = start.max(0.0);

        let pre = self.clips.clone();
        if self.clips.iter().all(|c| c.media.is_none() && c.gfx.is_none()) {
            self.clips.clear(); // discard legacy/demo placeholders, but preserve created graphics
        }
        if insert {
            self.cut_inner(start);
            let locked = self.locked;
            for c in self.clips.iter_mut().filter(|c| !locked[c.lane.slot()] && c.start >= start - EPS) {
                c.start += dur;
            }
        }
        let alane = lane.partner();
        let with_audio = audio && !self.is_locked(alane);
        let link = if with_audio { self.alloc() } else { 0 };
        let vid = self.alloc();
        self.clips.push(Clip {
            id: vid,
            link,
            lane,
            start,
            dur,
            src_off: 0.0,
            src_len,
            name: name.clone(),
            seed,
            media: Some(media_id),
            fx: Fx::default(),
            effects: Vec::new(),
            transition_in: None,
            gfx: None, audio_gain: 100.0, audio_preset: AudioPreset::Neutral, fade_in: 0.0, fade_out: 0.0, audio_keys: Vec::new(),
        });
        let mut movers = vec![vid];
        if with_audio {
            let aid = self.alloc();
            self.clips.push(Clip {
                id: aid,
                link,
                lane: alane,
                start,
                dur,
                src_off: 0.0,
                src_len,
                name,
                seed: 0,
                media: Some(media_id),
                fx: Fx::default(),
                effects: Vec::new(),
                transition_in: None,
                gfx: None, audio_gain: 100.0, audio_preset: AudioPreset::Neutral, fade_in: 0.0, fade_out: 0.0, audio_keys: Vec::new(),
            });
            movers.push(aid);
        }
        if !insert {
            self.resolve_overwrite(&movers);
        }
        self.push_undo(pre);
        Some(vid)
    }

    // ---------------------------------------------------------------- discrete edits
    fn split_inner(&mut self, id: u32, t: f64) -> bool {
        let ids = self.group_ids(id);
        let ok = !ids.is_empty()
            && ids.iter().all(|&i| {
                self.clip(i).map_or(false, |c| !self.is_locked(c.lane) && c.start + 0.05 < t && t < c.end() - 0.05)
            });
        if !ok {
            return false;
        }
        let link = if ids.len() > 1 { self.alloc() } else { 0 };
        let mut news = Vec::new();
        for &i in &ids {
            let Some(idx) = self.index_of(i) else { continue };
            let c = self.clips[idx].clone();
            let mut n = c.clone();
            n.start = t;
            n.dur = c.end() - t;
            n.src_off = c.src_off + (t - c.start);
            n.link = link;
            self.clips[idx].dur = t - c.start;
            news.push(n);
        }
        for mut n in news {
            n.id = self.alloc();
            self.clips.push(n);
        }
        true
    }

    fn cut_inner(&mut self, t: f64) -> bool {
        let ids: Vec<u32> = self.clips.iter().map(|c| c.id).collect();
        let mut done: Vec<u32> = Vec::new();
        let mut any = false;
        for id in ids {
            if done.contains(&id) {
                continue;
            }
            let grp = self.group_ids(id);
            done.extend(&grp);
            any |= self.split_inner(id, t);
        }
        any
    }

    /// Blade one clip (and its linked partner) at `t`.
    pub fn split(&mut self, id: u32, t: f64) -> bool {
        let pre = self.clips.clone();
        let ok = self.split_inner(id, t);
        if ok {
            self.push_undo(pre);
        }
        ok
    }

    /// Add Edit: blade every clip on unlocked tracks under `t`.
    pub fn cut_at(&mut self, t: f64) -> bool {
        let pre = self.clips.clone();
        let any = self.cut_inner(t);
        if any {
            self.push_undo(pre);
        }
        any
    }

    /// Lift (leave a gap) or ripple-delete (close the gaps) the given clips and their partners.
    pub fn delete_many(&mut self, ids: &[u32], ripple: bool) -> bool {
        let ids = self.expand(ids);
        if ids.is_empty() {
            return false;
        }
        let pre = self.clips.clone();
        let mut ranges: Vec<(f64, f64)> =
            self.clips.iter().filter(|c| ids.contains(&c.id)).map(|c| (c.start, c.end())).collect();
        self.clips.retain(|c| !ids.contains(&c.id));
        if ripple {
            ranges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut merged: Vec<(f64, f64)> = Vec::new();
            for r in ranges {
                match merged.last_mut() {
                    Some(m) if r.0 <= m.1 + EPS => m.1 = m.1.max(r.1),
                    _ => merged.push(r),
                }
            }
            let locked = self.locked;
            for (s, e) in merged.into_iter().rev() {
                let d = e - s;
                for c in self.clips.iter_mut().filter(|c| !locked[c.lane.slot()] && c.start >= e - EPS) {
                    c.start = (c.start - d).max(0.0);
                }
            }
        }
        self.push_undo(pre);
        true
    }

    /// Detach selected clips from their linked partners.
    pub fn unlink(&mut self, ids: &[u32]) -> bool {
        if !ids.iter().any(|&i| self.clip(i).map_or(false, |c| c.link != 0)) {
            return false;
        }
        let pre = self.clips.clone();
        for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
            c.link = 0;
        }
        self.push_undo(pre);
        true
    }

    // ---------------------------------------------------------------- drag edits
    /// The clips a Move drag carries: grabbed group + every selected group (locked tracks excluded).
    pub fn move_set(&self, id: u32, sel: &[u32]) -> Vec<u32> {
        let mut all = vec![id];
        all.extend_from_slice(sel);
        self.expand(&all)
    }

    /// Re-derives the clip list from `pre` with the drag applied (no drift, no accumulated error).
    /// Returns true if anything differs from `pre`. Does NOT touch undo.
    pub fn apply_drag(&mut self, pre: &[Clip], id: u32, sel: &[u32], op: DragOp) -> bool {
        self.clips = pre.to_vec();
        let Some(o) = pre.iter().find(|c| c.id == id).cloned() else { return false };
        let ids = self.group_ids(id);

        match op {
            DragOp::Move { new_start, new_lane } => {
                let mv = self.move_set(id, sel);
                let min_start = pre.iter().filter(|c| mv.contains(&c.id)).map(|c| c.start).fold(f64::INFINITY, f64::min);
                let d = (new_start - o.start).max(-min_start);
                let target = new_lane.filter(|l| *l != o.lane && l.is_video() == o.lane.is_video() && !self.is_locked(*l));
                for c in self.clips.iter_mut().filter(|c| mv.contains(&c.id)) {
                    c.start += d;
                    if c.id == id {
                        if let Some(l) = target {
                            c.lane = l;
                        }
                    }
                }
            }
            DragOp::Slip { delta } => {
                let max_off = (o.src_len - o.dur).max(0.0);
                let new_off = (o.src_off - delta).max(0.0).min(max_off);
                let d = new_off - o.src_off;
                for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                    c.src_off += d;
                }
            }
            DragOp::TrimL { new_start, ripple } => {
                if ripple {
                    // clip stays put; its in-point moves; everything after shifts
                    let d = (new_start - o.start).max(-o.src_off).min((o.dur - MIN_DUR).max(0.0));
                    let from = o.end() - EPS;
                    for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                        c.src_off += d;
                        c.dur -= d;
                    }
                    if d.abs() > EPS {
                        let locked = self.locked;
                        for c in self.clips.iter_mut().filter(|c| !ids.contains(&c.id) && !locked[c.lane.slot()] && c.start >= from) {
                            c.start = (c.start - d).max(0.0);
                        }
                    }
                } else {
                    let (lo, _) = bounds(pre, &ids);
                    let min_s = (o.start - o.src_off).max(lo).max(0.0);
                    let max_s = (o.end() - MIN_DUR).max(min_s);
                    let ns = new_start.max(min_s).min(max_s);
                    let d = ns - o.start;
                    for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                        c.start += d;
                        c.dur -= d;
                        c.src_off += d;
                    }
                }
            }
            DragOp::TrimR { new_end, ripple } => {
                let (_, nb) = bounds(pre, &ids);
                let lo = o.start + MIN_DUR;
                let mut hi = o.start + (o.src_len - o.src_off);
                if !ripple {
                    hi = hi.min(nb);
                }
                let ne = new_end.max(lo).min(hi.max(lo));
                let d = ne - o.end();
                for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                    c.dur += d;
                }
                if ripple && d.abs() > EPS {
                    let from = o.end() - EPS;
                    let locked = self.locked;
                    for c in self.clips.iter_mut().filter(|c| !ids.contains(&c.id) && !locked[c.lane.slot()] && c.start >= from) {
                        c.start = (c.start + d).max(0.0);
                    }
                }
            }
            DragOp::Roll { new_t, right } => {
                let Some(r0) = pre.iter().find(|c| c.id == right).cloned() else { return false };
                let rids = self.group_ids(right);
                let lo = (o.start + MIN_DUR).max(r0.start - r0.src_off);
                let hi = (r0.end() - MIN_DUR).min(o.start + (o.src_len - o.src_off));
                let nt = new_t.max(lo).min(hi.max(lo));
                for c in self.clips.iter_mut() {
                    if ids.contains(&c.id) {
                        c.dur = nt - c.start;
                    } else if rids.contains(&c.id) {
                        let d = nt - c.start;
                        c.start = nt;
                        c.dur -= d;
                        c.src_off += d;
                    }
                }
            }
        }
        self.clips
            .iter()
            .zip(pre)
            .any(|(a, b)| a.lane != b.lane || (a.start - b.start).abs() > EPS || (a.dur - b.dur).abs() > EPS || (a.src_off - b.src_off).abs() > EPS)
    }

    /// After dropping moved/placed clips: whatever they now cover on the same lane is trimmed,
    /// split or removed (overwrite edit).
    pub fn resolve_overwrite(&mut self, ids: &[u32]) {
        let ids: Vec<u32> = ids.to_vec();
        let movers: Vec<(Lane, f64, f64)> =
            self.clips.iter().filter(|c| ids.contains(&c.id)).map(|c| (c.lane, c.start, c.end())).collect();
        let mut relink: Vec<(u32, u32)> = Vec::new();

        for (lane, a, b) in movers {
            let mut spill: Vec<Clip> = Vec::new();
            self.clips.retain_mut(|c| {
                if c.lane != lane || ids.contains(&c.id) {
                    return true;
                }
                let (s, e) = (c.start, c.end());
                if e <= a + EPS || s >= b - EPS {
                    return true; // no overlap
                }
                if s >= a - EPS && e <= b + EPS {
                    return false; // fully covered
                }
                if s < a && e > b {
                    // covered in the middle: keep the head, spill the tail into a new clip
                    let mut right = c.clone();
                    right.start = b;
                    right.dur = e - b;
                    right.src_off = c.src_off + (b - s);
                    c.dur = a - s;
                    spill.push(right);
                    return true;
                }
                if s < a {
                    c.dur = a - s; // tail covered
                } else {
                    let d = b - s; // head covered
                    c.start = b;
                    c.dur -= d;
                    c.src_off += d;
                }
                true
            });
            for mut r in spill {
                r.id = self.alloc();
                if r.link != 0 {
                    let nl = match relink.iter().find(|(old, _)| *old == r.link) {
                        Some(&(_, n)) => n,
                        None => {
                            let n = self.alloc();
                            relink.push((r.link, n));
                            n
                        }
                    };
                    r.link = nl;
                }
                self.clips.push(r);
            }
        }
    }

    // ---------------------------------------------------------------- save / load
    /// Plain-text project format (tab separated; names/paths are the last field of a line).
    pub fn to_text(&self) -> String {
        let mut s = String::from("YORU 9\n");
        s += &format!("N\t{}\n", self.next_id);
        for m in &self.media {
            s += &format!(
                "M\t{}\t{}\t{}\t{}\t{}\t{}\n",
                m.id, m.duration, m.has_audio as u8, m.seed, m.name, m.path.to_string_lossy()
            );
            if let Some(proxy) = &m.proxy_path {
                s += &format!("P\t{}\t{}\n", m.id, proxy.to_string_lossy());
            }
        }
        for c in &self.clips {
            s += &format!(
                "C\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                c.id,
                c.link,
                c.lane.token(),
                c.start,
                c.dur,
                c.src_off,
                c.src_len,
                c.seed,
                c.media.unwrap_or(0),
            );
            for v in c.fx.base {
                s += &format!("\t{v}");
            }
            s += &format!("\t{}\n", c.name);
        }
        for c in &self.clips {
            if let Some(g) = &c.gfx {
                let kind = match g.kind { GfxKind::Text => 0, GfxKind::Rect => 1, GfxKind::Ellipse => 2 };
                let esc = g.text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n");
                s += &format!("G\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    c.id, kind, g.w, g.h, g.color[0], g.color[1], g.color[2], g.color[3],
                    g.outline, g.outline_color[0], g.outline_color[1], g.outline_color[2], g.outline_color[3],
                    g.mono as u8, g.size, g.pad, g.bg[0], g.bg[1], g.bg[2], g.bg[3], g.align, g.shadow as u8, esc);
            }
            s += &format!("A\t{}\t{}\t{}\t{}\n", c.id, c.audio_gain, c.fade_in, c.fade_out);
            s += &format!("AP\t{}\t{}\n", c.id, c.audio_preset.id());
            for key in &c.audio_keys {
                s += &format!("AK\t{}\t{}\t{}\t{}\n", c.id, key.t, key.v, key.ease.id());
            }
            if let Some(tr) = c.transition_in {
                s += &format!("TR\t{}\t{}\t{}\t{}\n", c.id, tr.kind.id(), tr.enabled as u8, tr.duration);
            }
            for (order, effect) in c.effects.iter().enumerate() {
                s += &format!("E\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", c.id, order,
                    effect.kind.id(), effect.enabled as u8, effect.params[0], effect.params[1], effect.params[2], effect.params[3]);
                for (pi, keys) in effect.keys.iter().enumerate() {
                    for key in keys {
                        s += &format!("Q\t{}\t{}\t{}\t{}\t{}\t{}\n", c.id, order, pi, key.t, key.v, key.ease.id());
                    }
                }
            }
        }
        for c in &self.clips {
            for (pi, keys) in c.fx.keys.iter().enumerate() {
                for k in keys {
                    s += &format!("K\t{}\t{}\t{}\t{}\t{}\n", c.id, pi, k.t, k.v, k.ease.id());
                }
            }
        }
        s
    }

    pub fn from_text(text: &str) -> Result<Project, String> {
        let mut lines = text.lines();
        let ver = match lines.next().map(str::trim) {
            Some("YORU 1") => 1,
            Some("YORU 2") => 2,
            Some("YORU 3") => 3,
            Some("YORU 4") => 4,
            Some("YORU 5") => 5,
            Some("YORU 6") => 6,
            Some("YORU 7") => 7,
            Some("YORU 8") => 8,
            Some("YORU 9") => 9,
            _ => return Err("not a Yoru project file".into()),
        };
        let mut p = Project::empty();
        for (n, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let bad = || format!("line {}: damaged record", n + 2);
            let f: Vec<&str> = if line.starts_with("C\t") { line.splitn(19, '\t').collect() } else if line.starts_with("P\t") { line.splitn(3, '\t').collect() } else { line.splitn(32, '\t').collect() };
            match f[0] {
                "N" => p.next_id = f.get(1).and_then(|v| v.parse().ok()).ok_or_else(bad)?,
                "M" if f.len() >= 7 => p.media.push(Media {
                    id: f[1].parse().map_err(|_| bad())?,
                    duration: f[2].parse().map_err(|_| bad())?,
                    has_audio: f[3] == "1",
                    seed: f[4].parse().map_err(|_| bad())?,
                    name: f[5].to_string(),
                    path: PathBuf::from(f[6..].join("\t")),
                    proxy_path: None,
                }),
                "C" if (ver == 1 && f.len() == 11) || (ver >= 2 && f.len() == 19) => {
                    let media: u32 = f[9].parse().map_err(|_| bad())?;
                    let mut fx = Fx::default();
                    if ver >= 3 {
                        for (i, slot) in fx.base.iter_mut().enumerate() {
                            *slot = f[10 + i].parse().map_err(|_| bad())?;
                        }
                    } // v2 stored effects in different units and was never released: ignore them
                    p.clips.push(Clip {
                        id: f[1].parse().map_err(|_| bad())?,
                        link: f[2].parse().map_err(|_| bad())?,
                        lane: Lane::parse(f[3]).ok_or_else(bad)?,
                        start: f[4].parse().map_err(|_| bad())?,
                        dur: f[5].parse().map_err(|_| bad())?,
                        src_off: f[6].parse().map_err(|_| bad())?,
                        src_len: f[7].parse().map_err(|_| bad())?,
                        seed: f[8].parse().map_err(|_| bad())?,
                        media: if media == 0 { None } else { Some(media) },
                        fx,
                        effects: Vec::new(),
                        transition_in: None,
                        gfx: None, audio_gain: 100.0, audio_preset: AudioPreset::Neutral, fade_in: 0.0, fade_out: 0.0, audio_keys: Vec::new(),
                        name: f[f.len() - 1].to_string(),
                    });
                }
                "P" if ver >= 7 && f.len() == 3 => {
                    let id: u32 = f[1].parse().map_err(|_| bad())?;
                    if let Some(m) = p.media.iter_mut().find(|m| m.id == id) { m.proxy_path = Some(PathBuf::from(f[2])); }
                }
                "A" if f.len() == 5 => {
                    let id: u32 = f[1].parse().map_err(|_| bad())?;
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == id) {
                        c.audio_gain = f[2].parse().map_err(|_| bad())?;
                        c.fade_in = f[3].parse().map_err(|_| bad())?;
                        c.fade_out = f[4].parse().map_err(|_| bad())?;
                    }
                }
                "AP" if ver >= 8 && f.len() == 3 => {
                    let id: u32 = f[1].parse().map_err(|_| bad())?;
                    let preset = AudioPreset::from_id(f[2].parse().map_err(|_| bad())?).ok_or_else(bad)?;
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == id) { c.audio_preset = preset; }
                }
                "E" if ver >= 5 && f.len() == 9 => {
                    let cid: u32 = f[1].parse().map_err(|_| bad())?;
                    let order: usize = f[2].parse().map_err(|_| bad())?;
                    let kind = EffectKind::from_id(f[3].parse().map_err(|_| bad())?).ok_or_else(bad)?;
                    let mut params = [0.0f32; 4];
                    for i in 0..4 { params[i] = f[5 + i].parse().map_err(|_| bad())?; }
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == cid) {
                        let effect = EffectInstance { kind, enabled: f[4] == "1", params, keys: Default::default() };
                        let at = order.min(c.effects.len());
                        c.effects.insert(at, effect);
                    }
                }
                "Q" if ver >= 6 && f.len() == 7 => {
                    let cid: u32 = f[1].parse().map_err(|_| bad())?;
                    let order: usize = f[2].parse().map_err(|_| bad())?;
                    let pi: usize = f[3].parse().map_err(|_| bad())?;
                    let key = EffectKey {
                        t: f[4].parse().map_err(|_| bad())?,
                        v: f[5].parse().map_err(|_| bad())?,
                        ease: Ease::from_id(f[6].parse().map_err(|_| bad())?),
                    };
                    if pi >= 4 { return Err(bad()); }
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == cid) {
                        if let Some(effect) = c.effects.get_mut(order) {
                            effect.keys[pi].push(key);
                        }
                    }
                }
                "AK" if ver >= 7 && f.len() == 5 => {
                    let id: u32 = f[1].parse().map_err(|_| bad())?;
                    let key = EffectKey {
                        t: f[2].parse().map_err(|_| bad())?,
                        v: f[3].parse().map_err(|_| bad())?,
                        ease: Ease::from_id(f[4].parse().map_err(|_| bad())?),
                    };
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == id) { c.audio_keys.push(key); }
                }
                "TR" if ver >= 6 && f.len() == 5 => {
                    let cid: u32 = f[1].parse().map_err(|_| bad())?;
                    let kind = TransitionKind::from_id(f[2].parse().map_err(|_| bad())?).ok_or_else(bad)?;
                    let transition = Transition {
                        kind,
                        enabled: f[3] == "1",
                        duration: f[4].parse::<f32>().map_err(|_| bad())?.clamp(0.05, 5.0),
                    };
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == cid) { c.transition_in = Some(transition); }
                }
                "G" if f.len() >= 24 => {
                    let id: u32 = f[1].parse().map_err(|_| bad())?;
                    let kind = match f[2].parse::<u8>().map_err(|_| bad())? {
                        0 => GfxKind::Text, 1 => GfxKind::Rect, 2 => GfxKind::Ellipse, _ => return Err(bad()),
                    };
                    let unesc = unescape_gfx(&f[23..].join("\t"));
                    let color = [f[5].parse().map_err(|_| bad())?, f[6].parse().map_err(|_| bad())?, f[7].parse().map_err(|_| bad())?, f[8].parse().map_err(|_| bad())?];
                    let outline_color = [f[10].parse().map_err(|_| bad())?, f[11].parse().map_err(|_| bad())?, f[12].parse().map_err(|_| bad())?, f[13].parse().map_err(|_| bad())?];
                    let bg = [f[17].parse().map_err(|_| bad())?, f[18].parse().map_err(|_| bad())?, f[19].parse().map_err(|_| bad())?, f[20].parse().map_err(|_| bad())?];
                    let g = Gfx { kind, text: unesc, w: f[3].parse().map_err(|_| bad())?, h: f[4].parse().map_err(|_| bad())?,
                        color, outline: f[9].parse().map_err(|_| bad())?, outline_color, mono: f[14] == "1",
                        size: f[15].parse().map_err(|_| bad())?, pad: f[16].parse().map_err(|_| bad())?,
                        bg, align: f[21].parse().map_err(|_| bad())?, shadow: f[22] == "1" };
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == id) { c.gfx = Some(g); }
                }
                "K" if f.len() == 6 => {
                    let cid: u32 = f[1].parse().map_err(|_| bad())?;
                    let pi: usize = f[2].parse().map_err(|_| bad())?;
                    if pi >= NPARAM {
                        return Err(bad());
                    }
                    let key = Key {
                        t: f[3].parse().map_err(|_| bad())?,
                        v: f[4].parse().map_err(|_| bad())?,
                        ease: Ease::from_id(f[5].parse().map_err(|_| bad())?),
                    };
                    if let Some(c) = p.clips.iter_mut().find(|c| c.id == cid) {
                        c.fx.keys[pi].push(key);
                    }
                }
                _ => return Err(bad()),
            }
        }
        for c in p.clips.iter_mut() {
            for k in c.fx.keys.iter_mut() {
                k.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
            }
            for effect in c.effects.iter_mut() {
                for keys in effect.keys.iter_mut() {
                    keys.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
                }
            }
        }
        let max_id = p.media.iter().map(|m| m.id).chain(p.clips.iter().flat_map(|c| [c.id, c.link])).max().unwrap_or(0);
        p.next_id = p.next_id.max(max_id);
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn new_project_starts_empty() {
        let p = Project::new();
        assert!(p.media.is_empty());
        assert!(p.clips.is_empty());
    }

    fn media_project() -> (Project, u32) {
        let mut p = Project::demo();
        let m = p.add_media(PathBuf::from("a.mp4"), "a".into(), 10.0, true);
        assert!(p.append_media(m));
        (p, m)
    }

    #[test]
    fn append_replaces_placeholders_and_links_audio() {
        let (p, _) = media_project();
        assert_eq!(p.clips.len(), 2);
        assert_eq!(p.clips[0].link, p.clips[1].link);
        assert_eq!(p.clips[0].lane, Lane::V1);
        assert_eq!(p.clips[1].lane, Lane::A1);
        assert!((p.end() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn split_is_undoable_and_splits_the_pair() {
        let (mut p, _) = media_project();
        let id = p.clips[0].id;
        assert!(p.split(id, 4.0));
        assert_eq!(p.clips.len(), 4);
        assert!(p.undo());
        assert_eq!(p.clips.len(), 2);
        assert!(p.redo());
        assert_eq!(p.clips.len(), 4);
    }

    #[test]
    fn locked_track_blocks_blade() {
        let (mut p, _) = media_project();
        p.locked[Lane::A1.slot()] = true;
        let id = p.clips[0].id;
        assert!(!p.split(id, 4.0)); // partner is on a locked track -> whole group refuses
        assert_eq!(p.clips.len(), 2);
    }

    #[test]
    fn trim_right_cannot_exceed_source() {
        let (mut p, _) = media_project();
        let pre = p.clips.clone();
        let id = pre[0].id;
        p.apply_drag(&pre, id, &[], DragOp::TrimR { new_end: 99.0, ripple: false });
        assert!((p.clips[0].end() - 10.0).abs() < 1e-9);
        p.apply_drag(&pre, id, &[], DragOp::TrimR { new_end: 6.0, ripple: false });
        assert!((p.clips[0].end() - 6.0).abs() < 1e-9);
        assert!((p.clips[1].end() - 6.0).abs() < 1e-9); // partner follows
    }

    #[test]
    fn rolling_edit_moves_the_cut() {
        let (mut p, _) = media_project();
        let id = p.clips[0].id;
        assert!(p.split(id, 5.0));
        let left = p.clips.iter().find(|c| c.lane == Lane::V1 && c.start == 0.0).unwrap().id;
        let right = p.clips.iter().find(|c| c.lane == Lane::V1 && c.start == 5.0).unwrap().id;
        let pre = p.clips.clone();
        assert!(p.apply_drag(&pre, left, &[], DragOp::Roll { new_t: 7.0, right }));
        let l = p.clip(left).unwrap();
        let r = p.clip(right).unwrap();
        assert!((l.end() - 7.0).abs() < 1e-9 && (r.start - 7.0).abs() < 1e-9);
        assert!((r.src_off - 7.0).abs() < 1e-9); // right clip's in-point moved with the cut
    }

    #[test]
    fn move_to_another_track_and_overwrite() {
        let (mut p, m) = media_project();
        // second clip on V2 over the same time range
        let v2 = p.place_media(m, Lane::V(1), 0.0, false).unwrap();
        assert_eq!(p.clip(v2).unwrap().lane, Lane::V(1));
        let pre = p.clips.clone();
        let v1 = pre.iter().find(|c| c.lane == Lane::V1).unwrap().id;
        // drag the V2 clip down to V1: it overwrites what was there
        assert!(p.apply_drag(&pre, v2, &[], DragOp::Move { new_start: 0.0, new_lane: Some(Lane::V1) }));
        let set = p.move_set(v2, &[]);
        p.resolve_overwrite(&set);
        assert!(p.clip(v1).is_none());
        assert_eq!(p.clip(v2).unwrap().lane, Lane::V1);
    }

    #[test]
    fn insert_pushes_later_clips() {
        let (mut p, m) = media_project();
        p.place_media(m, Lane::V1, 4.0, true).unwrap(); // splits at 4, inserts 10s
        assert!((p.end() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn ripple_delete_closes_the_gap() {
        let (mut p, m) = media_project();
        assert!(p.append_media(m)); // second copy at 10..20
        let first = p.clips[0].id;
        assert!(p.delete_many(&[first], true));
        assert!((p.end() - 10.0).abs() < 1e-9);
        assert!(p.clips.iter().all(|c| c.start.abs() < 1e-9));
    }

    #[test]
    fn keyframes_interpolate_and_survive_save_load() {
        let mut fx = Fx::default();
        fx.toggle_anim(4, 0.0); // opacity animated, key at 0 = 100
        fx.set_value(4, 2.0, 0.0); // key at 2 = 0
        fx.keys[4][0].ease = Ease::Linear;
        assert!((fx.value(4, 1.0) - 50.0).abs() < 1e-3);
        assert!((fx.value(4, 5.0) - 0.0).abs() < 1e-3);

        let (mut p, _) = media_project();
        let id = p.clips[0].id;
        p.clip_mut(id).unwrap().fx = fx.clone();
        let q = Project::from_text(&p.to_text()).expect("parse");
        assert_eq!(q.clips[0].fx, fx);
        assert!(q.clips[1].fx.is_identity());
    }

    #[test]
    fn save_load_round_trip() {
        let (p, _) = media_project();
        let q = Project::from_text(&p.to_text()).expect("parse");
        assert_eq!(q.clips.len(), p.clips.len());
        assert_eq!(q.media.len(), 1);
        assert!((q.end() - p.end()).abs() < 1e-9);
        assert_eq!(q.clips[0].link, q.clips[1].link);
        assert_eq!(q.clips[1].lane, Lane::A1);
    }
    #[test]
    fn graphics_and_audio_settings_survive_save_load_and_undo() {
        let mut p = Project::empty();
        let mut g = Gfx::title("hello\nworld");
        g.color = [12, 34, 56, 200];
        g.outline = 2.5;
        let id = p.add_graphic(3.0, g.clone(), "Title");
        let c = p.clip_mut(id).unwrap();
        c.audio_gain = 82.0;
        c.fade_in = 0.75;
        c.fade_out = 1.25;
        let q = Project::from_text(&p.to_text()).expect("graphics project parses");
        assert_eq!(q.clip(id).unwrap().gfx.as_ref(), Some(&g));
        assert_eq!(q.clip(id).unwrap().audio_gain, 82.0);
        assert_eq!(q.clip(id).unwrap().fade_in, 0.75);
        assert_eq!(q.clip(id).unwrap().fade_out, 1.25);
        assert!(p.undo());
        assert!(p.clip(id).is_none());
        assert!(p.redo());
        assert_eq!(p.clip(id).unwrap().gfx.as_ref(), Some(&g));
    }


    #[test]
    fn yoru9_proxy_transition_effect_and_audio_keys_round_trip() {
        let (mut p, media_id) = media_project();
        p.set_proxy_path(media_id, Some(PathBuf::from("proxies/a.mp4")));
        let video_id = p.clips.iter().find(|c| c.lane.is_video()).unwrap().id;
        let audio_id = p.clips.iter().find(|c| !c.lane.is_video()).unwrap().id;
        {
            let video = p.clip_mut(video_id).unwrap();
            video.transition_in = Some(Transition { kind: TransitionKind::DiamondIris, duration: 0.6, enabled: true });
            let mut effect = EffectInstance::new(EffectKind::Wave, 0.0);
            effect.keys[0].push(EffectKey { t: 0.0, v: 5.0, ease: Ease::Linear });
            effect.keys[0].push(EffectKey { t: 1.0, v: 32.0, ease: Ease::EaseOut });
            video.effects.push(effect);
        }
        p.clip_mut(audio_id).unwrap().audio_keys.push(EffectKey { t: 0.5, v: 82.0, ease: Ease::EaseInOut });
        p.clip_mut(audio_id).unwrap().audio_preset = AudioPreset::Podcast;

        let saved = p.to_text();
        assert!(saved.starts_with("YORU 9\n"));
        let q = Project::from_text(&saved).expect("YORU 9 project parses");
        assert_eq!(q.media(media_id).unwrap().proxy_path.as_deref(), Some(Path::new("proxies/a.mp4")));
        assert_eq!(q.clip(video_id).unwrap().transition_in.unwrap().kind, TransitionKind::DiamondIris);
        assert_eq!(q.clip(video_id).unwrap().effects[0].keys[0].len(), 2);
        assert_eq!(q.clip(audio_id).unwrap().audio_keys[0].v, 82.0);
        assert_eq!(q.clip(audio_id).unwrap().audio_preset, AudioPreset::Podcast);
    }

    #[test]
    fn yoru8_projects_still_load_after_v9_expansion() {
        let (p, _) = media_project();
        let old_text = p.to_text().replacen("YORU 9\n", "YORU 8\n", 1);
        let q = Project::from_text(&old_text).expect("YORU 8 project remains readable");
        for c in &q.clips {
            assert_eq!(c.audio_preset, AudioPreset::Neutral);
        }
    }

    #[test]
    fn applied_effect_stack_survives_save_load_and_undo() {
        let (mut p, _) = media_project();
        let id = p.clips[0].id;
        let before = p.clips.clone();
        p.clip_mut(id).unwrap().effects.push(EffectInstance::new(EffectKind::Wave, 0.75));
        p.clip_mut(id).unwrap().effects.push(EffectInstance::new(EffectKind::GaussianBlur, 0.75));
        p.clip_mut(id).unwrap().effects.push(EffectInstance::new(EffectKind::VHS, 0.9));
        let saved = p.to_text();
        let q = Project::from_text(&saved).expect("effect project parses");
        assert_eq!(q.clip(id).unwrap().effects, p.clip(id).unwrap().effects);
        p.push_undo(before);
        assert!(p.undo());
        assert!(p.clip(id).unwrap().effects.is_empty());
    }

    #[test]
    fn expanded_effect_and_transition_ids_are_append_only() {
        assert_eq!(EffectKind::from_id(0), Some(EffectKind::Wave));
        assert_eq!(EffectKind::from_id(11), Some(EffectKind::FadeOut));
        assert_eq!(EffectKind::from_id(19), Some(EffectKind::Scanlines));
        assert_eq!(EffectKind::from_id(23), Some(EffectKind::VHS));
        assert_eq!(TransitionKind::from_id(0), Some(TransitionKind::Dissolve));
        assert_eq!(TransitionKind::from_id(7), Some(TransitionKind::CircleIris));
        assert_eq!(TransitionKind::from_id(9), Some(TransitionKind::DiamondIris));
        assert_eq!(AudioPreset::from_id(6), Some(AudioPreset::Underwater));
        assert_eq!(AudioPreset::from_id(7), Some(AudioPreset::Podcast));
        assert_eq!(AudioPreset::from_id(11), Some(AudioPreset::Muffled));
    }

}
