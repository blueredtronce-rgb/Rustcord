//! In-app video playback: MP4/MOV with H.264 + AAC-LC from Discord's media hosts, decoded on one
//! worker thread per player, frames through a media-own `VideoFrames` (`VideoKey::Media`).
//! Design, limits and budgets: docs/MEDIA_DESIGN.md. UI: `ui/viewer.rs`.
//!
//! OWNERSHIP: media (`src/media/*`, `src/ui/viewer.rs`).
//!
//! # For other areas (Quests: video quests)
//! - Easiest: `ui::viewer::video_slot(ui, cx, url, size, no_seek_ahead)` draws a player with
//!   poster-less controls in `size` (a modal-sized slot) and returns its `Status` every paint.
//!   With `no_seek_ahead` the seek bar never goes past the furthest point already played.
//! - Or own a `Player` (`Player::open(ctx, url, Opts { no_seek_ahead: true, .. })`), call
//!   `drawn()` and `set_target()` on every paint it is visible, read `status()`:
//!   - `played_ms`: media time that really played (state Playing) while the player was drawn in
//!     the last second (unless `Opts::background`: drawn or not). Monotonic, never settable, advances at most in real time; a seek does not
//!     add to it. This is the only number a quest's progress may come from.
//!   - `position_ms` / `duration_ms` / `max_ms` (furthest position reached by playing).
//!   - `state`: Loading, Paused, Playing, Ended, Failed(why), Freed (worker gone: drop the
//!     handle; reopening starts at 0).
//! - A player not drawn for 1 s pauses itself (scrolled away, window hidden or minimized), unless
//!   `Opts::background` (quest videos); one paused or ended and not drawn for 30 s frees its decoders and thread.

pub mod audio;
pub mod mp4;
pub mod net;

use crate::voice::video::{FrameWriter, VideoFrames, VideoKey, codec};
use openh264::OpenH264API;
use openh264::decoder::{DecodeOptions, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

/// Not drawn this long while playing: pause.
const HIDDEN_PAUSE: Duration = Duration::from_secs(1);
/// Paused or ended and not drawn this long: the worker exits and frees everything.
const IDLE_FREE: Duration = Duration::from_secs(30);
/// Decoded sound kept ahead of the speaker.
const AUDIO_AHEAD_MS: u64 = 400;
const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// Why a video can't play here (the viewer's error card).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// Not MP4/MOV with H.264 (HEVC, WebM, AV1, interlaced …) or a broken file.
    Unsupported,
    /// Beyond 1080p60.
    TooLarge,
    /// No connection, a server error, offline mode without a cached copy: Retry.
    Network,
    /// 403 / 404 / 410: deleted, or an expired signed link.
    Gone,
    /// The server ignores Range and the file is over 32 MiB.
    CantStream,
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Fail {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Loading,
    Paused,
    Playing,
    Ended,
    Failed(Fail),
    /// The worker exited (idle, or its handle was dropped).
    Freed,
}

#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// Start again at the end (gifv).
    pub looped: bool,
    /// No sound at all (gifv): no speaker is opened.
    pub muted: bool,
    /// Seeks never go past the furthest position reached by playing (quest videos).
    pub no_seek_ahead: bool,
    /// Speaker name (Voice & Video's output), else the default.
    pub output: Option<String>,
    /// Keeps playing while not drawn (quest videos): no pause after `HIDDEN_PAUSE`, `played_ms`
    /// counts while playing drawn or not, pictures not drawn are decoded but neither converted
    /// nor published (no UI wakes).
    pub background: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub played_ms: u64,
    pub max_ms: u64,
    /// Video size (0 until known).
    pub size: (u32, u32),
    pub has_audio: bool,
    /// Playing, but waiting for data.
    pub buffering: bool,
}

#[derive(Default)]
struct Shared {
    state: AtomicU8,
    fail: AtomicU8,
    pos: AtomicU64,
    dur: AtomicU64,
    played: AtomicU64,
    max: AtomicU64,
    /// `now_ms()` of the last paint that showed the player.
    drawn: AtomicU64,
    /// Wanted frame size (w << 32 | h), 0 = the video's own.
    target: AtomicU64,
    size: AtomicU64,
    has_audio: AtomicBool,
    buffering: AtomicBool,
    volume: AtomicU32,
}

impl Shared {
    fn set(&self, s: State) {
        let (n, f) = match s {
            State::Loading => (0, 0),
            State::Paused => (1, 0),
            State::Playing => (2, 0),
            State::Ended => (3, 0),
            State::Freed => (4, 0),
            State::Failed(f) => (5, f as u8),
        };
        self.fail.store(f, Relaxed);
        self.state.store(n, Relaxed);
    }

    fn state(&self) -> State {
        const FAILS: [Fail; 5] = [Fail::Unsupported, Fail::TooLarge, Fail::Network, Fail::Gone, Fail::CantStream];
        match self.state.load(Relaxed) {
            0 => State::Loading,
            1 => State::Paused,
            2 => State::Playing,
            3 => State::Ended,
            4 => State::Freed,
            _ => State::Failed(FAILS[usize::from(self.fail.load(Relaxed)).min(4)]),
        }
    }

    fn drawn_age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.drawn.load(Relaxed)))
    }
}

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

enum Ctl {
    Play,
    Pause,
    Seek(u64),
}

/// The media-own frame slots (at most `MAX_FRAME_SLOTS` players with frames).
pub fn frames(ctx: &egui::Context) -> Arc<VideoFrames> {
    static F: OnceLock<Arc<VideoFrames>> = OnceLock::new();
    F.get_or_init(|| Arc::new(VideoFrames::new(ctx.clone()))).clone()
}

/// Player key of a media URL: the image cache's key (signature params dropped), hashed.
pub fn key_of(url: &str) -> u64 {
    crate::cache::images::fnv1a64(crate::cache::images::cache_key(url).as_bytes())
}

/// Handle of one playing (or paused) video; dropping it stops the worker and frees its frame.
pub struct Player {
    sh: Arc<Shared>,
    tx: mpsc::Sender<Ctl>,
    key: u64,
    pub opts: Opts,
}

impl Player {
    /// Starts the worker: it opens and parses `url`, shows the first frame and waits paused.
    pub fn open(ctx: &egui::Context, url: &str, opts: Opts) -> Player {
        let sh = Arc::new(Shared::default());
        sh.volume.store(1f32.to_bits(), Relaxed);
        sh.drawn.store(now_ms(), Relaxed);
        let (tx, rx) = mpsc::channel();
        let key = key_of(url);
        let (frames, ctx, url2, o, s) = (frames(ctx), ctx.clone(), url.to_owned(), opts.clone(), sh.clone());
        let spawned = std::thread::Builder::new().name("rustcord-media".into()).spawn(move || worker(&url2, &o, &s, &rx, &frames, key, &ctx));
        if spawned.is_err() {
            sh.set(State::Failed(Fail::Network));
        }
        Player { sh, tx, key, opts }
    }

    pub fn key(&self) -> VideoKey {
        VideoKey::Media(self.key)
    }

    pub fn status(&self) -> Status {
        let size = self.sh.size.load(Relaxed);
        Status {
            state: self.sh.state(),
            position_ms: self.sh.pos.load(Relaxed),
            duration_ms: self.sh.dur.load(Relaxed),
            played_ms: self.sh.played.load(Relaxed),
            max_ms: self.sh.max.load(Relaxed),
            size: ((size >> 32) as u32, size as u32),
            has_audio: self.sh.has_audio.load(Relaxed),
            buffering: self.sh.buffering.load(Relaxed),
        }
    }

    pub fn play(&self) {
        let _ = self.tx.send(Ctl::Play);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Ctl::Pause);
    }

    /// To `ms` (clamped to the furthest point played with `no_seek_ahead`).
    pub fn seek(&self, ms: u64) {
        let _ = self.tx.send(Ctl::Seek(ms));
    }

    /// 0.0 ..= 1.0 (0 = muted).
    pub fn set_volume(&self, v: f32) {
        self.sh.volume.store(v.clamp(0.0, 1.0).to_bits(), Relaxed);
    }

    /// Frame size wanted (physical px; frames are converted straight to it).
    pub fn set_target(&self, w: u32, h: u32) {
        self.sh.target.store(u64::from(w) << 32 | u64::from(h), Relaxed);
    }

    /// Call on every paint that shows this player.
    pub fn drawn(&self) {
        self.sh.drawn.store(now_ms(), Relaxed);
    }

    /// Time since the last `drawn`.
    pub fn idle(&self) -> Duration {
        self.sh.drawn_age()
    }
}

fn worker(url: &str, opts: &Opts, sh: &Arc<Shared>, rx: &mpsc::Receiver<Ctl>, frames: &VideoFrames, key: u64, ctx: &egui::Context) {
    let vkey = VideoKey::Media(key);
    // re_mp4 parses untrusted files and is not fuzzed: a panic is an unsupported file.
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(url, opts, sh, rx, frames, &vkey, ctx)));
    frames.remove(&vkey);
    sh.set(match out {
        Ok(Ok(())) => State::Freed,
        Ok(Err(f)) => State::Failed(f),
        Err(_) => State::Failed(Fail::Unsupported),
    });
    ctx.request_repaint_of(egui::ViewportId::ROOT);
}

fn run(url: &str, opts: &Opts, sh: &Shared, rx: &mpsc::Receiver<Ctl>, frames: &VideoFrames, vkey: &VideoKey, ctx: &egui::Context) -> Result<(), Fail> {
    let (mut r, len) = net::open(url)?;
    let mut e = Engine::new(&mut r, len, opts, sh, frames.writer(vkey.clone()).ok_or(Fail::Network)?)?;
    e.r = Some(r);
    e.seek(0);
    sh.set(State::Paused);
    ctx.request_repaint_of(egui::ViewportId::ROOT);
    loop {
        let msg = if e.playing || e.show_one {
            match rx.try_recv() {
                Ok(m) => Some(m),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        } else {
            // Wakes only when the 30 s since the last paint are up (or a command comes).
            let Some(wait) = IDLE_FREE.checked_sub(sh.drawn_age()).filter(|w| !w.is_zero()) else { return Ok(()) };
            match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        };
        match msg {
            Some(Ctl::Play) => e.play(),
            Some(Ctl::Pause) => e.pause(State::Paused),
            Some(Ctl::Seek(ms)) => e.seek(if opts.no_seek_ahead { ms.min(sh.max.load(Relaxed)) } else { ms }),
            // A second's grace after a (re)start: a slow load can outlast the UI's last paint.
            None if e.playing && !opts.background && sh.drawn_age() >= HIDDEN_PAUSE && e.started.is_some_and(|t| t.elapsed() >= HIDDEN_PAUSE) => e.pause(State::Paused),
            None if e.playing => e.step()?,
            None if e.show_one => e.show_paused()?,
            None => {}
        }
        if msg.is_some() {
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }
}

struct AudioPart {
    a: mp4::Audio,
    i: usize,
    dec: symphonia_codec_aac::AacDecoder,
    out: audio::AudioOut,
    pcm: Vec<f32>,
    /// False once every sample played: the clock runs on `Instant` from then on.
    active: bool,
}

/// A converted picture waiting for its time.
struct Pending {
    pts: u64,
    size: (u32, u32),
    rgba: Vec<u8>,
}

struct Engine<'a> {
    r: Option<Box<dyn net::ReadSeek>>,
    sh: &'a Shared,
    opts: &'a Opts,
    v: mp4::Video,
    audio: Option<AudioPart>,
    dec: Decoder,
    writer: FrameWriter,
    /// Next video sample to feed (decode order), SPS/PPS sent since the last (re)start.
    vi: usize,
    fed_params: bool,
    /// Presentation times of fed samples not yet output (smallest first = next picture's).
    heap: BinaryHeap<Reverse<i64>>,
    pending: VecDeque<Pending>,
    spare: Vec<Vec<u8>>,
    v_eos: bool,
    /// Pictures that end at or before this (ms) are decoded, not shown (after a seek).
    skip_until: u64,
    frame_ms: u64,
    dur: u64,
    playing: bool,
    /// Publish the next picture at once (paused after open or a seek).
    show_one: bool,
    base: u64,
    started: Option<Instant>,
    last_clock: u64,
    sample: Vec<u8>,
    annexb: Vec<u8>,
}

impl<'a> Engine<'a> {
    fn new(r: &mut Box<dyn net::ReadSeek>, len: u64, opts: &'a Opts, sh: &'a Shared, writer: FrameWriter) -> Result<Engine<'a>, Fail> {
        mp4::check_boxes(r, len)?;
        r.seek(SeekFrom::Start(0)).map_err(|e| net::fail_of(&e))?;
        let parsed = re_mp4::Mp4::read(&mut *r, len).map_err(|e| match e {
            re_mp4::Error::Io(e) => net::fail_of(&e),
            _ => Fail::Unsupported,
        })?;
        let t = mp4::pick_tracks(&parsed)?;
        drop(parsed);
        let audio = t.audio.filter(|_| !opts.muted).and_then(|a| {
            let rate = AAC_RATES[usize::from((a.asc[0] & 7) << 1 | a.asc[1] >> 7).min(12)];
            let mut params = AudioCodecParameters::new();
            params.for_codec(CODEC_ID_AAC).with_extra_data(Box::new(a.asc));
            let dec = symphonia_codec_aac::AacDecoder::try_new(&params, &AudioDecoderOptions::default()).ok()?;
            // No speaker: the video plays silent.
            let out = audio::AudioOut::open(opts.output.as_deref(), rate).ok()?;
            // Opened paused: the speaker only runs while the video plays (0 % idle CPU).
            out.run(false);
            Some(AudioPart { a, i: 0, dec, out, pcm: Vec::new(), active: true })
        });
        sh.dur.store(t.duration_ms, Relaxed);
        sh.size.store(u64::from(t.video.size.0) << 32 | u64::from(t.video.size.1), Relaxed);
        sh.has_audio.store(audio.is_some(), Relaxed);
        Ok(Engine {
            r: None,
            sh,
            opts,
            frame_ms: (t.duration_ms / t.video.samples.len().max(1) as u64).max(1),
            dur: t.duration_ms,
            v: t.video,
            audio,
            dec: new_decoder()?,
            writer,
            vi: 0,
            fed_params: false,
            heap: BinaryHeap::new(),
            pending: VecDeque::new(),
            spare: Vec::new(),
            v_eos: false,
            skip_until: 0,
            playing: false,
            show_one: true,
            base: 0,
            started: None,
            last_clock: 0,
            sample: Vec::new(),
            annexb: Vec::new(),
        })
    }

    /// Audio master clock (ms); `Instant` without (or after) sound.
    fn clock(&self) -> u64 {
        match &self.audio {
            Some(a) if a.active && self.playing => self.base + a.out.played_frames() * 1000 / u64::from(a.out.rate().max(1)),
            _ => self.base + self.started.map_or(0, |t| t.elapsed().as_millis() as u64),
        }
    }

    fn play(&mut self) {
        if self.playing {
            return;
        }
        if self.sh.state() == State::Ended {
            self.seek(0);
        }
        self.playing = true;
        self.started = Some(Instant::now());
        self.last_clock = self.base;
        let base = self.base;
        self.seek_audio(base);
        if let Some(a) = &self.audio {
            a.out.run(true);
        }
        self.sh.set(State::Playing);
    }

    fn pause(&mut self, to: State) {
        if self.playing {
            self.base = self.clock().min(self.dur);
            self.playing = false;
            self.started = None;
            if let Some(a) = &mut self.audio {
                a.out.run(false);
                a.out.clear();
            }
        }
        self.sh.buffering.store(false, Relaxed);
        self.sh.set(to);
    }

    fn seek(&mut self, ms: u64) {
        let ms = ms.min(self.dur.saturating_sub(1));
        let k = mp4::keyframe_for(&self.v.samples, (ms * self.v.timescale / 1000) as i64);
        if let Ok(d) = new_decoder() {
            self.dec = d;
        }
        (self.vi, self.fed_params, self.v_eos, self.skip_until) = (k, false, false, ms);
        self.heap.clear();
        while let Some(p) = self.pending.pop_front() {
            self.spare.push(p.rgba);
        }
        self.base = ms;
        self.started = self.playing.then(Instant::now);
        self.last_clock = ms;
        self.seek_audio(ms);
        self.sh.pos.store(ms, Relaxed);
        self.show_one = true;
        if self.sh.state() == State::Ended {
            self.sh.set(State::Paused);
        }
    }

    fn seek_audio(&mut self, ms: u64) {
        if let Some(a) = &mut self.audio {
            a.i = mp4::audio_index_for(&a.a.samples, (ms * a.a.timescale / 1000) as i64);
            a.dec.reset();
            a.out.clear();
            a.active = true;
        }
    }

    fn read(&mut self, s: mp4::S) -> Result<(), Fail> {
        let r = self.r.as_mut().ok_or(Fail::Network)?;
        self.sample.resize(s.size as usize, 0);
        r.seek(SeekFrom::Start(s.offset)).and_then(|_| r.read_exact(&mut self.sample)).map_err(|e| net::fail_of(&e))
    }

    /// Keeps `AUDIO_AHEAD_MS` of sound queued.
    fn fill_audio(&mut self) -> Result<(), Fail> {
        let Some(a) = &mut self.audio else { return Ok(()) };
        if !a.active {
            return Ok(());
        }
        a.out.set_volume(f32::from_bits(self.sh.volume.load(Relaxed)));
        let ahead = u64::from(a.out.rate()) * AUDIO_AHEAD_MS / 1000;
        while (a.out.queued_frames() as u64) < ahead && a.i < a.a.samples.len() {
            let s = a.a.samples[a.i];
            a.i += 1;
            let r = self.r.as_mut().ok_or(Fail::Network)?;
            self.sample.resize(s.size as usize, 0);
            r.seek(SeekFrom::Start(s.offset)).and_then(|_| r.read_exact(&mut self.sample)).map_err(|e| net::fail_of(&e))?;
            // A broken packet is skipped (a short gap), not the end of the video.
            if let Ok(buf) = a.dec.decode_ref(&PacketRef::new(0, s.cts.into(), s.dur.into(), &self.sample)) {
                let ch = buf.spec().channels().count();
                buf.copy_to_vec_interleaved(&mut a.pcm);
                a.out.push(&a.pcm, ch);
            }
        }
        if a.i >= a.a.samples.len() && a.out.queued_frames() == 0 {
            // Sound ended before the picture: the clock continues on its own.
            let now = self.base + a.out.played_frames() * 1000 / u64::from(a.out.rate().max(1));
            a.active = false;
            self.base = now;
            self.started = Some(Instant::now());
        }
        Ok(())
    }

    /// A `background` player nobody has drawn for `HIDDEN_PAUSE`: its pictures are not shown.
    fn unseen(&self) -> bool {
        self.opts.background && self.sh.drawn_age() >= HIDDEN_PAUSE
    }

    /// Feeds one sample (or flushes at the end) and queues the picture that comes out.
    fn decode_next(&mut self, late_before: u64) -> Result<(), Fail> {
        // shortcut: unseen pictures are still decoded (H.264 references); audio-only needs a keyframe resync when drawn again. Upgrade if 1080p background CPU matters.
        let unseen = self.unseen();
        let (w, h) = self.out_size();
        let Some(&s) = self.v.samples.get(self.vi) else {
            let rest = self.dec.flush_remaining().map_err(|_| Fail::Unsupported)?;
            for yuv in &rest {
                let pts = self.heap.pop().map_or(self.dur, |Reverse(t)| t.max(0) as u64 * 1000 / self.v.timescale);
                if pts + self.frame_ms > self.skip_until.max(late_before) {
                    let rgba = if unseen { Vec::new() } else { convert(yuv, (w, h), self.spare.pop()) };
                    self.pending.push_back(Pending { pts, size: (w, h), rgba });
                }
            }
            self.v_eos = true;
            return Ok(());
        };
        self.vi += 1;
        self.read(s)?;
        self.annexb.clear();
        if !self.fed_params {
            self.annexb.extend_from_slice(&self.v.params);
            self.fed_params = true;
        }
        mp4::to_annexb(&self.sample, self.v.nal_len, &mut self.annexb).ok_or(Fail::Unsupported)?;
        self.heap.push(Reverse(s.cts));
        // openh264 refuses broken access units; the next keyframe recovers.
        if let Ok(Some(yuv)) = self.dec.decode_with_options(&self.annexb, DecodeOptions::new()) {
            let pts = self.heap.pop().map_or(0, |Reverse(t)| t.max(0) as u64 * 1000 / self.v.timescale);
            if pts + self.frame_ms > self.skip_until.max(late_before) {
                let rgba = if unseen { Vec::new() } else { convert(&yuv, (w, h), self.spare.pop()) };
                self.pending.push_back(Pending { pts, size: (w, h), rgba });
            }
        }
        Ok(())
    }

    /// Frame size: the UI's target, never above the video or 1920×1080, even.
    fn out_size(&self) -> (u32, u32) {
        let (vw, vh) = self.v.size;
        let t = self.sh.target.load(Relaxed);
        let (tw, th) = if t == 0 { (vw, vh) } else { ((t >> 32) as u32, t as u32) };
        let (mw, mh) = crate::voice::video::MAX_FRAME_SIZE;
        codec::fit_within(vw, vh, tw.clamp(2, mw), th.clamp(2, mh))
    }

    fn publish(&mut self) {
        if let Some(p) = self.pending.pop_front() {
            if !p.rgba.is_empty() {
                self.writer.write(p.size.0, p.size.1, |px| {
                    if px.len() == p.rgba.len() {
                        px.copy_from_slice(&p.rgba);
                    }
                });
            }
            self.sh.pos.store(p.pts.min(self.dur), Relaxed);
            self.spare.push(p.rgba);
        }
    }

    /// Paused after open or a seek: decode up to the picture at the position and show it.
    fn show_paused(&mut self) -> Result<(), Fail> {
        if self.pending.is_empty() && !self.v_eos {
            return self.decode_next(0);
        }
        self.publish();
        self.show_one = false;
        Ok(())
    }

    fn step(&mut self) -> Result<(), Fail> {
        self.fill_audio()?;
        let clock = self.clock().min(self.dur);
        if clock > self.last_clock {
            // Only time that played on screen counts (docs at the top).
            if self.opts.background || self.sh.drawn_age() < HIDDEN_PAUSE {
                self.sh.played.fetch_add(clock - self.last_clock, Relaxed);
            }
            self.sh.max.fetch_max(clock, Relaxed);
            self.last_clock = clock;
        }
        if self.pending.is_empty() && !self.v_eos {
            let late = if self.show_one { 0 } else { clock.saturating_sub(2 * self.frame_ms) };
            let t = Instant::now();
            self.decode_next(late)?;
            self.sh.buffering.store(t.elapsed() > Duration::from_millis(250), Relaxed);
            return Ok(());
        }
        if let Some(p) = self.pending.front() {
            if p.pts <= clock || self.show_one {
                self.publish();
                self.show_one = false;
            } else {
                std::thread::sleep(Duration::from_millis((p.pts - clock).min(10)));
            }
            return Ok(());
        }
        // The picture ended; let the sound finish.
        if self.audio.as_ref().is_some_and(|a| a.active) && clock < self.dur {
            self.sh.pos.store(clock, Relaxed);
            std::thread::sleep(Duration::from_millis(10));
            return Ok(());
        }
        if self.opts.looped {
            self.seek(0);
            self.show_one = false;
            return Ok(());
        }
        self.sh.pos.store(self.dur, Relaxed);
        self.sh.max.store(self.dur, Relaxed);
        self.pause(State::Ended);
        self.base = self.dur;
        Ok(())
    }
}

fn new_decoder() -> Result<Decoder, Fail> {
    // NoFlush: with the default a stream with B-frames comes out of order (docs/MEDIA_DESIGN.md §3).
    Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new().flush_after_decode(Flush::NoFlush)).map_err(|_| Fail::Unsupported)
}

/// RGBA of `yuv` at `size` into a reused buffer.
fn convert(yuv: &openh264::decoder::DecodedYUV<'_>, size: (u32, u32), buf: Option<Vec<u8>>) -> Vec<u8> {
    let mut buf = buf.unwrap_or_default();
    buf.resize(size.0 as usize * size.1 as usize * 4, 0);
    let (w, h) = yuv.dimensions();
    if (w as u32, h as u32) == size {
        // openh264's SIMD path at the native size.
        yuv.write_rgba8(&mut buf);
    } else {
        // shortcut: BT.601 limited range; phone videos are BT.709 (slight hue shift). Read colr/VUI if noticed.
        codec::i420_to_rgba(yuv, size, &mut buf);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's frame number: frame n has luma 16 + 2n in its top-left block.
    fn frame_no(rgba: &[u8], w: usize) -> u32 {
        let mut sum = 0u32;
        for r in 8..24 {
            for c in 8..24 {
                let p = &rgba[(r * w + c) * 4..];
                // RGB of a grey: R = G = B ≈ 1.164 (Y - 16).
                sum += u32::from(p[1]);
            }
        }
        let y = (f64::from(sum) / 256.0 / 1.164 + 16.0).round() as u32;
        y.saturating_sub(16).div_ceil(2)
    }

    fn engine<'a>(opts: &'a Opts, sh: &'a Shared, frames: &VideoFrames) -> Engine<'a> {
        let bytes = include_bytes!("../../tests/fixtures/media/demo.mp4").to_vec();
        let len = bytes.len() as u64;
        let mut r: Box<dyn net::ReadSeek> = Box::new(std::io::Cursor::new(bytes));
        let mut e = Engine::new(&mut r, len, opts, sh, frames.writer(VideoKey::Media(1)).unwrap()).unwrap();
        e.r = Some(r);
        e.seek(0);
        e
    }

    #[test]
    fn decodes_the_fixture_in_display_order() {
        let (opts, sh, frames) = (Opts { muted: true, ..Opts::default() }, Shared::default(), VideoFrames::default());
        let mut e = engine(&opts, &sh, &frames);
        assert_eq!(e.v.size, (640, 360));
        let mut order = Vec::new();
        while !(e.v_eos && e.pending.is_empty()) {
            if let Some(p) = e.pending.pop_front() {
                order.push((p.pts, frame_no(&p.rgba, p.size.0 as usize)));
            } else {
                e.decode_next(0).unwrap();
            }
        }
        assert_eq!(order.len(), 90);
        assert!(order.windows(2).all(|w| w[1].0 > w[0].0), "presentation times rise");
        let wrong: Vec<_> = order.iter().enumerate().filter(|(i, (_, n))| *n != *i as u32).collect();
        assert!(wrong.is_empty(), "frame n shown n-th: {wrong:?}");
        assert!((order[30].0 as i64 - 1000).abs() <= 1, "frame 30 at 1 s: {:?}", order[30]);
    }

    #[test]
    fn aac_track_gives_three_seconds() {
        let bytes = include_bytes!("../../tests/fixtures/media/demo.mp4");
        let t = mp4::pick_tracks(&re_mp4::Mp4::read_bytes(bytes).unwrap()).unwrap();
        let a = t.audio.unwrap();
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).with_extra_data(Box::new(a.asc));
        let mut dec = symphonia_codec_aac::AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let (mut frames, mut rate) = (0usize, 0u32);
        for s in &a.samples {
            let data = &bytes[s.offset as usize..][..s.size as usize];
            let out = dec.decode_ref(&PacketRef::new(0, s.cts.into(), s.dur.into(), data)).unwrap();
            rate = out.spec().rate();
            frames += out.frames();
        }
        let secs = frames as f64 / f64::from(rate);
        assert!((secs - 3.0).abs() < 0.05, "{secs} s at {rate} Hz");
    }

    #[test]
    fn seek_starts_at_a_keyframe_and_shows_the_target() {
        let (opts, sh, frames) = (Opts { muted: true, ..Opts::default() }, Shared::default(), VideoFrames::default());
        let mut e = engine(&opts, &sh, &frames);
        e.seek(1730);
        assert!(e.v.samples[e.vi].sync, "decoding restarts at a sync sample");
        assert!(e.v.samples[e.vi].cts * 1000 / e.v.timescale as i64 <= 1730);
        while e.pending.is_empty() {
            e.decode_next(0).unwrap();
        }
        let (pts, n) = e.pending.front().map(|p| (p.pts, frame_no(&p.rgba, 640))).unwrap();
        assert!(pts <= 1730 && 1730 < pts + e.frame_ms, "first picture shown covers 1.73 s: {pts}");
        assert_eq!(n, 51);
        e.show_paused().unwrap();
        assert_eq!(frames.latest(&VideoKey::Media(1)).map(|f| f.1.size), Some([640, 360]));
        assert!((1700..=1730).contains(&sh.pos.load(Relaxed)), "the position is the shown picture's");
    }

    #[test]
    fn played_time_needs_the_player_on_screen() {
        let (opts, sh, frames) = (Opts { muted: true, no_seek_ahead: true, ..Opts::default() }, Shared::default(), VideoFrames::default());
        let mut e = engine(&opts, &sh, &frames);
        // Drawn long ago: playing advances the position, not `played_ms`.
        sh.drawn.store(now_ms(), Relaxed);
        std::thread::sleep(Duration::from_millis(1100));
        e.play();
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            e.step().unwrap();
        }
        assert_eq!(sh.played.load(Relaxed), 0);
        assert!(sh.max.load(Relaxed) >= 250);
        // On screen: it counts, at most in real time.
        let before = sh.max.load(Relaxed);
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            sh.drawn.store(now_ms(), Relaxed);
            e.step().unwrap();
        }
        let played = sh.played.load(Relaxed);
        assert!((200..=330).contains(&played), "{played} ms");
        // A seek adds nothing, and seeking ahead is clamped by `run` to `max`.
        e.seek(2500.min(sh.max.load(Relaxed)));
        assert_eq!(sh.played.load(Relaxed), played);
        assert!(sh.max.load(Relaxed) >= before);
    }

    #[test]
    fn background_plays_unseen() {
        let (opts, sh, frames) = (Opts { muted: true, background: true, ..Opts::default() }, Shared::default(), VideoFrames::default());
        let mut e = engine(&opts, &sh, &frames);
        sh.drawn.store(now_ms(), Relaxed);
        std::thread::sleep(Duration::from_millis(1100));
        e.play();
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            e.step().unwrap();
        }
        let played = sh.played.load(Relaxed);
        assert!((200..=330).contains(&played), "{played} ms counts unseen");
        assert!(frames.latest(&VideoKey::Media(1)).is_none(), "nothing is published unseen");
        // Drawn again: pictures come out.
        let t = Instant::now();
        while frames.latest(&VideoKey::Media(1)).is_none() && t.elapsed() < Duration::from_secs(2) {
            sh.drawn.store(now_ms(), Relaxed);
            e.step().unwrap();
        }
        assert!(frames.latest(&VideoKey::Media(1)).is_some());
    }

    #[test]
    fn malformed_files_fail_without_panicking() {
        let bytes = include_bytes!("../../tests/fixtures/media/demo.mp4");
        let (opts, sh, frames) = (Opts { muted: true, ..Opts::default() }, Shared::default(), VideoFrames::default());
        for cut in [bytes.len() / 2, 100, 0] {
            let data = bytes[..cut].to_vec();
            let len = data.len() as u64;
            let mut r: Box<dyn net::ReadSeek> = Box::new(std::io::Cursor::new(data));
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Engine::new(&mut r, len, &opts, &sh, frames.writer(VideoKey::Media(2)).unwrap()).map(drop)));
            assert!(matches!(out, Ok(Err(_))), "cut at {cut}");
        }
        let mut big = bytes.to_vec();
        // The moov box (at the end of this file) claims 2 GB.
        let at = big.windows(4).position(|w| w == b"moov").unwrap() - 4;
        big[at..at + 4].copy_from_slice(&2_000_000_000u32.to_be_bytes());
        let len = big.len() as u64;
        let mut r: Box<dyn net::ReadSeek> = Box::new(std::io::Cursor::new(big));
        assert_eq!(Engine::new(&mut r, len, &opts, &sh, frames.writer(VideoKey::Media(3)).unwrap()).err(), Some(Fail::Unsupported));
    }
}
