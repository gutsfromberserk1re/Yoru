//! Playback engine: timeline time -> (media file, source time).
//!
//!   * keeps a small pool of open decoders (LRU, `POOL_MAX`)
//!   * switches the active decoder when the playhead crosses a clip boundary
//!   * contiguous clips of the same file (e.g. after a blade cut) keep the SAME decoder running,
//!     so a cut inside one file is perfectly seamless
//!   * when the next clip uses a different file, it is pre-rolled `PREROLL` seconds early
//!     (decoder seeked + audio buffer filled while paused) so the switch is gapless
//!   * exposes the active decoder's audio clock, mapped back to timeline time

use crate::media::{Frame, Player};
use crate::project::Project;
use eframe::egui::Context;
use std::collections::{HashMap, HashSet};

const POOL_MAX: usize = 4;
const PREROLL: f64 = 1.0;
const MAP_EPS: f64 = 1e-3;

struct Slot {
    player: Player,
    used: u64,
}

/// source_time = timeline_time + map
#[derive(Clone, Copy)]
struct Mapping {
    media: u32,
    map: f64,
}

impl Mapping {
    fn matches(&self, other: &Mapping) -> bool {
        self.media == other.media && (self.map - other.map).abs() < MAP_EPS
    }
}

pub struct Engine {
    players: HashMap<u32, Slot>,
    failed: HashSet<u32>,
    active: Option<Mapping>,
    prepared: Option<Mapping>,
    playing: bool,
    tick: u64,
    quality: u32,
}

impl Engine {
    pub fn new() -> Self {
        Self { players: HashMap::new(), failed: HashSet::new(), active: None, prepared: None, playing: false, tick: 0, quality: 2 }
    }

    /// Adds an already-open decoder (used right after import, which had to open the file anyway).
    pub fn insert(&mut self, media: u32, player: Player) {
        self.failed.remove(&media);
        let mut player = player;
        player.set_quality(self.quality);
        while self.players.len() >= POOL_MAX && !self.players.contains_key(&media) {
            let victim = self
                .players
                .iter()
                .filter(|(id, _)| !self.pinned(**id))
                .min_by_key(|(_, s)| s.used)
                .map(|(id, _)| *id);
            match victim {
                Some(v) => {
                    self.players.remove(&v);
                }
                None => break,
            }
        }
        self.players.insert(media, Slot { player, used: self.tick });
    }

    /// Drop a decoder so the next update reopens it from the current preview path (proxy or source).
    pub fn invalidate_media(&mut self, media: u32) {
        if let Some(slot) = self.players.remove(&media) {
            slot.player.set_playing(false, None);
        }
        self.failed.remove(&media);
        if self.active.map_or(false, |m| m.media == media) { self.active = None; }
        if self.prepared.map_or(false, |m| m.media == media) { self.prepared = None; }
    }

    /// Preview resolution divisor for every open decoder (1 = full, 2 = 1/2, 4 = 1/4).
    pub fn set_quality(&mut self, div: u32) {
        self.quality = div.max(1);
        for s in self.players.values_mut() {
            s.player.set_quality(self.quality);
        }
    }

    fn pinned(&self, id: u32) -> bool {
        self.active.map_or(false, |a| a.media == id) || self.prepared.map_or(false, |p| p.media == id)
    }

    fn ensure(&mut self, ctx: &Context, proj: &Project, media: u32) -> bool {
        if self.players.contains_key(&media) {
            return true;
        }
        if self.failed.contains(&media) {
            return false;
        }
        let Some(m) = proj.media(media) else { return false };
        let preview_path = m.proxy_path.as_ref().filter(|path| path.is_file()).unwrap_or(&m.path);
        match Player::open(preview_path, ctx.clone()) {
            Ok(p) => {
                self.insert(media, p);
                true
            }
            Err(e) => {
                eprintln!("cannot open {}: {e}", m.path.display());
                self.failed.insert(media);
                false
            }
        }
    }

    fn set_playing(&mut self, media: u32, on: bool, resync: Option<f64>) {
        if let Some(s) = self.players.get_mut(&media) {
            s.player.set_playing(on, resync);
        }
    }

    fn pause(&mut self, media: u32) {
        self.set_playing(media, false, None);
    }

    /// Call once per frame. `jumped` = the playhead was moved by the user (or an edit happened),
    /// so decoders must be repositioned instead of just following the clock.
    pub fn update(&mut self, ctx: &Context, proj: &Project, t: f64, playing: bool, jumped: bool, muted: bool, gain: f32) {
        self.tick += 1;
        let play_changed = playing != self.playing;
        self.playing = playing;

        let cur = proj
            .video_clip_at(t)
            .and_then(|c| c.media.map(|m| Mapping { media: m, map: c.src_off - c.start }));

        match cur {
            None => {
                // gap, placeholder clip, or end of project
                if let Some(a) = self.active.take() {
                    self.pause(a.media);
                }
            }
            Some(c) => {
                let same = self.active.map_or(false, |a| a.matches(&c));
                if same && !jumped {
                    if play_changed {
                        // resume exactly at the playhead (audio re-aligns), or pause
                        self.set_playing(c.media, playing, Some(t + c.map));
                    }
                } else if self.ensure(ctx, proj, c.media) {
                    if let Some(a) = self.active {
                        if a.media != c.media {
                            self.pause(a.media);
                        }
                    }
                    let prepared = !jumped && self.prepared.map_or(false, |p| p.matches(&c));
                    if !prepared {
                        if let Some(s) = self.players.get_mut(&c.media) {
                            s.player.seek(t + c.map);
                        }
                    }
                    self.set_playing(c.media, playing, None);
                    self.prepared = None;
                    self.active = Some(c);
                } else if let Some(a) = self.active.take() {
                    self.pause(a.media); // unreadable file behaves like a gap
                }
            }
        }

        if let Some(a) = self.active {
            if let Some(s) = self.players.get_mut(&a.media) {
                s.used = self.tick;
                s.player.set_muted(muted);
                s.player.set_volume(gain);
            }
        }
        if playing {
            self.preroll(ctx, proj, t);
        }
    }

    fn preroll(&mut self, ctx: &Context, proj: &Project, t: f64) {
        let Some(n) = proj.next_video_clip_after(t) else { return };
        if n.start - t > PREROLL {
            return;
        }
        let Some(m) = n.media else { return };
        if self.active.map_or(false, |a| a.media == m) {
            return; // same decoder: nothing to warm up
        }
        let next = Mapping { media: m, map: n.src_off - n.start };
        if self.prepared.map_or(false, |p| p.matches(&next)) {
            return;
        }
        let src = n.src_off;
        if !self.ensure(ctx, proj, m) {
            return;
        }
        if let Some(s) = self.players.get_mut(&m) {
            s.player.seek(src);
            s.player.set_playing(false, None);
            s.used = self.tick;
        }
        self.prepared = Some(next);
    }

    /// The most recently decoded frame of the active clip (for re-rendering when effects change).
    pub fn current(&self) -> Option<&Frame> {
        let a = self.active?;
        self.players.get(&a.media)?.player.current.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Duration of one frame in the active source, used for accurate paused frame stepping.
    pub fn frame_step(&self) -> f64 {
        self.active
            .and_then(|a| self.players.get(&a.media))
            .map(|s| 1.0 / s.player.info.fps.max(1.0))
            .unwrap_or(1.0 / 30.0)
    }

    /// Newly decoded frame for timeline time `t`, if the picture changed.
    pub fn poll(&mut self, t: f64) -> Option<&Frame> {
        let a = self.active?;
        let s = self.players.get_mut(&a.media)?;
        if s.player.poll(t + a.map) {
            s.player.current.as_ref()
        } else {
            None
        }
    }

    /// Audio master clock in timeline seconds (None without audio -> caller uses the wall clock).
    pub fn clock(&self) -> Option<f64> {
        let a = self.active?;
        self.players.get(&a.media)?.player.clock().map(|c| c - a.map)
    }

    pub fn peaks(&self) -> Option<(f32, f32)> {
        let a = self.active?;
        self.players.get(&a.media)?.player.peaks()
    }
}
