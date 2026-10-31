//! Playback engine: timeline time -> (media file, source time).
//!
//!   * keeps a small pool of open decoders (LRU)
//!   * switches the active decoder when the playhead crosses a clip boundary
//!   * contiguous clips of the same file (e.g. after a blade cut) keep the SAME decoder running,
//!     so a cut inside one file is perfectly seamless
//!   * the next *visible* clip is warmed in two phases so no single UI frame pays for everything:
//!       1. `OPEN_LEAD` seconds ahead: the decoder is opened (file probe, threads, audio device)
//!       2. `PREROLL` seconds ahead: the decoder is seeked and its audio buffer filled while paused
//!     "next visible clip" includes a lower-track clip that is revealed when an upper-track clip
//!     ends, not just a clip that starts later
//!   * a paused edit / scrub jitter that does not move the picture does not re-seek the decoder
//!   * pool trimming (dropping a decoder can block on the audio device) is deferred until
//!     playback stops, and failed opens are retried instead of being remembered forever
//!   * exposes the active decoder's audio clock, mapped back to timeline time

use crate::media::{Frame, Player};
use crate::project::Project;
use eframe::egui::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Decoders kept open while idle. Trimmed back to this whenever playback is stopped.
const POOL_SOFT: usize = 4;
/// Decoders allowed while playing; evicting is deferred so the drop cost never lands mid-playback.
const POOL_HARD: usize = 6;
/// Open the next decoder this many timeline seconds before it becomes visible.
const OPEN_LEAD: f64 = 3.0;
/// Seek + fill audio on the next decoder this many seconds before it becomes visible.
const PREROLL: f64 = 1.0;
/// How often (timeline seconds) the look-ahead is re-evaluated during playback.
const PROBE_EVERY: f64 = 0.1;
/// Probe just past a boundary so float noise at `end() == start` cannot pick the wrong clip.
const PROBE_EPS: f64 = 1e-4;
const MAP_EPS: f64 = 1e-3;
/// A file that failed to open is retried (when not playing) after this long.
const RETRY_AFTER: Duration = Duration::from_secs(2);

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

/// The next change of the visible picture: at timeline time `at`, `map` becomes visible.
#[derive(Clone, Copy)]
struct Upcoming {
    at: f64,
    map: Mapping,
}

pub struct Engine {
    players: HashMap<u32, Slot>,
    /// media id -> when the open failed
    failed: HashMap<u32, Instant>,
    active: Option<Mapping>,
    prepared: Option<Mapping>,
    /// decoder opened for the upcoming clip but not yet seeked (protected from eviction)
    warming: Option<u32>,
    upcoming: Option<Upcoming>,
    next_probe: f64,
    /// (media, source time) the active decoder was last positioned at while paused. While this
    /// matches the requested time, a re-seek would only redo work that is already done.
    settled: Option<(u32, f64)>,
    playing: bool,
    tick: u64,
    quality: u32,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            failed: HashMap::new(),
            active: None,
            prepared: None,
            warming: None,
            upcoming: None,
            next_probe: f64::NEG_INFINITY,
            settled: None,
            playing: false,
            tick: 0,
            quality: 2,
        }
    }

    /// Adds an already-open decoder (used right after import, which had to open the file anyway).
    pub fn insert(&mut self, media: u32, mut player: Player) {
        self.failed.remove(&media);
        player.set_quality(self.quality);
        // A replaced decoder has not been positioned yet: anything that assumed otherwise is void.
        if self.settled.map_or(false, |(m, _)| m == media) {
            self.settled = None;
        }
        if self.prepared.map_or(false, |p| p.media == media) {
            self.prepared = None;
        }
        let cap = if self.playing { POOL_HARD } else { POOL_SOFT };
        while self.players.len() >= cap && !self.players.contains_key(&media) {
            if !self.evict_one() {
                break;
            }
        }
        self.players.insert(media, Slot { player, used: self.tick });
    }

    /// Drop a decoder so the next update reopens it from the current preview path (proxy or source).
    pub fn invalidate_media(&mut self, media: u32) {
        if let Some(mut slot) = self.players.remove(&media) {
            slot.player.set_playing(false, None);
        }
        self.failed.remove(&media);
        if self.active.map_or(false, |m| m.media == media) {
            self.active = None;
        }
        if self.prepared.map_or(false, |m| m.media == media) {
            self.prepared = None;
        }
        if self.warming == Some(media) {
            self.warming = None;
        }
        if self.settled.map_or(false, |(m, _)| m == media) {
            self.settled = None;
        }
    }

    /// Preview resolution divisor for every open decoder (1 = full, 2 = 1/2, 4 = 1/4).
    pub fn set_quality(&mut self, div: u32) {
        self.quality = div.max(1);
        for s in self.players.values_mut() {
            s.player.set_quality(self.quality);
        }
        // The shown frame has the old resolution: the next jump must really re-decode.
        self.settled = None;
    }

    fn pinned(&self, id: u32) -> bool {
        self.active.map_or(false, |a| a.media == id)
            || self.prepared.map_or(false, |p| p.media == id)
            || self.warming == Some(id)
    }

    /// Drops the least-recently-used decoder that is not needed. Returns false if none can go.
    fn evict_one(&mut self) -> bool {
        let victim = self
            .players
            .iter()
            .filter(|(id, _)| !self.pinned(**id))
            .min_by_key(|(_, s)| s.used)
            .map(|(id, _)| *id);
        match victim {
            Some(v) => {
                self.players.remove(&v);
                true
            }
            None => false,
        }
    }

    fn ensure(&mut self, ctx: &Context, proj: &Project, media: u32) -> bool {
        if self.players.contains_key(&media) {
            return true;
        }
        // Failed opens are retried, but never during playback (a retry blocks the UI thread).
        let retry = self.failed.get(&media).map(|at| !self.playing && at.elapsed() >= RETRY_AFTER);
        match retry {
            Some(false) => return false,
            Some(true) => {
                self.failed.remove(&media);
            }
            None => {}
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
                self.failed.insert(media, Instant::now());
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

    /// True when the paused active decoder was already positioned at (about) source time `x`
    /// and actually holds a frame, so a re-seek would decode the same picture again.
    fn is_settled_at(&self, media: u32, x: f64) -> bool {
        let Some((m, s)) = self.settled else { return false };
        if m != media {
            return false;
        }
        let Some(slot) = self.players.get(&media) else { return false };
        if slot.player.current.is_none() {
            return false;
        }
        (s - x).abs() <= 0.5 / slot.player.info.fps.max(1.0)
    }

    /// Call once per frame. `jumped` = the playhead was moved by the user (or an edit happened),
    /// so decoders must be repositioned instead of just following the clock.
    pub fn update(
        &mut self,
        ctx: &Context,
        proj: &Project,
        t: f64,
        playing: bool,
        jumped: bool,
        step_direction: Option<i8>,
        muted: bool,
        gain: f32,
    ) {
        self.tick += 1;
        let play_changed = playing != self.playing;
        self.playing = playing;
        if playing {
            self.settled = None; // the decoder is moving on its own now
        }

        let cur = proj
            .video_clip_at(t)
            .and_then(|c| c.media.map(|m| Mapping { media: m, map: c.src_off - c.start }));

        match cur {
            None => {
                // gap, placeholder clip, or end of project
                if let Some(a) = self.active.take() {
                    self.pause(a.media);
                }
                self.settled = None;
            }
            Some(c) => {
                let x = t + c.map;
                let same = self.active.map_or(false, |a| a.matches(&c));
                if same && !jumped {
                    if let Some(direction) = step_direction {
                        if let Some(s) = self.players.get_mut(&c.media) {
                            s.player.step(x, direction);
                        }
                        self.settled = Some((c.media, x));
                    }
                    if play_changed {
                        // resume exactly at the playhead (audio re-aligns), or pause
                        self.set_playing(c.media, playing, Some(x));
                    }
                } else if same && !playing && !play_changed && step_direction.is_none() && self.is_settled_at(c.media, x) {
                    // An edit or scrub jitter that left the picture where it was: the decoder is
                    // already (or is already being) positioned here. Skipping the seek avoids a
                    // keyframe re-decode, a stale-frame flush and a texture re-upload.
                } else if self.ensure(ctx, proj, c.media) {
                    if let Some(a) = self.active {
                        if a.media != c.media {
                            self.pause(a.media);
                        }
                    }
                    let prepared = !jumped && self.prepared.map_or(false, |p| p.matches(&c));
                    if prepared {
                        self.settled = None;
                    } else {
                        if let Some(s) = self.players.get_mut(&c.media) {
                            if let Some(direction) = step_direction {
                                s.player.step(x, direction);
                            } else {
                                s.player.seek(x);
                            }
                        }
                        self.settled = if playing { None } else { Some((c.media, x)) };
                    }
                    self.set_playing(c.media, playing, None);
                    self.prepared = None;
                    self.warming = None;
                    self.active = Some(c);
                } else if let Some(a) = self.active.take() {
                    self.pause(a.media); // unreadable file behaves like a gap
                    self.settled = None;
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
            self.preroll(ctx, proj, t, jumped);
        } else {
            self.upcoming = None;
            self.warming = None;
            self.next_probe = f64::NEG_INFINITY;
            // Deferred trim: one decoder per update so the drop cost is spread out.
            if self.players.len() > POOL_SOFT {
                self.evict_one();
            }
        }
    }

    /// Finds the next moment the visible picture changes and what becomes visible then.
    /// Boundaries that change nothing (a clip starting hidden under an upper track, or a cut
    /// inside one continuous source) are skipped. Looks at most `OPEN_LEAD` seconds ahead.
    fn lookahead(proj: &Project, t: f64) -> Option<Upcoming> {
        let mut seen = proj
            .video_clip_at(t)
            .and_then(|c| c.media.map(|m| Mapping { media: m, map: c.src_off - c.start }));
        let mut from = t;
        for _ in 0..8 {
            let cur_end = proj.video_clip_at(from).map(|c| c.end());
            let next_start = proj.next_video_clip_after(from).map(|c| c.start);
            let at = match (cur_end, next_start) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => return None,
            };
            if at - t > OPEN_LEAD {
                return None;
            }
            let to = proj
                .video_clip_at(at + PROBE_EPS)
                .and_then(|c| c.media.map(|m| Mapping { media: m, map: c.src_off - c.start }));
            from = at + PROBE_EPS;
            match (seen, to) {
                (_, None) => seen = None, // gap: keep looking for what follows it
                (Some(s), Some(n)) if s.matches(&n) => seen = Some(n), // nothing visibly changes
                (_, Some(n)) => return Some(Upcoming { at, map: n }),
            }
        }
        None
    }

    fn preroll(&mut self, ctx: &Context, proj: &Project, t: f64, jumped: bool) {
        if jumped || t >= self.next_probe {
            self.upcoming = Self::lookahead(proj, t);
            self.next_probe = t + PROBE_EVERY;
        }
        let Some(up) = self.upcoming else {
            self.warming = None;
            return;
        };
        let lead = up.at - t;
        if lead < 0.0 || lead > OPEN_LEAD {
            return;
        }
        let m = up.map.media;
        if self.active.map_or(false, |a| a.media == m) {
            return; // same decoder: nothing to warm up
        }
        if self.prepared.map_or(false, |p| p.matches(&up.map)) {
            return;
        }

        // Phase 1: open the decoder well before it is needed.
        if !self.ensure(ctx, proj, m) {
            return;
        }
        self.warming = Some(m);
        if lead > PREROLL {
            return;
        }

        // Phase 2: position it and fill its audio while paused, so the switch is gapless.
        let src = up.at + up.map.map;
        if let Some(s) = self.players.get_mut(&m) {
            s.player.seek(src);
            s.player.set_playing(false, None);
            s.used = self.tick;
        }
        self.prepared = Some(up.map);
        self.warming = None;
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
