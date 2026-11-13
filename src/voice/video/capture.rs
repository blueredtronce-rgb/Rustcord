//! Capture and the send pipeline: camera through Media Foundation (`windows` 0.62,
//! `IMFSourceReader`, NV12 out; `capture/camera.rs`), monitors/windows through `windows-capture`
//! 2.0 (Windows Graphics Capture, BGRA, cursor on, border off where allowed;
//! `capture/screen.rs`), frame pacing to `SendLimits::fps`, and the "rustcord-video-tx" thread:
//! capture → `codec::fit_within` / `codec::to_i420` → `codec::Encoder` → `EncodedFrame`s for the
//! transport (+ my own preview when shown).
//!
//! OWNERSHIP: capture engineer. Keep the pub signatures (stream.rs uses them). Add `windows`
//! features in Cargo.toml as needed (same 0.62.2, no new crate).
//!
//! Rules: a device or capture session is open only between `start` (a user's StartCamera /
//! StartScreenShare) and dropping its `Sending`; nothing is captured otherwise. Frames beyond
//! `fps` are dropped before conversion; at most `ENCODED_QUEUE` encoded frames wait. Non-Windows
//! builds: every function returns an error.
#![cfg_attr(not(windows), allow(dead_code, unused_imports, reason = "the capture sources exist on Windows only"))]

#[cfg(windows)]
mod camera;
#[cfg(windows)]
mod screen;

use super::codec::{self, Encoder, I420};
use super::{CameraInfo, ENCODED_QUEUE, EncodedFrame, FrameWriter, MonitorInfo, RawFrame, ScreenSource, SendLimits, StreamKind, VideoSend, WindowInfo, lock};
use openh264::formats::YUVSource;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

#[cfg(not(windows))]
const UNSUPPORTED: &str = "Video capture is only available on Windows.";
/// The transport dropped its end (its connection ended) while I was still capturing.
const CLOSED: &str = "The video connection closed.";
/// A resized window's new size must hold this long before the encoder restarts at it (each
/// restart begins with a keyframe); meanwhile frames are stretched into the running size.
const RESIZE_SETTLE: Duration = Duration::from_millis(500);

/// What to capture.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// `CameraInfo::id`; None = the first camera. Sent as `StreamKind::Camera`.
    Camera { device: Option<String> },
    /// Sent as `StreamKind::Screen`.
    Screen(ScreenSource),
}

/// Cameras. Blocking (COM / MF enumeration): call from `spawn_blocking`.
pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    #[cfg(windows)]
    {
        camera::list()
    }
    #[cfg(not(windows))]
    {
        Err(UNSUPPORTED.into())
    }
}

/// Monitors and shareable top-level windows (visible, titled, not ours). Blocking.
pub fn list_screen_sources() -> Result<(Vec<MonitorInfo>, Vec<WindowInfo>), String> {
    #[cfg(windows)]
    {
        screen::list()
    }
    #[cfg(not(windows))]
    {
        Err(UNSUPPORTED.into())
    }
}

/// A running capture + encode thread. Dropping it releases the device / closes the capture
/// session and ends the thread; the transport then sees `VideoSend::frames` close.
pub struct Sending {
    /// Never sent on: dropping it disconnects the thread's `Stop`.
    _stop: std_mpsc::Sender<()>,
    /// Set on drop, before `_stop` disconnects: the pipeline hands out no frame after it.
    stopped: Arc<AtomicBool>,
    preview: Arc<Mutex<Option<FrameWriter>>>,
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

impl Sending {
    /// Also write each (scaled) frame to my own preview tile, or stop doing so (`None`).
    pub fn set_preview(&self, preview: Option<FrameWriter>) {
        *lock(&self.preview) = preview;
    }
}

/// Start capturing `source` within `limits`, paced to `limits.fps`. Returns at once with the
/// transport half; opening the device happens on the thread. When capture stops by itself
/// (no such camera, device unplugged, window closed) the thread calls `failed` once with
/// user-facing text and ends; the manager then turns the video off and tells the UI.
pub fn start(source: Source, limits: SendLimits, failed: impl FnOnce(String) + Send + 'static) -> Result<(Sending, VideoSend), String> {
    #[cfg(not(windows))]
    {
        let _ = (source, limits, failed);
        Err(UNSUPPORTED.into())
    }
    #[cfg(windows)]
    {
        let kind = match source {
            Source::Camera { .. } => StreamKind::Camera,
            Source::Screen(_) => StreamKind::Screen,
        };
        let (tx, frames) = mpsc::channel(ENCODED_QUEUE);
        let keyframe = Arc::new(AtomicBool::new(false));
        let preview = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = std_mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let pipe = Pipeline::new(limits, kind, tx, keyframe.clone(), preview.clone(), stopped.clone());
        let stop = Stop(stop_rx, stopped.clone());
        std::thread::Builder::new()
            .name("rustcord-video-tx".into())
            .spawn(move || {
                let result = match source {
                    Source::Camera { device } => camera::run(device.as_deref(), pipe, &stop),
                    Source::Screen(screen) => screen::run(screen, pipe, &stop),
                };
                finish(result, &stop, failed);
            })
            .map_err(|_| "Could not start video.".to_string())?;
        Ok((Sending { _stop: stop_tx, stopped, preview }, VideoSend { kind, limits, frames, keyframe }))
    }
}

/// The capture thread's side of `Sending`: its channel (to wait on) and its `stopped` flag.
struct Stop(std_mpsc::Receiver<()>, Arc<AtomicBool>);

impl Stop {
    /// `Sending` was dropped: the user (or the call's end) stopped this capture.
    fn requested(&self) -> bool {
        self.1.load(Ordering::Acquire) || matches!(self.0.try_recv(), Err(std_mpsc::TryRecvError::Disconnected))
    }
}

/// End of the capture thread: report a failure, unless the capture was stopped on purpose.
fn finish(result: Result<(), String>, stop: &Stop, failed: impl FnOnce(String)) {
    if let Err(reason) = result
        && !stop.requested()
    {
        failed(reason);
    }
}

/// Drops frames beyond `fps`. A frame up to a quarter interval early still counts, so a source
/// running at exactly `fps` keeps its rate despite jitter.
struct Pacer {
    interval: Duration,
    next: Option<Instant>,
}

impl Pacer {
    fn new(fps: u32) -> Pacer {
        Pacer { interval: Duration::from_secs(1) / fps.max(1), next: None }
    }

    fn due(&mut self, now: Instant) -> bool {
        if let Some(next) = self.next
            && now + self.interval / 4 < next
        {
            return false;
        }
        // On schedule: keep the cadence. More than a frame late: restart from now.
        let base = self.next.filter(|&n| now < n + self.interval).unwrap_or(now);
        self.next = Some(base + self.interval);
        true
    }
}

/// Scale, convert, preview, encode and queue captured frames (runs on the capture thread).
struct Pipeline {
    limits: SendLimits,
    screen: bool,
    pacer: Pacer,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe: Arc<AtomicBool>,
    preview: Arc<Mutex<Option<FrameWriter>>>,
    /// The encoder and the last converted picture (send size), from the first frame on.
    enc: Option<(Encoder, I420)>,
    /// A different send size than the encoder's and since when frames have it (`RESIZE_SETTLE`).
    pending_size: Option<((u32, u32), Instant)>,
    last_encoded: Option<Instant>,
    /// `Sending` was dropped: nothing more goes to the transport.
    stopped: Arc<AtomicBool>,
}

impl Pipeline {
    fn new(
        limits: SendLimits,
        kind: StreamKind,
        tx: mpsc::Sender<EncodedFrame>,
        keyframe: Arc<AtomicBool>,
        preview: Arc<Mutex<Option<FrameWriter>>>,
        stopped: Arc<AtomicBool>,
    ) -> Pipeline {
        let screen = kind == StreamKind::Screen;
        Pipeline { limits, screen, pacer: Pacer::new(limits.fps), tx, keyframe, preview, enc: None, pending_size: None, last_encoded: None, stopped }
    }

    /// The transport is gone or the user stopped this capture.
    fn closed(&self) -> bool {
        self.tx.is_closed() || self.stopped.load(Ordering::Acquire)
    }

    /// Whether a frame arriving now is used: paced to `fps` and only while the transport has
    /// room. Otherwise the caller drops it before any copy or conversion.
    fn due(&mut self, now: Instant) -> Result<bool, String> {
        if self.closed() {
            return Err(CLOSED.into());
        }
        Ok(self.tx.capacity() > 0 && self.pacer.due(now))
    }

    /// A frame `due` accepted.
    fn push(&mut self, frame: &RawFrame<'_>, now: Instant) -> Result<(), String> {
        let (w, h) = codec::fit_within(frame.width, frame.height, self.limits.max_width, self.limits.max_height);
        let restart = match &self.enc {
            None => true,
            Some((_, picture)) if picture.dimensions() == (w as usize, h as usize) => {
                self.pending_size = None;
                false
            }
            // A resized window: only once the new size held (a drag-resize changes it per frame).
            Some(_) => {
                let since = self.pending_size.filter(|p| p.0 == (w, h)).map_or(now, |p| p.1);
                self.pending_size = Some(((w, h), since));
                now >= since + RESIZE_SETTLE
            }
        };
        if restart {
            // A new encoder, whose first frame is a keyframe.
            self.enc = Some((Encoder::new(w, h, self.limits, self.screen)?, I420::new(w, h)));
            self.pending_size = None;
        }
        let Some((_, picture)) = &mut self.enc else { return Ok(()) };
        codec::to_i420(frame, picture); // stretched into the running size until a restart
        let (w, h) = picture.dimensions();
        let (w, h) = (w as u32, h as u32);
        if let Some(preview) = lock(&self.preview).as_ref() {
            preview.write(w, h, |rgba| codec::i420_to_rgba(picture, (w, h), rgba));
        }
        self.encode(now)
    }

    /// Screen share between frames: Windows only delivers changes, so on a static screen a
    /// keyframe request (PLI, a new viewer) is answered by re-encoding the last picture.
    fn repeat(&mut self, now: Instant) -> Result<(), String> {
        if self.closed() {
            return Err(CLOSED.into());
        }
        let idle = self.last_encoded.is_some_and(|t| now >= t + self.pacer.interval);
        if idle && self.keyframe.load(Ordering::Acquire) && self.tx.capacity() > 0 { self.encode(now) } else { Ok(()) }
    }

    fn encode(&mut self, now: Instant) -> Result<(), String> {
        let Some((encoder, picture)) = &mut self.enc else {
            return Ok(());
        };
        let keyframe = self.keyframe.swap(false, Ordering::AcqRel);
        self.last_encoded = Some(now);
        let encoded = encoder.encode(picture, keyframe, now)?;
        if self.stopped.load(Ordering::Acquire) {
            return Err(CLOSED.into()); // stopped meanwhile: a frame captured after Stop is never sent
        }
        match encoded {
            Some(frame) => match self.tx.try_send(frame) {
                Ok(()) => {}
                // A lost frame breaks the reference chain: the next one must be a keyframe.
                Err(TrySendError::Full(_)) => self.keyframe.store(true, Ordering::Release),
                Err(TrySendError::Closed(_)) => return Err(CLOSED.into()),
            },
            // Skipped by the rate control: keep the keyframe request for the next frame.
            None if keyframe => self.keyframe.store(true, Ordering::Release),
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames accepted in 10 s from a source at `hz` (alternating ±2 ms jitter) paced to `fps`.
    fn accepted(hz: u32, fps: u32) -> u32 {
        let start = Instant::now();
        let mut pacer = Pacer::new(fps);
        let period = Duration::from_secs(1) / hz;
        let jitter = Duration::from_millis(2);
        (1..hz * 10)
            .filter(|&i| {
                let t = start + period * i;
                pacer.due(if i % 2 == 0 { t + jitter } else { t - jitter })
            })
            .count() as u32
    }

    #[test]
    fn pacing_keeps_the_rate_and_drops_the_rest() {
        assert!((298..=301).contains(&accepted(30, 30)), "jitter must not halve a 30 Hz camera: {}", accepted(30, 30));
        assert!((298..=301).contains(&accepted(60, 30)), "{}", accepted(60, 30));
        assert!((298..=301).contains(&accepted(144, 30)), "{}", accepted(144, 30));
        assert!((248..=250).contains(&accepted(25, 30)), "slower sources pass unchanged: {}", accepted(25, 30));
        assert!((148..=151).contains(&accepted(60, 15)), "{}", accepted(60, 15));
        assert!((9..=11).contains(&accepted(60, 1)), "{}", accepted(60, 1));
    }

    #[test]
    fn pacing_restarts_after_a_stall() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(10);
        assert!(pacer.due(t0));
        assert!(!pacer.due(t0 + Duration::from_millis(50)));
        // 1 s without frames: the next one is due, and so is one 100 ms after it, not a burst.
        assert!(pacer.due(t0 + Duration::from_secs(1)));
        assert!(!pacer.due(t0 + Duration::from_millis(1010)));
        assert!(pacer.due(t0 + Duration::from_millis(1100)));
    }

    #[test]
    fn pipeline_scales_previews_and_answers_keyframe_requests() {
        use crate::model::Id;
        use crate::voice::video::{PixelFormat, VideoFrames, VideoKey};

        let (tx, mut rx) = mpsc::channel(ENCODED_QUEUE);
        let keyframe = Arc::new(AtomicBool::new(false));
        let preview = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut pipe = Pipeline::new(SendLimits::screen(10, 360), StreamKind::Screen, tx, keyframe.clone(), preview.clone(), stopped.clone());
        let frames = VideoFrames::default();
        let me = VideoKey::User(Id(1));
        *lock(&preview) = frames.writer(me.clone());

        let data = vec![200u8; 1280 * 720 * 4];
        let frame = RawFrame { width: 1280, height: 720, stride: 1280 * 4, format: PixelFormat::Bgra, data: &data };
        let t0 = Instant::now();
        assert!(pipe.due(t0).unwrap());
        pipe.push(&frame, t0).unwrap();
        let first = rx.try_recv().unwrap();
        assert!(first.keyframe && first.captured == t0);
        assert_eq!(frames.latest(&me).unwrap().1.size, [640, 360], "scaled into the send box");
        assert!(!pipe.due(t0 + Duration::from_millis(20)).unwrap(), "beyond fps");

        // A keyframe request on a static screen is answered once a frame interval has passed
        // (openh264's rate control may skip a try; the request then stays).
        keyframe.store(true, Ordering::Release);
        pipe.repeat(t0 + Duration::from_millis(50)).unwrap();
        assert!(rx.try_recv().is_err());
        let mut t = t0;
        let repeated = (1..10).find_map(|n| {
            t = t0 + Duration::from_millis(100 * n);
            pipe.repeat(t).unwrap();
            rx.try_recv().ok()
        });
        assert!(repeated.expect("the last picture again").keyframe);
        assert!(!keyframe.load(Ordering::Acquire));
        pipe.repeat(t + Duration::from_secs(1)).unwrap();
        assert!(rx.try_recv().is_err(), "nothing without a request");

        // A full queue: frames are dropped before any work.
        while pipe.tx.try_send(EncodedFrame { annexb: Vec::new(), keyframe: false, captured: t0 }).is_ok() {}
        assert!(!pipe.due(t + Duration::from_secs(2)).unwrap());

        // Stopped by the user (`Sending` dropped): nothing more is handed to the transport.
        while rx.try_recv().is_ok() {}
        stopped.store(true, Ordering::Release);
        assert_eq!(pipe.due(t + Duration::from_secs(3)), Err(CLOSED.to_string()));
        assert_eq!(pipe.push(&frame, t + Duration::from_secs(3)), Err(CLOSED.to_string()));
        assert!(rx.try_recv().is_err(), "the frame captured after Stop is not sent");
        stopped.store(false, Ordering::Release);

        drop(rx);
        assert_eq!(pipe.due(t + Duration::from_secs(3)), Err(CLOSED.to_string()));
        assert_eq!(pipe.repeat(t + Duration::from_secs(3)), Err(CLOSED.to_string()));
    }

    #[test]
    fn a_resized_window_restarts_the_encoder_once_its_size_settles() {
        use crate::voice::video::PixelFormat;

        let (tx, mut rx) = mpsc::channel(ENCODED_QUEUE);
        let mut pipe = Pipeline::new(SendLimits::screen(30, 360), StreamKind::Screen, tx, Arc::default(), Arc::default(), Arc::default());
        let data = vec![120u8; 1280 * 720 * 4]; // uniform: stretching changes nothing, no scene cut
        let frame = |w: u32| RawFrame { width: w, height: 720, stride: w as usize * 4, format: PixelFormat::Bgra, data: &data };
        let size = |pipe: &Pipeline| pipe.enc.as_ref().map(|e| e.1.dimensions());
        let t0 = Instant::now();
        pipe.push(&frame(1280), t0).unwrap();
        assert!(rx.try_recv().unwrap().keyframe);
        assert_eq!(size(&pipe), Some((640, 360)));

        // Dragged wider and narrower, each size held less than RESIZE_SETTLE: same encoder.
        for (i, w) in [1000, 1000, 960, 960, 960].into_iter().enumerate() {
            pipe.push(&frame(w), t0 + Duration::from_millis(100 * (i as u64 + 1))).unwrap();
            assert_eq!(size(&pipe), Some((640, 360)), "{w} at step {i}");
            assert!(rx.try_recv().map_or(true, |f| !f.keyframe), "no new keyframe while resizing");
        }
        // 960 wide since +300 ms: settled at +800 ms, one new encoder at 480×360.
        pipe.push(&frame(960), t0 + Duration::from_millis(800)).unwrap();
        assert_eq!(size(&pipe), Some((480, 360)));
        assert!(rx.try_recv().unwrap().keyframe);
        assert!(pipe.pending_size.is_none());
        // Back and forth to the running size: nothing pending.
        pipe.push(&frame(1280), t0 + Duration::from_millis(900)).unwrap();
        pipe.push(&frame(960), t0 + Duration::from_millis(1000)).unwrap();
        assert!(pipe.pending_size.is_none() && size(&pipe) == Some((480, 360)));
    }

    /// By hand: `cargo test capture -- --ignored`. Lists only; nothing is opened or captured.
    #[test]
    #[ignore = "uses this machine's cameras, monitors and windows"]
    fn lists_real_sources() {
        let cameras = list_cameras().unwrap();
        let (monitors, windows) = list_screen_sources().unwrap();
        assert!(!monitors.is_empty() && monitors.iter().filter(|m| m.primary).count() == 1);
        assert!(monitors.iter().all(|m| !m.name.is_empty() && m.width > 0 && m.height > 0));
        assert!(windows.iter().all(|w| !w.title.trim().is_empty()));
        assert!(cameras.iter().all(|c| !c.id.is_empty() && !c.name.is_empty()));
        println!("{} cameras, {} monitors, {} windows", cameras.len(), monitors.len(), windows.len());
    }

    #[test]
    fn failure_is_reported_only_when_not_stopped_on_purpose() {
        let reported = |stopped: bool, result: Result<(), String>| {
            let (tx, rx) = std_mpsc::channel::<()>();
            let _sending = (!stopped).then_some(tx);
            let mut out = None;
            finish(result, &Stop(rx, Arc::default()), |reason| out = Some(reason));
            out
        };
        assert_eq!(reported(false, Err("Camera unplugged.".into())), Some("Camera unplugged.".into()));
        assert_eq!(reported(true, Err(CLOSED.into())), None, "stopping is not a failure");
        assert_eq!(reported(false, Ok(())), None);
    }
}
