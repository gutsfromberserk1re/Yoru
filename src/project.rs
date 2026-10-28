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
            Ease::EaseInOut => "Smooth",
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
                self.insert_key(i, Key { t, v, ease: Ease::EaseInOut });
            }
        }
    }

    /// Stopwatch: start animating (first key at `t`) or stop (keys removed, value frozen at `t`).
    pub fn toggle_anim(&mut self, i: usize, t: f64) {
        if self.keys[i].is_empty() {
            let v = self.base[i];
            self.keys[i].push(Key { t, v, ease: Ease::EaseInOut });
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
                self.insert_key(i, Key { t, v, ease: Ease::EaseInOut });
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
}

impl Clip {
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

    /// Placeholder project shown until real media is imported.
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
        });
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
            .filter(|c| c.lane.is_video() && !self.hidden[c.lane.slot()] && c.start <= t && t < c.end())
            .max_by_key(|c| c.lane.slot())
    }
    pub fn next_video_clip_after(&self, t: f64) -> Option<&Clip> {
        self.clips
            .iter()
            .filter(|c| c.lane.is_video() && !self.hidden[c.lane.slot()] && c.start > t)
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
        self.media.push(Media { id, path, name, duration, has_audio, seed: id % 9 + 1 });
        id
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
        if self.clips.iter().all(|c| c.media.is_none()) {
            self.clips.clear(); // first real import replaces the placeholder project
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
        let mut s = String::from("YORU 3\n");
        s += &format!("N\t{}\n", self.next_id);
        for m in &self.media {
            s += &format!(
                "M\t{}\t{}\t{}\t{}\t{}\t{}\n",
                m.id,
                m.duration,
                m.has_audio as u8,
                m.seed,
                m.name,
                m.path.to_string_lossy()
            );
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
            _ => return Err("not a Yoru project file".into()),
        };
        let mut p = Project::empty();
        for (n, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let bad = || format!("line {}: damaged record", n + 2);
            let f: Vec<&str> = line.splitn(19, '\t').collect();
            match f[0] {
                "N" => p.next_id = f.get(1).and_then(|v| v.parse().ok()).ok_or_else(bad)?,
                "M" if f.len() >= 7 => p.media.push(Media {
                    id: f[1].parse().map_err(|_| bad())?,
                    duration: f[2].parse().map_err(|_| bad())?,
                    has_audio: f[3] == "1",
                    seed: f[4].parse().map_err(|_| bad())?,
                    name: f[5].to_string(),
                    path: PathBuf::from(f[6..].join("\t")),
                }),
                "C" if (ver == 1 && f.len() == 11) || (ver >= 2 && f.len() == 19) => {
                    let media: u32 = f[9].parse().map_err(|_| bad())?;
                    let mut fx = Fx::default();
                    if ver == 3 {
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
                        name: f[f.len() - 1].to_string(),
                    });
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
        }
        let max_id = p.media.iter().map(|m| m.id).chain(p.clips.iter().flat_map(|c| [c.id, c.link])).max().unwrap_or(0);
        p.next_id = p.next_id.max(max_id);
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
