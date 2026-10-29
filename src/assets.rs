//! Background analysis of imported media: keyframe thumbnails + audio waveform peaks.
//!
//! Each import spawns up to two low-priority threads (video thumbs, audio peaks) that open the
//! file on their own and stream results to the UI thread over a channel. The UI thread uploads
//! thumbnails to the GPU a few per frame (never a hitch) and the timeline draws whatever has
//! arrived so far, so clips fill in progressively while you keep working.

use crate::media::{AudioWorker, VideoDecodeStep, VideoWorker};
use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

const THUMB_W: u32 = 192;
const MAX_THUMBS: f64 = 150.0;
const PEAK_RATE: f64 = 50.0; // waveform bins per second
const PEAK_AUDIO_HZ: u32 = 8000; // analysis sample rate (cheap, plenty for a waveform)
const PEAK_BIN: usize = (PEAK_AUDIO_HZ as f64 / PEAK_RATE) as usize;
const UPLOADS_PER_FRAME: usize = 6;

enum Msg {
    Thumb { media: u32, pts: f64, w: usize, h: usize, rgba: Vec<u8> },
    Peaks { media: u32, data: Vec<f32> },
}

#[derive(Default)]
struct Entry {
    /// (source time, texture, aspect) sorted by time
    thumbs: Vec<(f64, TextureHandle, f32)>,
    peaks: Vec<f32>,
}

pub struct Assets {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    map: HashMap<u32, Entry>,
}

impl Assets {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, map: HashMap::new() }
    }

    /// Start analysing a media file (no-op if already started).
    pub fn spawn(&mut self, ctx: &Context, media: u32, path: &Path, duration: f64, has_audio: bool) {
        if self.map.contains_key(&media) {
            return;
        }
        self.map.insert(media, Entry::default());
        let (p, tx, c) = (path.to_path_buf(), self.tx.clone(), ctx.clone());
        let _ = std::thread::Builder::new()
            .name("thumbs".into())
            .spawn(move || thumbs_job(p, media, duration, tx, c));
        if has_audio {
            let (p, tx, c) = (path.to_path_buf(), self.tx.clone(), ctx.clone());
            let _ = std::thread::Builder::new().name("peaks".into()).spawn(move || peaks_job(p, media, tx, c));
        }
    }

    /// Drain finished work. Call once per frame; uploads at most a few textures.
    pub fn pump(&mut self, ctx: &Context) {
        let mut uploads = 0;
        while uploads < UPLOADS_PER_FRAME {
            let Ok(msg) = self.rx.try_recv() else { return };
            match msg {
                Msg::Thumb { media, pts, w, h, rgba } => {
                    let Some(e) = self.map.get_mut(&media) else { continue }; // media was closed
                    let img = ColorImage::from_rgba_premultiplied([w, h], &rgba);
                    let tex = ctx.load_texture(format!("th{media}_{}", e.thumbs.len()), img, TextureOptions::LINEAR);
                    let pos = e.thumbs.partition_point(|(p, _, _)| *p < pts);
                    e.thumbs.insert(pos, (pts, tex, w as f32 / h.max(1) as f32));
                    uploads += 1;
                }
                Msg::Peaks { media, data } => {
                    if let Some(e) = self.map.get_mut(&media) {
                        e.peaks.extend(data);
                    }
                }
            }
        }
        ctx.request_repaint(); // more queued: continue next frame
    }

    /// Thumbnail nearest to source time `t`.
    pub fn thumb_at(&self, media: u32, t: f64) -> Option<(&TextureHandle, f32)> {
        let e = self.map.get(&media)?;
        if e.thumbs.is_empty() {
            return None;
        }
        let i = e.thumbs.partition_point(|(p, _, _)| *p < t);
        let best = match (i.checked_sub(1), e.thumbs.get(i)) {
            (Some(a), Some(b)) => {
                if (t - e.thumbs[a].0).abs() <= (b.0 - t).abs() {
                    &e.thumbs[a]
                } else {
                    b
                }
            }
            (Some(a), None) => &e.thumbs[a],
            (None, Some(b)) => b,
            (None, None) => return None,
        };
        Some((&best.1, best.2))
    }

    pub fn aspect(&self, media: u32) -> Option<f32> {
        self.map.get(&media)?.thumbs.first().map(|t| t.2)
    }

    /// Peak amplitude (0..1) over source time [t0, t1]. None until that part has been analysed.
    pub fn peak(&self, media: u32, t0: f64, t1: f64) -> Option<f32> {
        let e = self.map.get(&media)?;
        let a = (t0.max(0.0) * PEAK_RATE) as usize;
        if a >= e.peaks.len() {
            return None;
        }
        let b = ((t1.max(0.0) * PEAK_RATE).ceil() as usize).clamp(a + 1, e.peaks.len());
        e.peaks[a..b].iter().copied().reduce(f32::max)
    }
}

fn thumbs_job(path: PathBuf, media: u32, duration: f64, tx: Sender<Msg>, ctx: Context) {
    let Ok((mut w, info)) = VideoWorker::open_sized(&path, THUMB_W) else { return };
    let dur = if duration > 0.5 { duration } else { info.duration.max(60.0) };
    let step = (dur / MAX_THUMBS).max(1.0);
    let (mut t, mut last) = (0.0f64, f64::NEG_INFINITY);
    while t < dur {
        w.seek(t, 0);
        // Seek to the frame nearest t. Pre-target frames are discarded before RGBA conversion,
        // keeping thumbnail extraction fast even for long-GOP source footage.
        let f = loop {
            match w.next_frame(Some((t, 0.5 / info.fps.max(1.0)))) {
                VideoDecodeStep::Frame(f) => break Some(f),
                VideoDecodeStep::Skipped => continue,
                VideoDecodeStep::End => break None,
            }
        };
        if let Some(f) = f {
            if f.pts - last > 0.01 {
                last = f.pts;
                if tx.send(Msg::Thumb { media, pts: f.pts, w: f.w, h: f.h, rgba: f.rgba }).is_err() {
                    return; // app closed
                }
                ctx.request_repaint();
            }
        }
        t += step;
    }
}

fn peaks_job(path: PathBuf, media: u32, tx: Sender<Msg>, ctx: Context) {
    let Ok(Some(mut w)) = AudioWorker::open(&path, PEAK_AUDIO_HZ) else { return };
    let mut buf: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let (mut acc, mut n) = (0.0f32, 0usize);
    loop {
        buf.clear();
        if !w.next_chunk(&mut buf) {
            break;
        }
        for fr in buf.chunks_exact(2) {
            acc = acc.max(fr[0].abs().max(fr[1].abs()));
            n += 1;
            if n == PEAK_BIN {
                out.push(acc);
                acc = 0.0;
                n = 0;
            }
        }
        if out.len() >= 500 {
            if tx.send(Msg::Peaks { media, data: std::mem::take(&mut out) }).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
    if n > 0 {
        out.push(acc);
    }
    if !out.is_empty() {
        let _ = tx.send(Msg::Peaks { media, data: out });
        ctx.request_repaint();
    }
}
