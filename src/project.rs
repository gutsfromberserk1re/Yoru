//! Project model: media pool, clips, edit operations, undo/redo.
//! Pure data + logic (no egui, no ffmpeg) so it stays easy to reason about and to test.
//!
//! Undo model: every edit snapshots the clip list *before* the change. Clip lists are tiny
//! (hundreds of entries), so a snapshot is cheaper and far less bug-prone than per-edit inverses,
//! and a whole mouse drag collapses into one undo step.

use std::path::PathBuf;

const EPS: f64 = 1e-6;
pub const MIN_DUR: f64 = 0.1;
const UNDO_LIMIT: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    Conn,
    Main,
    Audio,
}

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
}

impl Clip {
    pub fn end(&self) -> f64 {
        self.start + self.dur
    }
}

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
    Move { new_start: f64 },
    Slip { delta: f64 },
    TrimL { new_start: f64 },
    TrimR { new_end: f64, ripple: bool },
}

pub struct Project {
    pub media: Vec<Media>,
    pub clips: Vec<Clip>,
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
    /// Placeholder project shown until real media is imported.
    pub fn demo() -> Self {
        let mut p = Project { media: vec![], clips: vec![], next_id: 0, undo: vec![], redo: vec![] };
        let story = [
            ("Cabana", 0.0, 14.0),
            ("Mountain gold", 14.0, 17.0),
            ("Jungle forest", 31.0, 16.0),
            ("Hammock", 47.0, 19.0),
            ("Waterfall", 66.0, 22.0),
            ("Beach", 88.0, 23.0),
        ];
        for (i, &(n, s, d)) in story.iter().enumerate() {
            p.push_placeholder(Lane::Main, s, d, n, i as u32 + 1);
        }
        p.push_placeholder(Lane::Conn, 12.0, 18.0, "Title_01", 11);
        p.push_placeholder(Lane::Conn, 55.0, 30.0, "Overlay.png", 12);
        p.push_placeholder(Lane::Audio, 0.0, 111.0, "hip hop bluegrass", 0);
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
    fn index_of(&self, id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == id)
    }
    pub fn media(&self, id: u32) -> Option<&Media> {
        self.media.iter().find(|m| m.id == id)
    }
    /// The clip plus everything linked to it.
    pub fn group_ids(&self, id: u32) -> Vec<u32> {
        let Some(c) = self.clip(id) else { return vec![] };
        if c.link == 0 {
            return vec![id];
        }
        self.clips.iter().filter(|x| x.link == c.link).map(|x| x.id).collect()
    }
    /// Topmost Main-lane clip under `t` (what the monitor shows).
    pub fn main_clip_at(&self, t: f64) -> Option<&Clip> {
        self.clips.iter().rev().find(|c| c.lane == Lane::Main && c.start <= t && t < c.end())
    }
    pub fn next_main_clip_after(&self, t: f64) -> Option<&Clip> {
        self.clips
            .iter()
            .filter(|c| c.lane == Lane::Main && c.start > t)
            .min_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal))
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

    // ---------------------------------------------------------------- save / load
    /// Plain-text project format (tab separated; names/paths are the last field of a line).
    pub fn to_text(&self) -> String {
        let mut s = String::from("YORU 1\n");
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
            let lane = match c.lane {
                Lane::Conn => 0,
                Lane::Main => 1,
                Lane::Audio => 2,
            };
            s += &format!(
                "C\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.id,
                c.link,
                lane,
                c.start,
                c.dur,
                c.src_off,
                c.src_len,
                c.seed,
                c.media.unwrap_or(0),
                c.name
            );
        }
        s
    }

    pub fn from_text(text: &str) -> Result<Project, String> {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("YORU 1") {
            return Err("not a Yoru project file".into());
        }
        let mut p = Project { media: vec![], clips: vec![], next_id: 0, undo: vec![], redo: vec![] };
        for (n, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let bad = || format!("line {}: damaged record", n + 2);
            let f: Vec<&str> = line.splitn(11, '\t').collect();
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
                "C" if f.len() == 11 => {
                    let media: u32 = f[9].parse().map_err(|_| bad())?;
                    p.clips.push(Clip {
                        id: f[1].parse().map_err(|_| bad())?,
                        link: f[2].parse().map_err(|_| bad())?,
                        lane: match f[3] {
                            "0" => Lane::Conn,
                            "1" => Lane::Main,
                            "2" => Lane::Audio,
                            _ => return Err(bad()),
                        },
                        start: f[4].parse().map_err(|_| bad())?,
                        dur: f[5].parse().map_err(|_| bad())?,
                        src_off: f[6].parse().map_err(|_| bad())?,
                        src_len: f[7].parse().map_err(|_| bad())?,
                        seed: f[8].parse().map_err(|_| bad())?,
                        media: if media == 0 { None } else { Some(media) },
                        name: f[10].to_string(),
                    });
                }
                _ => return Err(bad()),
            }
        }
        let max_id = p.media.iter().map(|m| m.id).chain(p.clips.iter().flat_map(|c| [c.id, c.link])).max().unwrap_or(0);
        p.next_id = p.next_id.max(max_id);
        Ok(p)
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

    /// Appends the media to the end of the Main lane (video + linked audio).
    pub fn append_media(&mut self, media_id: u32) -> bool {
        let Some(m) = self.media(media_id) else { return false };
        let (name, seed, audio) = (m.name.clone(), m.seed, m.has_audio);
        let known = m.duration > MIN_DUR;
        let dur = if known { m.duration } else { 10.0 };
        let src_len = if known { dur } else { f64::INFINITY };

        let pre = self.clips.clone();
        if self.clips.iter().all(|c| c.media.is_none()) {
            self.clips.clear(); // first real import replaces the placeholder project
        }
        let at = self.clips.iter().filter(|c| c.lane == Lane::Main).map(Clip::end).fold(0.0, f64::max);
        let link = if audio { self.alloc() } else { 0 };
        let vid = self.alloc();
        self.clips.push(Clip {
            id: vid,
            link,
            lane: Lane::Main,
            start: at,
            dur,
            src_off: 0.0,
            src_len,
            name: name.clone(),
            seed,
            media: Some(media_id),
        });
        if audio {
            let aid = self.alloc();
            self.clips.push(Clip {
                id: aid,
                link,
                lane: Lane::Audio,
                start: at,
                dur,
                src_off: 0.0,
                src_len,
                name,
                seed: 0,
                media: Some(media_id),
            });
        }
        self.push_undo(pre);
        true
    }

    // ---------------------------------------------------------------- discrete edits
    fn split_inner(&mut self, id: u32, t: f64) -> bool {
        let ids = self.group_ids(id);
        let ok = !ids.is_empty()
            && ids.iter().all(|&i| self.clip(i).map_or(false, |c| c.start + 0.05 < t && t < c.end() - 0.05));
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

    /// Blade one clip (and its linked partner) at `t`.
    pub fn split(&mut self, id: u32, t: f64) -> bool {
        let pre = self.clips.clone();
        let ok = self.split_inner(id, t);
        if ok {
            self.push_undo(pre);
        }
        ok
    }

    /// Blade every clip under `t`.
    pub fn cut_at(&mut self, t: f64) -> bool {
        let pre = self.clips.clone();
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
        if any {
            self.push_undo(pre);
        }
        any
    }

    /// Lift (leave a gap) or ripple-delete (close the gap) a clip and its partner.
    pub fn delete(&mut self, id: u32, ripple: bool) -> bool {
        let ids = self.group_ids(id);
        if ids.is_empty() {
            return false;
        }
        let pre = self.clips.clone();
        let b = self.clips.iter().filter(|c| ids.contains(&c.id)).map(Clip::end).fold(0.0, f64::max);
        let a = self.clips.iter().filter(|c| ids.contains(&c.id)).map(|c| c.start).fold(f64::INFINITY, f64::min);
        self.clips.retain(|c| !ids.contains(&c.id));
        if ripple {
            let d = b - a;
            for c in self.clips.iter_mut().filter(|c| c.start >= b - EPS) {
                c.start = (c.start - d).max(0.0);
            }
        }
        self.push_undo(pre);
        true
    }

    // ---------------------------------------------------------------- drag edits
    /// Re-derives the clip list from `pre` with the drag applied (no drift, no accumulated error).
    /// Returns true if anything differs from `pre`. Does NOT touch undo.
    pub fn apply_drag(&mut self, pre: &[Clip], id: u32, op: DragOp) -> bool {
        self.clips = pre.to_vec();
        let Some(o) = pre.iter().find(|c| c.id == id).cloned() else { return false };
        let ids = self.group_ids(id);

        match op {
            DragOp::Move { new_start } => {
                let min_start = pre.iter().filter(|c| ids.contains(&c.id)).map(|c| c.start).fold(f64::INFINITY, f64::min);
                let d = (new_start - o.start).max(-min_start);
                for c in self.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                    c.start += d;
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
            DragOp::TrimL { new_start } => {
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
                    for c in self.clips.iter_mut().filter(|c| !ids.contains(&c.id) && c.start >= from) {
                        c.start = (c.start + d).max(0.0);
                    }
                }
            }
        }
        self.clips
            .iter()
            .zip(pre)
            .any(|(a, b)| (a.start - b.start).abs() > EPS || (a.dur - b.dur).abs() > EPS || (a.src_off - b.src_off).abs() > EPS)
    }

    /// After dropping a moved clip: whatever it now covers on the same lane is trimmed, split or removed.
    pub fn resolve_overwrite(&mut self, id: u32) {
        let ids = self.group_ids(id);
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
    fn trim_right_cannot_exceed_source() {
        let (mut p, _) = media_project();
        let pre = p.clips.clone();
        let id = pre[0].id;
        p.apply_drag(&pre, id, DragOp::TrimR { new_end: 99.0, ripple: false });
        assert!((p.clips[0].end() - 10.0).abs() < 1e-9);
        p.apply_drag(&pre, id, DragOp::TrimR { new_end: 6.0, ripple: false });
        assert!((p.clips[0].end() - 6.0).abs() < 1e-9);
        assert!((p.clips[1].end() - 6.0).abs() < 1e-9); // partner follows
    }

    #[test]
    fn save_load_round_trip() {
        let (p, _) = media_project();
        let q = Project::from_text(&p.to_text()).expect("parse");
        assert_eq!(q.clips.len(), p.clips.len());
        assert_eq!(q.media.len(), 1);
        assert!((q.end() - p.end()).abs() < 1e-9);
        assert_eq!(q.clips[0].link, q.clips[1].link);
    }

    #[test]
    fn ripple_delete_closes_the_gap() {
        let (mut p, m) = media_project();
        assert!(p.append_media(m)); // second copy at 10..20
        let first = p.clips[0].id;
        assert!(p.delete(first, true));
        assert!((p.end() - 10.0).abs() < 1e-9);
        assert!(p.clips.iter().all(|c| c.start.abs() < 1e-9));
    }
}
