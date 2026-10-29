//! Media engine v1: FFmpeg video decode + audio playback with an audio master clock.
//!
//! Threads
//!   * UI thread     - sends tiny commands, polls finished frames, reads the audio clock.
//!   * video-decode  - decodes ahead of the playhead into a bounded queue (back-pressured).
//!   * audio-decode  - decodes + resamples to the device rate/stereo f32 into a shared FIFO.
//!   * cpal callback - drains the FIFO, publishes the clock and meter peaks. Never allocates.
//!
//! Sync model: the sound card is the master clock. The UI reads `Player::clock()` and picks the
//! video frame whose pts <= clock. Every seek bumps a generation/epoch so stale frames and stale
//! audio that are still in flight are discarded instead of played.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use eframe::egui::Context;
use ffmpeg::{
    codec, format,
    format::{sample::Type as SampleType, Sample as SampleFmt},
    media::Type,
    software::{resampling, scaling},
    util::frame::{Audio as AFrame, Video as VFrame},
    ChannelLayout, Packet,
};
use ffmpeg_next as ffmpeg;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_PREVIEW_W: u32 = 1920; // 'Full' playback resolution cap
const VIDEO_QUEUE: usize = 8;

// ------------------------------------------------------------------ public data
pub struct Frame {
    pub pts: f64,
    pub gen: u64,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
pub struct Info {
    pub w: u32,
    pub h: u32,
    pub fps: f64,
    pub duration: f64,
}

fn rational(r: ffmpeg::Rational) -> f64 {
    f64::from(r.numerator()) / f64::from(r.denominator().max(1))
}

// ================================================================== VIDEO WORKER
enum Cmd {
    Seek(f64, u64),
    Play(bool),
    /// Preview resolution divisor: 1 = full, 2 = half, 4 = quarter.
    Quality(u32),
}

pub struct VideoWorker {
    ictx: format::context::Input,
    dec: ffmpeg::decoder::Video,
    scaler: scaling::Context,
    idx: usize,
    tb: f64,
    raw: VFrame,
    rgba: VFrame,
    flushed: bool,
    gen: u64,
    w: u32,
    h: u32,
    src_w: u32,
    src_h: u32,
    max_w: u32,
}

impl VideoWorker {
    pub fn open(path: &Path) -> Result<(Self, Info), ffmpeg::Error> {
        Self::open_sized(path, MAX_PREVIEW_W)
    }

    /// `max_w`: output width cap (preview = 1280, thumbnails = small).
    pub fn open_sized(path: &Path, max_w: u32) -> Result<(Self, Info), ffmpeg::Error> {
        let ictx = format::input(path)?;
        let (idx, tb, params, fps) = {
            let s = ictx.streams().best(Type::Video).ok_or(ffmpeg::Error::StreamNotFound)?;
            let fps = rational(s.avg_frame_rate());
            (s.index(), rational(s.time_base()), s.parameters(), if fps.is_finite() && fps > 1.0 { fps } else { 30.0 })
        };
        let mut cctx = codec::context::Context::from_parameters(params)?;
        cctx.set_threading(ffmpeg::threading::Config::kind(ffmpeg::threading::Type::Frame));
        let dec = cctx.decoder().video()?;

        let (sw, sh) = (dec.width().max(2), dec.height().max(2));
        let w = (sw.min(max_w) & !1).max(2);
        let h = ((((sh as u64 * w as u64) / sw as u64) as u32) & !1).max(2);
        let scaler = scaling::Context::get(dec.format(), sw, sh, format::Pixel::RGBA, w, h, scaling::Flags::BILINEAR)?;
        let dur = ictx.duration();
        let info = Info { w, h, fps, duration: if dur > 0 { dur as f64 / 1_000_000.0 } else { 0.0 } };
        let me = Self {
            ictx,
            dec,
            scaler,
            idx,
            tb,
            raw: VFrame::empty(),
            rgba: VFrame::new(format::Pixel::RGBA, w, h),
            flushed: false,
            gen: 0,
            w,
            h,
            src_w: sw,
            src_h: sh,
            max_w,
        };
        Ok((me, info))
    }

    /// Changes the output size on the fly (rebuilds the scaler). Frames already queued keep
    /// their old size; the caller re-seeks so new ones arrive at the new size.
    pub fn rescale(&mut self, div: u32) {
        let div = div.clamp(1, 8);
        let full = self.src_w.min(self.max_w);
        let w = ((full / div) & !1).max(2);
        let h = ((((self.src_h as u64 * w as u64) / self.src_w.max(1) as u64) as u32) & !1).max(2);
        if (w, h) == (self.w, self.h) {
            return;
        }
        if let Ok(sc) = scaling::Context::get(
            self.dec.format(),
            self.src_w,
            self.src_h,
            format::Pixel::RGBA,
            w,
            h,
            scaling::Flags::BILINEAR,
        ) {
            self.scaler = sc;
            self.rgba = VFrame::new(format::Pixel::RGBA, w, h);
            self.w = w;
            self.h = h;
        }
    }

    pub fn seek(&mut self, t: f64, gen: u64) {
        let ts = (t.max(0.0) * 1_000_000.0) as i64;
        let _ = self.ictx.seek(ts, ..ts); // lands on the previous keyframe
        self.dec.flush();
        self.flushed = false;
        self.gen = gen;
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if self.dec.receive_frame(&mut self.raw).is_ok() {
                if let Some(f) = self.convert() {
                    return Some(f);
                }
                continue;
            }
            if self.flushed {
                return None;
            }
            let mut pkt = Packet::empty();
            match pkt.read(&mut self.ictx) {
                Ok(()) => {
                    if pkt.stream() == self.idx {
                        let _ = self.dec.send_packet(&pkt);
                    }
                }
                Err(_) => {
                    let _ = self.dec.send_eof();
                    self.flushed = true;
                }
            }
        }
    }

    fn convert(&mut self) -> Option<Frame> {
        self.scaler.run(&self.raw, &mut self.rgba).ok()?;
        let pts = self.raw.timestamp().or(self.raw.pts()).unwrap_or(0) as f64 * self.tb;
        let (w, h) = (self.w as usize, self.h as usize);
        let stride = self.rgba.stride(0);
        let data = self.rgba.data(0);
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            out.extend_from_slice(&data[y * stride..y * stride + w * 4]); // strip row padding
        }
        Some(Frame { pts, gen: self.gen, w, h, rgba: out })
    }
}

fn video_run(mut w: VideoWorker, rx: Receiver<Cmd>, tx: SyncSender<Frame>, ctx: Context, fps: f64) {
    let half = 0.5 / fps;
    let mut playing = false;
    let mut target: Option<f64> = Some(0.0); // decode-and-discard until this time
    let mut out: Option<Frame> = None; // finished frame waiting for queue space
    let mut eof = false;

    loop {
        // 1. commands - block only when there is genuinely nothing to do
        let busy = playing || target.is_some() || out.is_some();
        let mut seek = None;
        let mut quality = None;
        let mut apply = |c: Cmd, playing: &mut bool| match c {
            Cmd::Play(p) => *playing = p,
            Cmd::Seek(t, g) => seek = Some((t, g)), // coalesce: only the newest seek matters
            Cmd::Quality(d) => quality = Some(d),
        };
        if busy {
            loop {
                match rx.try_recv() {
                    Ok(c) => apply(c, &mut playing),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        } else {
            match rx.recv() {
                Ok(c) => apply(c, &mut playing),
                Err(_) => return,
            }
        }
        if let Some(d) = quality {
            w.rescale(d);
        }
        if let Some((t, g)) = seek {
            w.seek(t, g);
            target = Some(t);
            out = None;
            eof = false;
        }

        // 2. hand a finished frame to the UI (back-pressure when the queue is full)
        if let Some(f) = out.take() {
            match tx.try_send(f) {
                Ok(()) => ctx.request_repaint(),
                Err(TrySendError::Full(f)) => {
                    out = Some(f);
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
            continue;
        }

        // 3. decode one more frame if somebody wants it
        if eof {
            target = None;
            if playing {
                std::thread::sleep(Duration::from_millis(5));
            }
            continue;
        }
        if !playing && target.is_none() {
            continue;
        }
        match w.next_frame() {
            Some(f) => {
                if let Some(t) = target {
                    if f.pts + half < t {
                        continue;
                    }
                    target = None;
                }
                out = Some(f);
            }
            None => eof = true,
        }
    }
}

// ================================================================== AUDIO SHARED STATE
struct AudioBuf {
    q: VecDeque<f32>, // interleaved stereo
    epoch: u64,       // bumped on every seek; stale pushes are rejected
    next_t: f64,      // timeline time of the next sample the device will consume
    eof: bool,
}

/// Extrapolating clock: the callback publishes (t, stamp); the UI adds elapsed wall time.
struct Clock {
    base: Instant,
    t: AtomicU64,
    ns: AtomicU64,
    run: AtomicBool,
}

impl Clock {
    fn set(&self, t: f64, run: bool) {
        self.t.store(t.to_bits(), Relaxed);
        self.ns.store(self.base.elapsed().as_nanos() as u64, Relaxed);
        self.run.store(run, Relaxed);
    }
    fn now(&self) -> f64 {
        let t = f64::from_bits(self.t.load(Relaxed));
        if !self.run.load(Relaxed) {
            return t; // stalled / paused: hold still so video holds too
        }
        let dt = (self.base.elapsed().as_nanos() as u64).saturating_sub(self.ns.load(Relaxed));
        t + (dt as f64 * 1e-9).min(0.05) // cap in case the device stops calling us
    }
}

struct AudioShared {
    buf: Mutex<AudioBuf>,
    clock: Clock,
    playing: AtomicBool,
    muted: AtomicBool,
    gain: AtomicU32, // linear playback gain, updated without locking the audio FIFO
    peak_l: AtomicU32, // positive f32 bit patterns order like the floats -> fetch_max = peak hold
    peak_r: AtomicU32,
}

impl AudioShared {
    fn new() -> Self {
        Self {
            buf: Mutex::new(AudioBuf { q: VecDeque::with_capacity(1 << 17), epoch: 0, next_t: 0.0, eof: false }),
            clock: Clock { base: Instant::now(), t: AtomicU64::new(0), ns: AtomicU64::new(0), run: AtomicBool::new(false) },
            playing: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            peak_l: AtomicU32::new(0),
            peak_r: AtomicU32::new(0),
        }
    }
    fn lock(&self) -> MutexGuard<'_, AudioBuf> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()) // critical sections are microseconds
    }
    fn push(&self, epoch: u64, data: &[f32]) -> bool {
        let mut b = self.lock();
        if b.epoch != epoch {
            return false;
        }
        b.q.extend(data);
        true
    }
    fn queued(&self) -> usize {
        self.lock().q.len()
    }
    fn mark_eof(&self, epoch: u64) {
        let mut b = self.lock();
        if b.epoch == epoch {
            b.eof = true;
        }
    }
    /// Drop everything queued and restart the audio timeline at `t`. Returns the new epoch.
    fn reset(&self, t: f64) -> u64 {
        let mut b = self.lock();
        b.q.clear();
        b.epoch += 1;
        b.next_t = t;
        b.eof = false;
        self.clock.set(t, false);
        b.epoch
    }
}

// ================================================================== AUDIO DECODE WORKER
pub struct AudioWorker {
    ictx: format::context::Input,
    dec: ffmpeg::decoder::Audio,
    rs: resampling::Context,
    idx: usize,
    tb: f64,
    rate: u32,
    raw: AFrame,
    out: AFrame,
    flushed: bool,
    skip_until: Option<f64>, // after a seek: trim samples that precede the target
}

fn make_resampler(dec: &ffmpeg::decoder::Audio, rate: u32) -> Result<resampling::Context, ffmpeg::Error> {
    resampling::Context::get(
        dec.format(),
        dec.channel_layout(),
        dec.rate(),
        SampleFmt::F32(SampleType::Packed),
        ChannelLayout::STEREO,
        rate,
    )
}

impl AudioWorker {
    /// Ok(None) = the file has no audio stream.
    pub fn open(path: &Path, rate: u32) -> Result<Option<Self>, ffmpeg::Error> {
        let ictx = format::input(path)?;
        let found = ictx.streams().best(Type::Audio).map(|s| (s.index(), rational(s.time_base()), s.parameters()));
        let Some((idx, tb, params)) = found else { return Ok(None) };
        let mut dec = codec::context::Context::from_parameters(params)?.decoder().audio()?;
        if dec.channel_layout().is_empty() {
            dec.set_channel_layout(ChannelLayout::default(dec.channels() as i32));
        }
        let rs = make_resampler(&dec, rate)?;
        Ok(Some(Self {
            ictx,
            dec,
            rs,
            idx,
            tb,
            rate,
            raw: AFrame::empty(),
            out: AFrame::empty(),
            flushed: false,
            skip_until: None,
        }))
    }

    pub fn seek(&mut self, t: f64) {
        let ts = (t.max(0.0) * 1_000_000.0) as i64;
        let _ = self.ictx.seek(ts, ..ts);
        self.dec.flush();
        if let Ok(rs) = make_resampler(&self.dec, self.rate) {
            self.rs = rs; // drop resampler history from before the jump
        }
        self.flushed = false;
        self.skip_until = Some(t);
    }

    /// Decode + resample the next chunk into `buf`. False at end of stream.
    pub fn next_chunk(&mut self, buf: &mut Vec<f32>) -> bool {
        loop {
            if self.dec.receive_frame(&mut self.raw).is_ok() {
                self.resample(buf);
                if !buf.is_empty() {
                    return true;
                }
                continue;
            }
            if self.flushed {
                return false;
            }
            let mut pkt = Packet::empty();
            match pkt.read(&mut self.ictx) {
                Ok(()) => {
                    if pkt.stream() == self.idx {
                        let _ = self.dec.send_packet(&pkt);
                    }
                }
                Err(_) => {
                    let _ = self.dec.send_eof();
                    self.flushed = true;
                }
            }
        }
    }

    fn resample(&mut self, buf: &mut Vec<f32>) {
        if self.rs.run(&self.raw, &mut self.out).is_err() {
            return;
        }
        let n = self.out.samples();
        if n == 0 {
            return;
        }
        let rate = self.rate as f64;
        let mut start = 0usize;
        if let Some(t0) = self.skip_until {
            let p = self.raw.timestamp().map_or(t0, |p| p as f64 * self.tb);
            if p + n as f64 / rate <= t0 {
                return; // whole frame is before the seek target
            }
            if p < t0 {
                start = (((t0 - p) * rate) as usize).min(n);
            }
            self.skip_until = None;
        }
        let data = self.out.plane::<(f32, f32)>(0);
        buf.reserve((n - start) * 2);
        for &(l, r) in &data[start..n] {
            buf.push(l);
            buf.push(r);
        }
    }
}

fn audio_run(mut w: AudioWorker, rx: Receiver<(f64, u64)>, sh: Arc<AudioShared>, rate: u32) {
    let high_water = rate as usize; // 0.5 s of stereo samples
    let mut epoch = 0u64;
    let mut eof = false;
    let mut buf: Vec<f32> = Vec::with_capacity(16 * 1024);

    loop {
        let idle = eof || sh.queued() >= high_water;
        let cmd = if idle {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => return,
            }
        } else {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        };
        if let Some((mut t, mut e)) = cmd {
            while let Ok((t2, e2)) = rx.try_recv() {
                (t, e) = (t2, e2); // coalesce
            }
            w.seek(t);
            epoch = e;
            eof = false;
            continue;
        }
        if idle {
            continue;
        }
        buf.clear();
        if w.next_chunk(&mut buf) {
            sh.push(epoch, &buf);
        } else {
            sh.mark_eof(epoch);
            eof = true;
        }
    }
}

// ================================================================== AUDIO OUTPUT (cpal)
fn build_stream<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, sh: Arc<AudioShared>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = (cfg.channels as usize).max(1);
    let rate = cfg.sample_rate.0 as f64;
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            if !sh.playing.load(Relaxed) {
                data.fill(T::EQUILIBRIUM);
                return;
            }
            let ts = info.timestamp();
            let lat = ts.playback.duration_since(&ts.callback).map_or(0.0, |d| d.as_secs_f64());
            let frames = data.len() / ch;
            let muted = sh.muted.load(Relaxed);
            let gain = f32::from_bits(sh.gain.load(Relaxed)).clamp(0.0, 4.0);
            let mut b = sh.lock();
            let t0 = b.next_t;
            let (mut pl, mut pr) = (0f32, 0f32);
            let mut got = 0usize;
            for frame in data.chunks_exact_mut(ch) {
                if b.q.len() < 2 {
                    break;
                }
                let (l, r) = (b.q.pop_front().unwrap_or(0.0), b.q.pop_front().unwrap_or(0.0));
                let (l, r) = if muted { (0.0, 0.0) } else { (l * gain, r * gain) }; // still consumed: stays in sync
                pl = pl.max(l.abs());
                pr = pr.max(r.abs());
                if ch == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                } else {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    for s in &mut frame[2..] {
                        *s = T::EQUILIBRIUM;
                    }
                }
                got += 1;
            }
            for s in &mut data[got * ch..] {
                *s = T::EQUILIBRIUM; // underrun tail / odd remainder -> silence
            }
            let starved = got < frames;
            let silent_run = starved && b.eof; // audio ended before the video: keep time moving
            let advanced = if silent_run { frames } else { got };
            b.next_t += advanced as f64 / rate;
            drop(b);
            // clock = what is audible *right now* = start of this buffer minus output latency
            sh.clock.set(t0 - lat, !starved || silent_run);
            sh.peak_l.fetch_max(pl.to_bits(), Relaxed);
            sh.peak_r.fetch_max(pr.to_bits(), Relaxed);
        },
        |e| eprintln!("audio stream error: {e}"),
        None,
    )
}

struct Audio {
    sh: Arc<AudioShared>,
    cmd: Sender<(f64, u64)>,
    _stream: cpal::Stream, // keep alive; !Send, so Player stays on the UI thread
}

fn open_audio(path: &Path) -> Option<Audio> {
    let dev = cpal::default_host().default_output_device()?;
    let supported = dev.default_output_config().ok()?;
    let fmt = supported.sample_format();
    let cfg: cpal::StreamConfig = supported.into();
    let rate = cfg.sample_rate.0;

    let sh = Arc::new(AudioShared::new());
    let (cmd_tx, cmd_rx) = channel::<(f64, u64)>();
    let (ok_tx, ok_rx) = channel::<bool>();
    let (p, shc) = (path.to_path_buf(), sh.clone());
    std::thread::Builder::new()
        .name("audio-decode".into())
        .spawn(move || match AudioWorker::open(&p, rate) {
            Ok(Some(w)) => {
                let _ = ok_tx.send(true);
                audio_run(w, cmd_rx, shc, rate);
            }
            _ => {
                let _ = ok_tx.send(false); // no audio stream (or undecodable)
            }
        })
        .ok()?;
    if !ok_rx.recv().unwrap_or(false) {
        return None;
    }
    let stream = match fmt {
        cpal::SampleFormat::F32 => build_stream::<f32>(&dev, &cfg, sh.clone()),
        cpal::SampleFormat::I16 => build_stream::<i16>(&dev, &cfg, sh.clone()),
        cpal::SampleFormat::U16 => build_stream::<u16>(&dev, &cfg, sh.clone()),
        _ => return None,
    }
    .ok()?;
    stream.play().ok()?;
    Some(Audio { sh, cmd: cmd_tx, _stream: stream })
}

// ================================================================== PLAYER (UI side)
pub struct Player {
    pub info: Info,
    pub current: Option<Frame>,
    cmd: Sender<Cmd>,
    frames: Receiver<Frame>,
    pending: VecDeque<Frame>,
    gen: u64,
    need_first: bool,
    audio: Option<Audio>,
}

impl Player {
    pub fn open(path: &Path, ctx: Context) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| e.to_string())?;
        let (cmd_tx, cmd_rx) = channel();
        let (fr_tx, fr_rx) = sync_channel(VIDEO_QUEUE);
        let (info_tx, info_rx) = channel();
        let pbuf: PathBuf = path.into();

        std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || match VideoWorker::open(&pbuf) {
                Ok((w, info)) => {
                    let _ = info_tx.send(Ok(info));
                    video_run(w, cmd_rx, fr_tx, ctx, info.fps);
                }
                Err(e) => {
                    let _ = info_tx.send(Err(e.to_string()));
                }
            })
            .map_err(|e| e.to_string())?;

        let info: Info = info_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            info,
            current: None,
            cmd: cmd_tx,
            frames: fr_rx,
            pending: VecDeque::new(),
            gen: 0,
            need_first: true,
            audio: open_audio(path),
        })
    }

    pub fn set_quality(&mut self, div: u32) {
        let _ = self.cmd.send(Cmd::Quality(div));
    }

    pub fn set_muted(&self, on: bool) {
        if let Some(a) = &self.audio { a.sh.muted.store(on, Relaxed); }
    }

    /// Set real-time linear audio gain (0..4). Safe to call during playback.
    pub fn set_volume(&self, gain: f32) {
        if let Some(a) = &self.audio { a.sh.gain.store(gain.clamp(0.0, 4.0).to_bits(), Relaxed); }
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Master clock in seconds (None when the file has no usable audio).
    pub fn clock(&self) -> Option<f64> {
        self.audio.as_ref().map(|a| a.sh.clock.now())
    }

    /// Peak-hold since the last call, linear 0..1+. None without audio.
    pub fn peaks(&self) -> Option<(f32, f32)> {
        let a = self.audio.as_ref()?;
        Some((f32::from_bits(a.sh.peak_l.swap(0, Relaxed)), f32::from_bits(a.sh.peak_r.swap(0, Relaxed))))
    }

    fn seek_audio(&self, t: f64) {
        if let Some(a) = &self.audio {
            let e = a.sh.reset(t);
            let _ = a.cmd.send((t, e));
        }
    }

    /// Jump (scrub / click). Stale in-flight frames and samples are discarded.
    pub fn seek(&mut self, t: f64) {
        self.gen += 1;
        self.pending.clear();
        self.need_first = true;
        let _ = self.cmd.send(Cmd::Seek(t, self.gen));
        self.seek_audio(t);
    }

    /// Start/stop real-time playback. `resync` re-aligns audio to that source time (exact resume).
    pub fn set_playing(&mut self, on: bool, resync: Option<f64>) {
        let _ = self.cmd.send(Cmd::Play(on));
        if let Some(a) = &self.audio {
            if on {
                if let Some(t) = resync {
                    self.seek_audio(t);
                }
            }
            a.sh.playing.store(on, Relaxed);
        }
    }

    /// Pull finished frames and pick the one for time `t`. True if `current` changed.
    pub fn poll(&mut self, t: f64) -> bool {
        while let Ok(f) = self.frames.try_recv() {
            if f.gen == self.gen {
                self.pending.push_back(f);
            }
        }
        let mut changed = false;
        if self.need_first {
            if let Some(f) = self.pending.pop_front() {
                self.current = Some(f);
                self.need_first = false;
                changed = true;
            }
        }
        let eps = 0.5 / self.info.fps;
        while self.pending.front().map_or(false, |f| f.pts <= t + eps) {
            self.current = self.pending.pop_front();
            changed = true;
        }
        changed
    }
}
