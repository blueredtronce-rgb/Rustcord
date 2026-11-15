//! Camera video, screen share and Go Live (VOICE_NOTES §5, phases D/E). Data flow, threads,
//! buffers, file ownership and the UI hook-up: docs/VIDEO_DESIGN.md.
//!
//! OWNERSHIP: this file (contract owner): the UI contract (`VideoCommand`, `VideoEvent`,
//! `VideoKey`, the source infos, `VideoFrames`), the pipeline/transport types (`RawFrame`,
//! `EncodedFrame`, `SendLimits`, `VideoLink`, `VideoCtl`, `VideoSend`) and the op 12/15 JSON.
//! `codec.rs`, `rtp_h264.rs` + `receive.rs` (+ `voice/transport/video.rs`), `capture.rs` and
//! `stream.rs` have one engineer each; keep their pub signatures.
//!
//! Rules: capture runs only between a user's Start* and the matching Stop (or the call's end, or
//! a capture error); a video frame is never sent unencrypted while DAVE is on
//! (`Dave::encrypt_h264` returns None); only visible videos are decoded; with no video on, no
//! video thread runs and nothing is allocated per packet beyond a header lookup.

pub mod capture;
pub mod codec;
pub mod receive;
pub mod rtp_h264;
pub mod stream;

use crate::model::{Id, null_default};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// H.264 RTP payload type and its RTX payload type (Select Protocol, VOICE_NOTES §2.3, §5.1).
pub const H264_PT: u8 = 103;
pub const H264_RTX_PT: u8 = 104;
/// RTP clock rate of video.
pub const VIDEO_CLOCK_HZ: u32 = 90_000;
/// Encoded frames waiting for the transport. When full the pipeline drops the frame and makes
/// the next one a keyframe (a gap in the reference chain would garble the receivers).
pub const ENCODED_QUEUE: usize = 2;
/// Videos with a frame slot at once: 4 decoded remote videos + my camera preview + my screen
/// share preview (worst case 6 × 8.3 MB; the UI asks for at most 4 remote videos at a time).
pub const MAX_FRAME_SLOTS: usize = 6;
/// Largest frame handed to the UI (≤ 8 MB RGBA per slot); decoders downscale bigger pictures.
pub const MAX_FRAME_SIZE: (u32, u32) = (1920, 1080);

/// UI -> backend, inside `Command::Video`. Every one is a user action; nothing is captured or
/// sent without `StartCamera` / `StartScreenShare`.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoCommand {
    /// Turn my camera on in the connected call. `device` = `CameraInfo::id`, None = the first
    /// camera. Answered with `CameraStarted` or `Error`.
    StartCamera { device: Option<String> },
    /// Answered with `CameraStopped` (nothing when it was off).
    StopCamera,
    /// Answered with `VideoEvent::Cameras`.
    ListCameras,
    /// Go Live in the connected call with a monitor or window from `ListScreenSources`.
    /// `fps` is clamped to 1..=30, `max_height` to 360..=1080 (`SendLimits::screen`). `audio`
    /// (application audio) is not supported yet: `true` is answered with `Error`.
    StartScreenShare { source: ScreenSource, fps: u8, max_height: u32, audio: bool },
    /// Answered with `ScreenShareStopped` (nothing when not sharing).
    StopScreenShare,
    /// Answered with `VideoEvent::Sources`.
    ListScreenSources,
    /// Watch someone's Go Live (key from `stream::stream_key`); I must be in its call. Answered
    /// with `RemoteVideo { key: Stream(..) }` once it connects, or `Error`.
    WatchStream { stream_key: String },
    /// Stop watching (op 19); answered with `RemoteVideoEnded`.
    StopWatching { stream_key: String },
    /// Decode a video only while the UI shows it: `User(id)` = that user's camera (my own id =
    /// my camera preview), `Stream(key)` = a watched Go Live (my own key = my share preview).
    /// Hidden videos cost a header lookup per packet and nothing else.
    SetVideoVisible { key: VideoKey, visible: bool },
}

/// Video -> UI, inside `VoiceEvent::Video`.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoEvent {
    CameraStarted,
    /// Off: by the user, at the end of the call, or after an `Error` (camera unplugged …).
    CameraStopped,
    /// My Go Live is running under this stream key.
    ScreenShareStarted { stream_key: String },
    ScreenShareStopped,
    /// Answer to `ListScreenSources`.
    Sources { monitors: Vec<MonitorInfo>, windows: Vec<WindowInfo> },
    /// Answer to `ListCameras`.
    Cameras(Vec<CameraInfo>),
    /// A video can be shown, or its size changed: someone's camera (op 12) or a watched stream.
    /// `width`/`height` 0 until its first frame is decoded. Frames: `VideoFrames::latest(&key)`.
    RemoteVideo { key: VideoKey, width: u32, height: u32 },
    /// That video is gone (camera off, user left, stream ended, stopped watching); drop its tile
    /// and texture.
    RemoteVideoEnded { key: VideoKey },
    /// User-facing text for a toast (no secrets).
    Error(String),
}

/// A screen-share source from `ListScreenSources`: the raw `HMONITOR` / `HWND` value, valid
/// while that monitor / window exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenSource {
    Monitor(u64),
    Window(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    pub source: ScreenSource,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub source: ScreenSource,
    pub title: String,
    /// Executable name, e.g. "notepad.exe".
    pub process: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraInfo {
    /// Media Foundation symbolic link; pass it back in `StartCamera`.
    pub id: String,
    /// Friendly name for the picker.
    pub name: String,
}

/// Which video a frame slot / event is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VideoKey {
    /// A user's camera in my call (my own id: my camera preview).
    User(Id),
    /// A Go Live stream by stream key (my own key: my screen-share preview).
    Stream(String),
    /// An in-app video (`media::Player`, own `VideoFrames`): hash of its URL's cache key.
    Media(u64),
}

/// Send caps of one video (op 12 `max_*`, encoder settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    /// bit/s
    pub max_bitrate: u32,
}

impl SendLimits {
    /// Camera: 720p30 at 2.5 Mbit/s (VOICE_NOTES §5.2).
    pub const CAMERA: SendLimits = SendLimits { max_width: 1280, max_height: 720, fps: 30, max_bitrate: 2_500_000 };

    /// Screen share: `fps` 1..=30, height 360..=1080 lines in a 16:9 box; 2.5 Mbit/s up to 720p,
    /// 4 Mbit/s above (non-Nitro caps are UNCONFIRMED, VOICE_NOTES §5.4).
    pub fn screen(fps: u8, max_height: u32) -> SendLimits {
        let h = max_height.clamp(360, 1080);
        let max_bitrate = if h > 720 { 4_000_000 } else { 2_500_000 };
        SendLimits { max_width: h * 16 / 9, max_height: h, fps: u32::from(fps.clamp(1, 30)), max_bitrate }
    }
}

/// Pixel layout of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code, reason = "made by the capture sources, Windows only (capture::SUPPORTED)"))]
pub enum PixelFormat {
    /// 4 bytes per pixel: B, G, R, A/X (Windows Graphics Capture, MF RGB32).
    Bgra,
    /// Y plane (`stride` bytes per row), then interleaved U/V rows at half resolution (MF cameras).
    Nv12,
}

/// One captured frame, borrowed from the capture API for the duration of a callback.
#[derive(Debug, Clone, Copy)]
pub struct RawFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of the first plane (≥ width × bytes per pixel).
    pub stride: usize,
    pub format: PixelFormat,
    pub data: &'a [u8],
}

/// One encoded H.264 access unit: Annex-B with 4-byte start codes, SPS/PPS before every IDR.
/// Not yet E2EE-encrypted.
#[derive(Debug)]
pub struct EncodedFrame {
    pub annexb: Vec<u8>,
    pub keyframe: bool,
    /// When it was captured; the 90 kHz RTP timestamp follows it.
    pub captured: Instant,
}

/// Which kind of video a connection sends (Identify / op 12 `streams[].type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// My camera, on the voice connection.
    Camera,
    /// My screen, on a Go Live connection (also used for viewer connections).
    Screen,
}

impl StreamKind {
    pub fn wire(self) -> &'static str {
        match self {
            StreamKind::Camera => "video",
            StreamKind::Screen => "screen",
        }
    }

    /// Identify `streams`: one layer, rid "100" (VOICE_NOTES §2.3; Ready answers with its SSRCs).
    pub fn identify_streams(self) -> Value {
        json!([{"type": self.wire(), "rid": "100", "quality": 100}])
    }
}

/// A running send pipeline, as the transport sees it.
#[derive(Debug)]
pub struct VideoSend {
    pub kind: StreamKind,
    pub limits: SendLimits,
    /// `ENCODED_QUEUE` deep; `None` from `recv` = the pipeline stopped.
    pub frames: mpsc::Receiver<EncodedFrame>,
    /// Set by the transport on a PLI and when sending starts; the encoder makes the next frame
    /// a keyframe and clears it.
    pub keyframe: Arc<AtomicBool>,
}

/// Manager -> transport requests for one connection.
#[derive(Debug)]
pub enum VideoCtl {
    /// Start sending (replacing a running sender): op 12 with Ready's SSRCs and `limits`, then
    /// each frame → `Dave::encrypt_h264` → RTP.
    Send(VideoSend),
    /// Stop sending: op 12 with `video_ssrc: 0` and no streams.
    StopSending,
    /// The wanted set of `VideoLink::rx` changed: send op 15.
    Wants,
}

/// The video half of one transport connection (`ConnectInfo::video`), made by the manager.
pub struct VideoLink {
    /// Identify `streams[].type`: Camera on the voice connection, Screen on Go Live ones.
    pub kind: StreamKind,
    /// Received video; the manager keeps a clone to set what is wanted.
    pub rx: receive::VideoRx,
    pub ctl: mpsc::UnboundedReceiver<VideoCtl>,
}

impl std::fmt::Debug for VideoLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoLink").field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// Op 12 announcing my video, `Some((video_ssrc, rtx_ssrc, kind, limits))` with the SSRCs of
/// Ready's `streams[0]`, or its end (`None`) (VOICE_NOTES §5.2).
pub fn op12_payload(audio_ssrc: u32, video: Option<(u32, u32, StreamKind, SendLimits)>) -> String {
    let d = match video {
        Some((ssrc, rtx, kind, l)) => json!({
            "audio_ssrc": audio_ssrc, "video_ssrc": ssrc, "rtx_ssrc": rtx,
            "streams": [{
                "type": kind.wire(), "rid": "100", "quality": 100, "active": true,
                "max_bitrate": l.max_bitrate, "max_framerate": l.fps,
                "max_resolution": {"type": "fixed", "width": l.max_width, "height": l.max_height},
                "ssrc": ssrc, "rtx_ssrc": rtx,
            }],
        }),
        None => json!({"audio_ssrc": audio_ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []}),
    };
    json!({"op": 12, "d": d}).to_string()
}

/// Someone's video SSRCs from a received op 12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteVideo {
    pub user_id: Id,
    /// 0 = no video.
    pub ssrc: u32,
    /// Retransmissions (PT 104) come on this SSRC; 0 = none.
    pub rtx_ssrc: u32,
    /// False: camera off or paused.
    pub active: bool,
}

/// Received op 12 `d`: `{user_id, audio_ssrc, video_ssrc, streams:[{ssrc, rtx_ssrc, active…}]}`.
/// None without a user id.
pub fn parse_op12(d: &Value) -> Option<RemoteVideo> {
    #[derive(Deserialize)]
    struct Wire {
        user_id: Id,
        #[serde(default, deserialize_with = "null_default")]
        video_ssrc: u32,
        #[serde(default, deserialize_with = "null_default")]
        streams: Vec<StreamWire>,
    }
    #[derive(Deserialize)]
    struct StreamWire {
        #[serde(default, deserialize_with = "null_default")]
        ssrc: u32,
        #[serde(default, deserialize_with = "null_default")]
        rtx_ssrc: u32,
        #[serde(default)]
        active: Option<bool>,
    }
    let w = Wire::deserialize(d).ok().filter(|w| !w.user_id.is_zero())?;
    // The stream of `video_ssrc`, else the first one with an SSRC (some payloads omit video_ssrc).
    let stream = w.streams.iter().find(|s| s.ssrc != 0 && (w.video_ssrc == 0 || s.ssrc == w.video_ssrc));
    let ssrc = if w.video_ssrc != 0 { w.video_ssrc } else { stream.map_or(0, |s| s.ssrc) };
    Some(RemoteVideo {
        user_id: w.user_id,
        ssrc,
        rtx_ssrc: stream.map_or(0, |s| s.rtx_ssrc),
        active: ssrc != 0 && stream.is_none_or(|s| s.active != Some(false)),
    })
}

/// Op 15 Media Sink Wants: full quality for each wanted video SSRC, nothing for any other
/// (`"any": 0`; the semantics of `any` are UNCONFIRMED, VOICE_NOTES §5.2).
pub fn op15_payload(wanted: &[u32]) -> String {
    let mut d = serde_json::Map::new();
    d.insert("any".into(), json!(0));
    for ssrc in wanted {
        d.insert(ssrc.to_string(), json!(100));
    }
    json!({"op": 15, "d": d}).to_string()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The latest frame of every shown video, shared with the UI (`BackendHandle::video`). One
/// reusable buffer per video: writers (decoder, preview) overwrite it in place and wake the UI;
/// the UI uploads it when `generation` changed. Bounded: `MAX_FRAME_SLOTS` slots of at most
/// `MAX_FRAME_SIZE` each. Frames never travel through the event channel.
#[derive(Default)]
pub struct VideoFrames {
    ctx: egui::Context,
    slots: Mutex<HashMap<VideoKey, Arc<Mutex<VideoFrame>>>>,
}

/// One video's latest frame: opaque RGBA (alpha 255, so premultiplied = straight).
#[derive(Default)]
pub struct VideoFrame {
    pub image: Arc<egui::ColorImage>,
    /// +1 per written frame; 0 = nothing written yet.
    pub generation: u64,
}

impl VideoFrames {
    pub fn new(ctx: egui::Context) -> VideoFrames {
        VideoFrames { ctx, slots: Mutex::default() }
    }

    /// For the UI, per shown tile and paint: `(generation, image)`, or None before the first
    /// frame or while a writer is busy (keep the old texture; the writer wakes the UI after).
    /// Cheap (clones an `Arc`); when `generation` changed: `texture.set(image, options)`, which
    /// takes the `Arc` without copying the pixels.
    pub fn latest(&self, key: &VideoKey) -> Option<(u64, Arc<egui::ColorImage>)> {
        let slot = lock(&self.slots).get(key)?.clone();
        let frame = slot.try_lock().ok()?;
        (frame.generation > 0).then(|| (frame.generation, frame.image.clone()))
    }

    /// For a decoder or preview: the slot of `key`, created on first use. None when
    /// `MAX_FRAME_SLOTS` other videos already have one.
    pub fn writer(&self, key: VideoKey) -> Option<FrameWriter> {
        let mut slots = lock(&self.slots);
        if !slots.contains_key(&key) && slots.len() >= MAX_FRAME_SLOTS {
            return None;
        }
        Some(FrameWriter { ctx: self.ctx.clone(), slot: slots.entry(key).or_default().clone() })
    }

    /// The video ended or was hidden: frees its frame (a writer that still holds the slot
    /// writes into nothing until it is dropped).
    pub fn remove(&self, key: &VideoKey) {
        lock(&self.slots).remove(key);
    }
}

/// Writes one video's frames (any thread).
pub struct FrameWriter {
    ctx: egui::Context,
    slot: Arc<Mutex<VideoFrame>>,
}

impl FrameWriter {
    /// Replace the frame and wake the UI. `fill` gets the `width × height × 4` RGBA bytes (the
    /// previous frame's, reused) and must write every pixel with alpha 255. The buffer is only
    /// reallocated on a size change or while the UI still holds the previous image. Returns
    /// false (nothing written) for an empty size or one beyond `MAX_FRAME_SIZE`.
    pub fn write(&self, width: u32, height: u32, fill: impl FnOnce(&mut [u8])) -> bool {
        if width == 0 || height == 0 || width > MAX_FRAME_SIZE.0 || height > MAX_FRAME_SIZE.1 {
            return false;
        }
        {
            let mut frame = lock(&self.slot);
            let size = [width as usize, height as usize];
            if frame.image.size != size {
                frame.image = Arc::new(egui::ColorImage::filled(size, egui::Color32::BLACK));
            }
            fill(Arc::make_mut(&mut frame.image).as_raw_mut());
            frame.generation += 1;
        }
        // One paint per frame: egui answers a zero-delay request with two (a 30 fps video
        // painted 60 times a second); a pending earlier wake absorbs this one.
        self.ctx.request_repaint_after_for(Duration::from_millis(1), egui::ViewportId::ROOT);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: String) -> Value {
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn frames_are_written_in_place_and_read_by_generation() {
        let frames = VideoFrames::default();
        let key = VideoKey::User(Id(5));
        assert!(frames.latest(&key).is_none());
        let w = frames.writer(key.clone()).unwrap();
        assert!(frames.latest(&key).is_none(), "nothing written yet");
        assert!(w.write(2, 1, |px| px.copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255])));
        let (generation, image) = frames.latest(&key).unwrap();
        assert_eq!((generation, image.size), (1, [2, 1]));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(4, 5, 6));

        // The UI still holds frame 1: frame 2 goes into a copy, frame 1 stays intact.
        w.write(2, 1, |px| px[0] = 9);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(1, 2, 3));
        let (generation, image2) = frames.latest(&key).unwrap();
        assert_eq!((generation, image2.pixels[0].r(), image2.pixels[1].b()), (2, 9, 6), "starts from the previous frame");
        // Once the UI let go, the same buffer is reused.
        let ptr = image2.pixels.as_ptr();
        drop((image, image2));
        w.write(2, 1, |_| {});
        assert_eq!(frames.latest(&key).unwrap().1.pixels.as_ptr(), ptr);
        // A size change reallocates.
        w.write(1, 1, |px| px.copy_from_slice(&[7, 7, 7, 255]));
        assert_eq!(frames.latest(&key).unwrap().1.size, [1, 1]);

        assert!(!w.write(0, 1, |_| panic!("not called")));
        assert!(!w.write(MAX_FRAME_SIZE.0 + 2, 2, |_| panic!("not called")), "oversized frames are refused");
        assert_eq!(frames.latest(&key).unwrap().0, 4);

        let _busy = w.slot.lock().unwrap();
        assert!(frames.latest(&key).is_none(), "the UI never waits for a writer");
    }

    #[test]
    fn a_frame_wakes_one_paint() {
        let ctx = egui::Context::default();
        let w = VideoFrames::new(ctx.clone()).writer(VideoKey::User(Id(1))).unwrap();
        let pass = || ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        for _ in 0..3 {
            pass();
        }
        assert!(!ctx.has_requested_repaint(), "settled");
        w.write(2, 2, |_| {});
        assert!(ctx.has_requested_repaint());
        pass();
        assert!(!ctx.has_requested_repaint(), "no second paint for the same frame");
    }

    #[test]
    fn frame_slots_are_bounded() {
        let frames = VideoFrames::default();
        let writers: Vec<_> = (0..MAX_FRAME_SLOTS as u64).map(|i| frames.writer(VideoKey::User(Id(i))).unwrap()).collect();
        assert!(frames.writer(VideoKey::Stream("call:1:2".into())).is_none());
        assert!(frames.writer(VideoKey::User(Id(0))).is_some(), "an existing slot is shared");
        frames.remove(&VideoKey::User(Id(0)));
        writers[0].write(1, 1, |_| {});
        assert!(frames.latest(&VideoKey::User(Id(0))).is_none(), "a removed slot is gone for the UI");
        assert!(frames.writer(VideoKey::Stream("call:1:2".into())).is_some());
    }

    #[test]
    fn screen_limits_are_clamped() {
        assert_eq!(SendLimits::screen(60, 4000), SendLimits { max_width: 1920, max_height: 1080, fps: 30, max_bitrate: 4_000_000 });
        assert_eq!(SendLimits::screen(0, 100), SendLimits { max_width: 640, max_height: 360, fps: 1, max_bitrate: 2_500_000 });
        assert_eq!(SendLimits::screen(15, 720).max_width, 1280);
    }

    #[test]
    fn op12_and_op15_payloads() {
        assert_eq!(StreamKind::Screen.identify_streams(), json!([{"type":"screen","rid":"100","quality":100}]));
        let start = parse(op12_payload(1, Some((2, 3, StreamKind::Camera, SendLimits::CAMERA))));
        assert_eq!(
            start,
            json!({"op":12,"d":{"audio_ssrc":1,"video_ssrc":2,"rtx_ssrc":3,"streams":[{
                "type":"video","rid":"100","quality":100,"active":true,"max_bitrate":2_500_000,"max_framerate":30,
                "max_resolution":{"type":"fixed","width":1280,"height":720},"ssrc":2,"rtx_ssrc":3}]}})
        );
        assert_eq!(parse(op12_payload(1, None)), json!({"op":12,"d":{"audio_ssrc":1,"video_ssrc":0,"rtx_ssrc":0,"streams":[]}}));
        assert_eq!(parse(op15_payload(&[7, 9])), json!({"op":15,"d":{"any":0,"7":100,"9":100}}));
        assert_eq!(parse(op15_payload(&[])), json!({"op":15,"d":{"any":0}}));
    }

    #[test]
    fn received_op12() {
        let on = json!({"user_id":"5","audio_ssrc":1,"video_ssrc":2,"streams":[{"ssrc":2,"rtx_ssrc":3,"rid":"100","active":true}]});
        assert_eq!(parse_op12(&on), Some(RemoteVideo { user_id: Id(5), ssrc: 2, rtx_ssrc: 3, active: true }));
        let off = json!({"user_id":"5","audio_ssrc":1,"video_ssrc":0,"streams":[]});
        assert_eq!(parse_op12(&off), Some(RemoteVideo { user_id: Id(5), ssrc: 0, rtx_ssrc: 0, active: false }));
        let paused = json!({"user_id":"5","video_ssrc":2,"streams":[{"ssrc":2,"rtx_ssrc":3,"active":false}]});
        assert!(!parse_op12(&paused).unwrap().active);
        let streams_only = json!({"user_id":"5","video_ssrc":null,"streams":[{"ssrc":0},{"ssrc":8,"rtx_ssrc":9}]});
        assert_eq!(parse_op12(&streams_only), Some(RemoteVideo { user_id: Id(5), ssrc: 8, rtx_ssrc: 9, active: true }));
        assert_eq!(parse_op12(&json!({"video_ssrc":2})), None);
        assert_eq!(parse_op12(&json!(null)), None);
    }
}
