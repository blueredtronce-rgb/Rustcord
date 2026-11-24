//! The video side of the voice task and Go Live (VOICE_NOTES §5.2, §5.4): camera on/off (capture
//! → `VideoCtl::Send` on the voice connection, op 4 `self_video`), screen share (op 18 →
//! STREAM_CREATE + STREAM_SERVER_UPDATE → a second `transport::run` with the stream token, DAVE
//! group `rtc_server_id - 1`, `StreamKind::Screen`), watching (op 20 → a viewer connection, op 19
//! to stop), visibility (`VideoRx::set_wanted` → `VideoCtl::Wants`).
//!
//! OWNERSHIP: Go Live / signalling engineer. The manager (voice/mod.rs) calls `Video`'s methods
//! on the voice task; keep their signatures. Add `StreamMsg` variants freely.
//!
//! Policy: everything starts with a user command and ends with its Stop, the call's end, a
//! STREAM_DELETE, a capture error or a lost Go Live connection; nothing reconnects on its own.
//! Capture starts only once the connection that sends it is up. A Go Live that is not connected
//! `CONNECT_TIMEOUT` after op 18/20 (or after a new stream server) ends with an error.

use super::capture::{self, Sending, Source};
use super::receive::VideoRx;
use super::{FrameWriter, MAX_FRAME_SLOTS, ScreenSource, SendLimits, StreamKind, VideoCommand, VideoCtl, VideoEvent, VideoFrames, VideoKey, VideoLink};
use crate::auth::Secret;
use crate::gateway::{GatewayCommand, StreamOp};
use crate::model::{GatewayEvent, Id};
use crate::voice::audio::{Captured, MediaLink, StreamGain};
use crate::voice::{Msg, VoiceEvent, VoiceEvents, transport};
use futures_util::FutureExt;
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// From op 18/20 (or a new stream server) until the Go Live connection is up (the call view
/// forgets a "Watch Stream" click after it).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Watched streams at once: one connection each, and 4 remote frame slots.
const MAX_WATCHED: usize = 4;
/// Remote videos shown (decoded) at once: every frame slot but my two previews'. More are refused.
const MAX_REMOTE_SHOWN: usize = MAX_FRAME_SLOTS - 2;
const NOT_IN_CALL: &str = "Join a voice channel first.";

/// Go Live stream key: `guild:{guild_id}:{channel_id}:{owner}`, or `call:{channel_id}:{owner}`
/// for DM/group calls (VOICE_NOTES §5.4). The UI builds watch keys with it from voice states
/// with `self_stream`.
pub fn stream_key(guild_id: Option<Id>, channel_id: Id, owner: Id) -> String {
    match guild_id {
        Some(g) => format!("guild:{g}:{channel_id}:{owner}"),
        None => format!("call:{channel_id}:{owner}"),
    }
}

/// `(guild_id, channel_id, owner)` of a guild or call stream key; None for anything else.
pub fn parse_stream_key(key: &str) -> Option<(Option<Id>, Id, Id)> {
    let id = |s: &str| s.parse().ok().filter(|&n| n != 0).map(Id);
    match key.split(':').collect::<Vec<_>>()[..] {
        ["guild", g, c, o] => Some((Some(id(g)?), id(c)?, id(o)?)),
        ["call", c, o] => Some((None, id(c)?, id(o)?)),
        _ => None,
    }
}

/// What `Video` gets to know about the voice task with each call.
pub(in crate::voice) struct Ctx {
    /// Ops 18–22 and op 4 go here; None while logged out.
    pub gateway: Option<mpsc::UnboundedSender<GatewayCommand>>,
    pub me: Id,
    /// Main-gateway session id (Go Live Identify uses it).
    pub session_id: Option<String>,
    /// The connected call `(guild_id, channel_id)`; None while joining or out of voice.
    pub call: Option<(Option<Id>, Id)>,
    /// The connected call's playback (Some with `call`): the `MediaLink::playback` of Go Live
    /// connections, so stream audio plays in the call's mix. Their `captured` is a channel whose
    /// sender `Video` keeps (nothing to send; the transport's keepalive keeps running).
    pub playback: Option<crate::voice::audio::Playback>,
}

/// Posted back to the voice task by tasks and threads `Video` started (`Msg::Stream`). `id`
/// names the Go Live attempt or capture; messages of replaced ones are ignored.
pub(in crate::voice) enum StreamMsg {
    /// A Go Live transport connection (mine or a watched one) is up.
    Connected { stream_key: String, id: u64 },
    /// A Go Live transport connection (mine or a watched one) ended.
    Ended { stream_key: String, id: u64, end: transport::End },
    /// `CONNECT_TIMEOUT` after the attempt `id` started.
    Timeout { stream_key: String, id: u64 },
    /// A capture stopped by itself (`capture::start`'s `failed`).
    CaptureFailed { kind: StreamKind, id: u64, reason: String },
}

/// Attempt and capture ids, unique for the process.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn post(tx: &mpsc::WeakUnboundedSender<Msg>, msg: StreamMsg) {
    if let Some(tx) = tx.upgrade() {
        let _ = tx.send(Msg::Stream(msg));
    }
}

/// A running capture; dropping it releases the camera / ends the screen capture.
struct Capture {
    sending: Sending,
    id: u64,
}

/// One Go Live (my screen share or a watched stream), from op 18/20 until it ends.
struct GoLive {
    key: String,
    gain: StreamGain,
    /// The current attempt (`StreamMsg` ids).
    id: u64,
    /// STREAM_CREATE `(rtc_server_id, rtc_channel_id)`.
    server: Option<(Id, Option<Id>)>,
    /// STREAM_SERVER_UPDATE `(endpoint, token)`, consumed by the next connect.
    endpoint: Option<(String, Secret)>,
    conn: Option<StreamConn>,
}

/// A running Go Live connection. Dropping it stops the transport and my screen capture.
struct StreamConn {
    _stop: oneshot::Sender<()>,
    /// Kept open and unused: no application audio is sent.
    _audio: mpsc::Sender<Captured>,
    ctl: mpsc::UnboundedSender<VideoCtl>,
    rx: VideoRx,
    connected: bool,
    /// My screen (my own share, once connected).
    capture: Option<Capture>,
}

impl Drop for StreamConn {
    /// The closing connection decodes and reports nothing more.
    fn drop(&mut self) {
        self.rx.end();
    }
}

impl GoLive {
    fn new(key: String, tx: &mpsc::WeakUnboundedSender<Msg>) -> GoLive {
        let mut live = GoLive { key, gain: StreamGain::default(), id: 0, server: None, endpoint: None, conn: None };
        live.restart(tx);
        live
    }

    /// A new attempt: drops the running connection; it must connect within `CONNECT_TIMEOUT`.
    fn restart(&mut self, tx: &mpsc::WeakUnboundedSender<Msg>) {
        (self.id, self.conn) = (next_id(), None);
        let (tx, stream_key, id) = (tx.clone(), self.key.clone(), self.id);
        tokio::spawn(async move {
            tokio::time::sleep(CONNECT_TIMEOUT).await;
            post(&tx, StreamMsg::Timeout { stream_key, id });
        });
    }

    fn connected(&self) -> bool {
        self.conn.as_ref().is_some_and(|c| c.connected)
    }

    /// Stream connection parameters once STREAM_CREATE and STREAM_SERVER_UPDATE arrived (any
    /// order); takes the token (one connect each). No `rtc_channel_id`: the key's channel.
    fn connect_info(&mut self, me: Id, session_id: String) -> Option<transport::ConnectInfo> {
        let (server_id, rtc_channel_id) = self.server?;
        let channel_id = rtc_channel_id.or_else(|| parse_stream_key(&self.key).map(|k| k.1))?;
        let (endpoint, token) = self.endpoint.take()?;
        let dave_group = Id(server_id.0.saturating_sub(1));
        Some(transport::ConnectInfo { server_id, channel_id, user_id: me, session_id, token, endpoint, dave_group, video: None })
    }
}

/// My screen share: its Go Live and what to capture.
struct Share {
    live: GoLive,
    source: ScreenSource,
    limits: SendLimits,
}

/// Toast text for a STREAM_DELETE reason; None when I asked for it.
fn delete_text(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("user_requested") => None,
        Some("stream_full") => Some("This stream is full."),
        Some("unauthorized") => Some("You don't have permission to stream or watch here."),
        Some("safety_guild_rate_limited") => Some("Streaming is rate limited right now. Try again later."),
        _ => Some("The stream has ended."),
    }
}

fn end_text(end: transport::End) -> String {
    match end {
        transport::End::Failed(reason) => reason,
        _ => "The stream connection was lost.".into(),
    }
}

/// Runs one Go Live transport connection; posts `Connected` and `Ended` with `id`.
fn spawn(tx: mpsc::WeakUnboundedSender<Msg>, info: transport::ConnectInfo, media: MediaLink, stream_key: String, id: u64) -> oneshot::Sender<()> {
    let (stop_tx, stop) = oneshot::channel();
    tokio::spawn(async move {
        let (notify, key) = (tx.clone(), stream_key.clone());
        let connected = move || post(&notify, StreamMsg::Connected { stream_key: key, id });
        // A panic must end the stream, not leave it hanging (like the call's connection).
        let end = AssertUnwindSafe(transport::run(info, media, stop, connected))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| transport::End::Failed("The stream stopped after an internal error.".into()));
        post(&tx, StreamMsg::Ended { stream_key, id, end });
    });
    stop_tx
}

/// Video state of the voice task: my camera / screen share, watched streams, Go Live
/// connections, the voice connection's video link.
pub(in crate::voice) struct Video {
    events: VoiceEvents,
    frames: Arc<VideoFrames>,
    /// For tasks that report back (`Msg::Stream`); weak like the manager's own.
    tx: mpsc::WeakUnboundedSender<Msg>,
    /// The voice connection's link: requests to its transport and its receive side.
    call: Option<(mpsc::UnboundedSender<VideoCtl>, VideoRx)>,
    camera: Option<Capture>,
    share: Option<Share>,
    watching: Vec<GoLive>,
    /// Videos the UI shows (`SetVideoVisible`), my previews included; until the call ends.
    shown: HashSet<VideoKey>,
}

impl Video {
    pub(in crate::voice) fn new(events: VoiceEvents, frames: Arc<VideoFrames>, tx: mpsc::WeakUnboundedSender<Msg>) -> Video {
        Video { events, frames, tx, call: None, camera: None, share: None, watching: Vec::new(), shown: HashSet::new() }
    }

    /// Op 4 `self_video`: my camera is on (or starting).
    pub(in crate::voice) fn camera_on(&self) -> bool {
        self.camera.is_some()
    }

    /// A user command (`Command::Video`). Camera, screen share and watching need `ctx.call`.
    pub(in crate::voice) fn command(&mut self, cmd: VideoCommand, ctx: Ctx) {
        match cmd {
            VideoCommand::StartCamera { device } => self.start_camera(device, &ctx),
            VideoCommand::StopCamera => self.stop_camera(ctx.me, None),
            VideoCommand::ListCameras => self.list(|| capture::list_cameras().map(VideoEvent::Cameras)),
            VideoCommand::ListScreenSources => {
                self.list(|| capture::list_screen_sources().map(|(monitors, windows)| VideoEvent::Sources { monitors, windows }));
            }
            VideoCommand::StartScreenShare { source, fps, max_height, audio } => {
                self.start_share(source, SendLimits::screen(fps, max_height), audio, &ctx);
            }
            VideoCommand::StopScreenShare => {
                if let Some(key) = self.share.as_ref().map(|s| s.live.key.clone()) {
                    self.end_live(&key, &ctx, None, true);
                }
            }
            VideoCommand::WatchStream { stream_key } => self.watch(stream_key, &ctx),
            VideoCommand::SetStreamVolume { stream_key, percent } => {
                if let Some(live) = self.watching.iter().find(|live| live.key == stream_key) {
                    live.gain.set(percent);
                }
            }
            VideoCommand::StopWatching { stream_key } => {
                if self.watching.iter().any(|l| l.key == stream_key) {
                    self.end_live(&stream_key, &ctx, None, true);
                }
            }
            VideoCommand::SetVideoVisible { key, visible } => self.set_visible(key, visible, ctx.me),
        }
    }

    /// STREAM_CREATE / STREAM_SERVER_UPDATE / STREAM_DELETE (only the voice task sees them: the
    /// server update carries a token). Keys that are not mine or watched are ignored.
    pub(in crate::voice) fn on_gateway(&mut self, ev: GatewayEvent, ctx: Ctx) {
        match ev {
            GatewayEvent::StreamCreate { stream_key, rtc_server_id, rtc_channel_id, .. } => {
                let Some(live) = self.live_mut(&stream_key) else { return };
                live.server = Some((rtc_server_id, rtc_channel_id));
                self.try_connect(&stream_key, &ctx);
            }
            GatewayEvent::StreamServerUpdate { stream_key, endpoint, token } => {
                let tx = self.tx.clone();
                let Some(live) = self.live_mut(&stream_key) else { return };
                match endpoint {
                    Some(endpoint) => {
                        live.endpoint = Some((endpoint, token));
                        self.try_connect(&stream_key, &ctx);
                    }
                    // Reallocating: drop the connection and wait for the next server.
                    None => live.restart(&tx),
                }
            }
            // The server ended it already: no op 19.
            GatewayEvent::StreamDelete { stream_key, reason, .. } => {
                let text = delete_text(reason.as_deref()).map(String::from);
                self.end_live(&stream_key, &ctx, text, false);
            }
            _ => {}
        }
    }

    pub(in crate::voice) fn on_msg(&mut self, msg: StreamMsg, ctx: Ctx) {
        match msg {
            StreamMsg::Connected { stream_key, id } => {
                let Some(conn) = self.live_mut(&stream_key).filter(|l| l.id == id).and_then(|l| l.conn.as_mut()) else { return };
                conn.connected = true;
                if self.is_share(&stream_key) {
                    self.start_capture(&ctx);
                } else {
                    self.send(VideoEvent::RemoteVideo { key: VideoKey::Stream(stream_key), width: 0, height: 0 });
                }
            }
            StreamMsg::Ended { stream_key, id, end } => {
                if self.live_mut(&stream_key).is_some_and(|l| l.id == id) {
                    self.end_live(&stream_key, &ctx, Some(end_text(end)), true);
                }
            }
            StreamMsg::Timeout { stream_key, id } => {
                if self.live_mut(&stream_key).is_some_and(|l| l.id == id && !l.connected()) {
                    let text = if self.is_share(&stream_key) { "Could not start the stream." } else { "Could not connect to the stream." };
                    self.end_live(&stream_key, &ctx, Some(text.into()), true);
                }
            }
            StreamMsg::CaptureFailed { kind: StreamKind::Camera, id, reason } => {
                if self.camera.as_ref().is_some_and(|c| c.id == id) {
                    self.stop_camera(ctx.me, Some(reason));
                }
            }
            StreamMsg::CaptureFailed { kind: StreamKind::Screen, id, reason } => {
                let capturing = |s: &&Share| s.live.conn.as_ref().and_then(|c| c.capture.as_ref()).is_some_and(|c| c.id == id);
                if let Some(key) = self.share.as_ref().filter(capturing).map(|s| s.live.key.clone()) {
                    self.end_live(&key, &ctx, Some(reason), true);
                }
            }
        }
    }

    /// A voice connection is starting: its video link (`ConnectInfo::video`), replacing the
    /// previous connection's.
    pub(in crate::voice) fn link_for_call(&mut self) -> VideoLink {
        let (ctl_tx, ctl) = mpsc::unbounded_channel();
        let rx = VideoRx::new(self.frames.clone(), self.events.clone(), None);
        self.call = Some((ctl_tx, rx.clone()));
        VideoLink { kind: StreamKind::Camera, rx, ctl }
    }

    /// The voice connection is gone (leave, kick, move, new token, logout): stop the camera,
    /// screen share and watching (op 19s while `ctx.gateway` is there), tell the UI.
    pub(in crate::voice) fn call_ended(&mut self, ctx: Ctx) {
        self.stop_camera(ctx.me, None);
        let keys: Vec<String> = self.share.iter().map(|s| &s.live).chain(&self.watching).map(|l| l.key.clone()).collect();
        for key in keys {
            self.end_live(&key, &ctx, None, true);
        }
        // Cameras of that connection are gone; the next connection announces its own.
        for key in std::mem::take(&mut self.shown) {
            if matches!(key, VideoKey::User(id) if id != ctx.me) {
                self.frames.remove(&key);
                self.send(VideoEvent::RemoteVideoEnded { key });
            }
        }
        // Every camera that connection announced ends too; it stays silent while it closes.
        if let Some((_, rx)) = self.call.take() {
            rx.end();
        }
    }

    fn send(&self, ev: VideoEvent) {
        self.events.send(VoiceEvent::Video(ev));
    }

    fn error(&self, text: impl Into<String>) {
        self.send(VideoEvent::Error(text.into()));
    }

    /// Device / source listing: blocking, so off the voice task.
    fn list(&self, f: impl FnOnce() -> Result<VideoEvent, String> + Send + 'static) {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || events.send(VoiceEvent::Video(f().unwrap_or_else(VideoEvent::Error))));
    }

    fn is_share(&self, key: &str) -> bool {
        self.share.as_ref().is_some_and(|s| s.live.key == key)
    }

    fn live_mut(&mut self, key: &str) -> Option<&mut GoLive> {
        match &mut self.share {
            Some(s) if s.live.key == key => Some(&mut s.live),
            _ => self.watching.iter_mut().find(|l| l.key == key),
        }
    }

    /// My preview's writer while the UI shows `key`.
    fn preview(&self, key: &VideoKey) -> Option<FrameWriter> {
        self.shown.contains(key).then(|| self.frames.writer(key.clone())).flatten()
    }

    fn start_camera(&mut self, device: Option<String>, ctx: &Ctx) {
        let Some(ctl) = self.call.as_ref().filter(|_| ctx.call.is_some()).map(|c| c.0.clone()) else { return self.error(NOT_IN_CALL) };
        self.stop_camera(ctx.me, None); // another device: off, then on
        let (id, tx) = (next_id(), self.tx.clone());
        let failed = move |reason: String| post(&tx, StreamMsg::CaptureFailed { kind: StreamKind::Camera, id, reason });
        match capture::start(Source::Camera { device }, SendLimits::CAMERA, failed) {
            Ok((sending, send)) => {
                let _ = ctl.send(VideoCtl::Send(send));
                sending.set_preview(self.preview(&VideoKey::User(ctx.me)));
                self.camera = Some(Capture { sending, id });
                self.send(VideoEvent::CameraStarted);
            }
            Err(e) => self.error(e),
        }
    }

    fn stop_camera(&mut self, me: Id, error: Option<String>) {
        if self.camera.take().is_none() {
            return;
        }
        if let Some((ctl, _)) = &self.call {
            let _ = ctl.send(VideoCtl::StopSending);
        }
        self.frames.remove(&VideoKey::User(me));
        if let Some(e) = error {
            self.error(e);
        }
        self.send(VideoEvent::CameraStopped);
    }

    fn start_share(&mut self, source: ScreenSource, limits: SendLimits, audio: bool, ctx: &Ctx) {
        if audio {
            return self.error("Sharing application audio is not supported yet.");
        }
        let (Some(gw), Some((guild_id, channel_id))) = (&ctx.gateway, ctx.call) else { return self.error(NOT_IN_CALL) };
        if let Some(share) = &mut self.share {
            // Another source while sharing: same stream, new capture.
            (share.source, share.limits) = (source, limits);
            return self.start_capture(ctx);
        }
        let key = stream_key(guild_id, channel_id, ctx.me);
        let _ = gw.send(GatewayCommand::Stream(StreamOp::Create { guild_id, channel_id }));
        // Other clients follow op 18 with "not paused" (whether it is needed is UNCONFIRMED).
        let _ = gw.send(GatewayCommand::Stream(StreamOp::SetPaused { stream_key: key.clone(), paused: false }));
        self.share = Some(Share { live: GoLive::new(key, &self.tx), source, limits });
    }

    /// My share's connection is up or its source changed: (re)start the screen capture on it.
    fn start_capture(&mut self, ctx: &Ctx) {
        let Some(key) = self.share.as_ref().filter(|s| s.live.connected()).map(|s| s.live.key.clone()) else { return };
        let preview = self.preview(&VideoKey::Stream(key.clone()));
        let (id, tx) = (next_id(), self.tx.clone());
        let failed = move |reason: String| post(&tx, StreamMsg::CaptureFailed { kind: StreamKind::Screen, id, reason });
        let Some(Share { live: GoLive { conn: Some(conn), .. }, source, limits }) = &mut self.share else { return };
        conn.capture = None; // a new source replaces the running capture
        match capture::start(Source::Screen(*source), *limits, failed) {
            Ok((sending, send)) => {
                let _ = conn.ctl.send(VideoCtl::Send(send));
                sending.set_preview(preview);
                conn.capture = Some(Capture { sending, id });
                self.send(VideoEvent::ScreenShareStarted { stream_key: key });
            }
            Err(e) => self.end_live(&key, ctx, Some(e), true),
        }
    }

    fn watch(&mut self, stream_key: String, ctx: &Ctx) {
        let Some(gw) = ctx.gateway.as_ref().filter(|_| ctx.call.is_some()) else { return self.error(NOT_IN_CALL) };
        let in_call = parse_stream_key(&stream_key).is_some_and(|(g, c, owner)| Some((g, c)) == ctx.call && owner != ctx.me);
        if !in_call {
            return self.error("You can only watch streams of others in your call.");
        }
        if self.watching.iter().any(|l| l.key == stream_key) {
            return;
        }
        if self.watching.len() >= MAX_WATCHED {
            return self.error(format!("You can watch at most {MAX_WATCHED} streams at once."));
        }
        let _ = gw.send(GatewayCommand::Stream(StreamOp::Watch { stream_key: stream_key.clone() }));
        self.watching.push(GoLive::new(stream_key, &self.tx));
    }

    /// Connects a Go Live once both STREAM_* dispatches arrived; a new stream server replaces
    /// the running connection (and my capture on it, restarted once connected).
    fn try_connect(&mut self, key: &str, ctx: &Ctx) {
        let (Some(session_id), Some(playback)) = (ctx.session_id.clone(), ctx.playback.clone()) else { return };
        let vkey = VideoKey::Stream(key.to_owned());
        let decode = !self.is_share(key) && self.shown.contains(&vkey);
        let (tx, frames, events) = (self.tx.clone(), self.frames.clone(), self.events.clone());
        let Some(live) = self.live_mut(key) else { return };
        let Some(mut info) = live.connect_info(ctx.me, session_id) else { return };
        live.restart(&tx);
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let rx = VideoRx::new(frames, events, Some(key.to_owned()));
        if decode && rx.set_wanted(&vkey, true) {
            let _ = ctl.send(VideoCtl::Wants);
        }
        info.video = Some(VideoLink { kind: StreamKind::Screen, rx: rx.clone(), ctl: ctl_rx });
        let (audio, captured) = mpsc::channel(1);
        let playback = playback.for_stream(live.id, live.gain.clone());
        let stop = spawn(tx, info, MediaLink { captured, playback }, key.to_owned(), live.id);
        live.conn = Some(StreamConn { _stop: stop, _audio: audio, ctl, rx, connected: false, capture: None });
    }

    /// Ends my share or a watched stream: op 19 (unless the server ended it), stops its
    /// connection and capture, frees its frame, tells the UI.
    fn end_live(&mut self, key: &str, ctx: &Ctx, error: Option<String>, op19: bool) {
        let mine = if self.is_share(key) {
            self.share = None;
            true
        } else if let Some(i) = self.watching.iter().position(|l| l.key == key) {
            self.watching.remove(i).gain.set(0);
            false
        } else {
            return;
        };
        if let Some(gw) = ctx.gateway.as_ref().filter(|_| op19) {
            let _ = gw.send(GatewayCommand::Stream(StreamOp::Delete { stream_key: key.into() }));
        }
        let vkey = VideoKey::Stream(key.into());
        self.frames.remove(&vkey);
        self.shown.remove(&vkey);
        if let Some(e) = error {
            self.error(e);
        }
        self.send(if mine { VideoEvent::ScreenShareStopped } else { VideoEvent::RemoteVideoEnded { key: vkey } });
    }

    /// Someone else's camera or stream (not one of my previews).
    fn is_remote(&self, key: &VideoKey, me: Id) -> bool {
        match key {
            VideoKey::User(id) => *id != me,
            VideoKey::Stream(k) => !self.is_share(k),
            // In-app videos never reach the voice manager.
            VideoKey::Media(_) => false,
        }
    }

    /// Decode a remote video only while shown (at most `MAX_REMOTE_SHOWN`); my own keys switch
    /// my preview.
    fn set_visible(&mut self, key: VideoKey, on: bool, me: Id) {
        if !on {
            self.shown.remove(&key);
        } else if !self.shown.contains(&key) {
            if let Some((_, rx)) = &self.call {
                // Cameras that ended (camera off, user left) are no longer wanted: forget them.
                self.shown.retain(|k| !matches!(k, VideoKey::User(id) if *id != me) || rx.is_wanted(k));
            }
            let remote = self.shown.iter().filter(|k| self.is_remote(k, me)).count();
            if !self.is_remote(&key, me) || remote < MAX_REMOTE_SHOWN {
                self.shown.insert(key.clone());
            }
        }
        let on = self.shown.contains(&key);
        let preview = match &key {
            VideoKey::User(id) if *id == me => Some(self.camera.as_ref()),
            VideoKey::Stream(k) if self.is_share(k) => Some(self.share.as_ref().and_then(|s| s.live.conn.as_ref()?.capture.as_ref())),
            _ => None,
        };
        if let Some(capture) = preview {
            if let Some(c) = capture {
                c.sending.set_preview(self.preview(&key));
            }
            if !on {
                self.frames.remove(&key);
            }
            return;
        }
        let link = match &key {
            VideoKey::User(_) => self.call.as_ref().map(|(ctl, rx)| (ctl, rx)),
            VideoKey::Stream(k) => self.watching.iter().find(|l| l.key == *k).and_then(|l| l.conn.as_ref()).map(|c| (&c.ctl, &c.rx)),
            VideoKey::Media(_) => None,
        };
        if let Some((ctl, rx)) = link
            && rx.set_wanted(&key, on)
        {
            let _ = ctl.send(VideoCtl::Wants);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendEvent, EventSink};

    /// Someone else's stream in my call, and mine (I am 9, the call is guild 1 channel 2).
    const KEY: &str = "guild:1:2:5";
    const MINE: &str = "guild:1:2:9";

    #[test]
    fn stream_keys() {
        assert_eq!(stream_key(Some(Id(1)), Id(2), Id(3)), "guild:1:2:3");
        assert_eq!(stream_key(None, Id(2), Id(3)), "call:2:3");
        assert_eq!(parse_stream_key("guild:1:2:3"), Some((Some(Id(1)), Id(2), Id(3))));
        assert_eq!(parse_stream_key("call:2:3"), Some((None, Id(2), Id(3))));
        for bad in ["test:5", "guild:1:2", "call:2:3:4", "call:x:3", "call:0:3", "", "guild:1:2:3:"] {
            assert_eq!(parse_stream_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn go_live_connect_info_and_texts() {
        let mut live = GoLive { key: "call:2:5".into(), gain: StreamGain::default(), id: 1, server: None, endpoint: Some(("host:443".into(), Secret::new("tok".into()))), conn: None };
        assert!(live.connect_info(Id(9), "s".into()).is_none(), "waits for STREAM_CREATE");
        assert!(live.endpoint.is_some(), "token kept until then");
        live.server = Some((Id(77), None));
        let info = live.connect_info(Id(9), "s".into()).unwrap();
        assert_eq!((info.server_id, info.channel_id, info.user_id, info.dave_group), (Id(77), Id(2), Id(9), Id(76)), "the key's channel, group rtc_server_id - 1");
        assert_eq!((info.session_id.as_str(), info.endpoint.as_str(), info.token.expose()), ("s", "host:443", "tok"));
        assert!(live.connect_info(Id(9), "s".into()).is_none(), "one connect per token");
        (live.server, live.endpoint) = (Some((Id(77), Some(Id(78)))), Some(("h:1".into(), Secret::new("t".into()))));
        assert_eq!(live.connect_info(Id(9), "s".into()).unwrap().channel_id, Id(78));

        assert_eq!(delete_text(Some("user_requested")), None);
        assert_eq!(delete_text(Some("stream_full")), Some("This stream is full."));
        assert_eq!(delete_text(Some("stream_ended")), Some("The stream has ended."));
        assert_eq!(delete_text(None), Some("The stream has ended."));
        assert_eq!(end_text(transport::End::Failed("x".into())), "x");
        assert_eq!(end_text(transport::End::Kicked), "The stream connection was lost.");
    }

    /// `(video, UI events)`; what it posts back goes nowhere (tests post by hand).
    fn video() -> (Video, std::sync::mpsc::Receiver<BackendEvent>) {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let events = VoiceEvents(EventSink::new(ev_tx, egui::Context::default()));
        (Video::new(events, Arc::default(), mpsc::unbounded_channel().0.downgrade()), ev_rx)
    }

    /// Connected to guild 1 channel 2 as user 9.
    fn ctx(gw: &mpsc::UnboundedSender<GatewayCommand>) -> Ctx {
        let playback = Some(crate::voice::audio::test_hooks::speaker().0);
        Ctx { gateway: Some(gw.clone()), me: Id(9), session_id: Some("sess".into()), call: Some((Some(Id(1)), Id(2))), playback }
    }

    fn events(rx: &std::sync::mpsc::Receiver<BackendEvent>) -> Vec<VideoEvent> {
        rx.try_iter().filter_map(|e| if let BackendEvent::Voice(VoiceEvent::Video(v)) = e { Some(v) } else { None }).collect()
    }

    fn ops(rx: &mut mpsc::UnboundedReceiver<GatewayCommand>) -> Vec<StreamOp> {
        std::iter::from_fn(|| rx.try_recv().ok()).filter_map(|c| if let GatewayCommand::Stream(op) = c { Some(op) } else { None }).collect()
    }

    fn create(key: &str) -> GatewayEvent {
        GatewayEvent::StreamCreate { stream_key: key.into(), rtc_server_id: Id(77), rtc_channel_id: Some(Id(78)), viewer_ids: Vec::new(), paused: false }
    }

    /// A closed local port: the spawned connection fails at once (its `Ended` goes nowhere).
    fn server(key: &str, endpoint: Option<&str>) -> GatewayEvent {
        GatewayEvent::StreamServerUpdate { stream_key: key.into(), endpoint: endpoint.map(String::from), token: Secret::new("stream-token".into()) }
    }

    fn delete(key: &str, reason: &str) -> GatewayEvent {
        GatewayEvent::StreamDelete { stream_key: key.into(), reason: Some(reason.into()), unavailable: false }
    }

    fn is_error(ev: &VideoEvent) -> bool {
        matches!(ev, VideoEvent::Error(_))
    }

    #[test]
    fn remote_videos_shown_are_bounded_and_ended_ones_forgotten() {
        let (mut v, _ui) = video();
        let rx = v.link_for_call().rx;
        let (me, user) = (Id(9), |id| VideoKey::User(Id(id)));
        for id in 1..=MAX_REMOTE_SHOWN as u64 + 1 {
            v.set_visible(user(id), true, me);
        }
        v.set_visible(VideoKey::Stream(KEY.into()), true, me);
        v.set_visible(user(9), true, me);
        assert_eq!(v.shown.len(), MAX_REMOTE_SHOWN + 1, "4 remote videos and my preview");
        assert!(v.shown.contains(&user(9)) && !v.shown.contains(&user(5)) && !v.shown.contains(&VideoKey::Stream(KEY.into())));
        assert!(!rx.is_wanted(&user(5)), "a refused video is not asked for");

        // User 1's camera ended: its place is free again.
        rx.on_remote(crate::voice::video::RemoteVideo { user_id: Id(1), ssrc: 100, rtx_ssrc: 0, active: true });
        rx.on_remote(crate::voice::video::RemoteVideo { user_id: Id(1), ssrc: 0, rtx_ssrc: 0, active: false });
        v.set_visible(user(5), true, me);
        assert!(v.shown.contains(&user(5)) && !v.shown.contains(&user(1)) && rx.is_wanted(&user(5)));
        // Hiding frees a place too.
        v.set_visible(user(2), false, me);
        v.set_visible(user(6), true, me);
        assert!(v.shown.contains(&user(6)) && rx.is_wanted(&user(6)));
    }

    #[tokio::test]
    async fn screen_share_signalling() {
        let (mut v, ui) = video();
        let (gw_tx, mut gw) = mpsc::unbounded_channel();
        let share = |audio| VideoCommand::StartScreenShare { source: ScreenSource::Monitor(1), fps: 60, max_height: 1080, audio };

        v.command(share(true), ctx(&gw_tx));
        v.command(share(false), Ctx { call: None, playback: None, ..ctx(&gw_tx) });
        v.command(VideoCommand::StartCamera { device: None }, ctx(&gw_tx));
        let evs = events(&ui);
        assert!(evs.len() == 3 && evs.iter().all(is_error), "audio, no call, no voice link: {evs:?}");
        assert!(ops(&mut gw).is_empty() && !v.camera_on());
        v.command(VideoCommand::StopCamera, ctx(&gw_tx));
        v.command(VideoCommand::StopScreenShare, ctx(&gw_tx));
        assert!(events(&ui).is_empty(), "stopping what is off says nothing");

        // Op 18 + op 22; the two dispatches in any order; the server's key decides.
        v.command(share(false), ctx(&gw_tx));
        assert_eq!(
            ops(&mut gw),
            [StreamOp::Create { guild_id: Some(Id(1)), channel_id: Id(2) }, StreamOp::SetPaused { stream_key: MINE.into(), paused: false }]
        );
        assert_eq!(v.share.as_ref().map(|s| s.limits), Some(SendLimits::screen(30, 1080)));
        v.on_gateway(server(MINE, Some("127.0.0.1:9")), ctx(&gw_tx));
        v.on_gateway(create(KEY), ctx(&gw_tx));
        assert!(v.share.as_ref().unwrap().live.conn.is_none(), "waits for its own STREAM_CREATE");
        v.on_gateway(create(MINE), ctx(&gw_tx));
        let id = v.share.as_ref().unwrap().live.id;
        assert!(v.share.as_ref().unwrap().live.conn.is_some());

        // Reallocation drops the connection and waits; late messages of it are ignored.
        v.on_gateway(server(MINE, None), ctx(&gw_tx));
        assert!(v.share.as_ref().unwrap().live.conn.is_none());
        v.on_msg(StreamMsg::Ended { stream_key: MINE.into(), id, end: transport::End::Rejoin }, ctx(&gw_tx));
        v.on_msg(StreamMsg::Timeout { stream_key: MINE.into(), id }, ctx(&gw_tx));
        v.on_msg(StreamMsg::CaptureFailed { kind: StreamKind::Screen, id, reason: "gone".into() }, ctx(&gw_tx));
        assert!(v.share.is_some() && events(&ui).is_empty() && ops(&mut gw).is_empty());

        // The new server: connected, then lost → op 19, the reason, stopped. No reconnect.
        v.on_gateway(server(MINE, Some("127.0.0.1:9")), ctx(&gw_tx));
        let id = v.share.as_ref().unwrap().live.id;
        v.on_msg(StreamMsg::Ended { stream_key: MINE.into(), id, end: transport::End::Failed("lost".into()) }, ctx(&gw_tx));
        assert_eq!(ops(&mut gw), [StreamOp::Delete { stream_key: MINE.into() }]);
        assert_eq!(events(&ui), [VideoEvent::Error("lost".into()), VideoEvent::ScreenShareStopped]);
        assert!(v.share.is_none());

        // Stopped by me: op 19. Ended by the server on my request: no op 19, no toast.
        v.command(share(false), ctx(&gw_tx));
        v.command(VideoCommand::StopScreenShare, ctx(&gw_tx));
        assert_eq!(ops(&mut gw).last(), Some(&StreamOp::Delete { stream_key: MINE.into() }));
        v.command(share(false), ctx(&gw_tx));
        ops(&mut gw);
        v.on_gateway(delete(MINE, "user_requested"), ctx(&gw_tx));
        assert!(ops(&mut gw).is_empty());
        assert_eq!(events(&ui), [VideoEvent::ScreenShareStopped, VideoEvent::ScreenShareStopped]);

        // Not connected in time.
        v.command(share(false), ctx(&gw_tx));
        let id = v.share.as_ref().unwrap().live.id;
        v.on_msg(StreamMsg::Timeout { stream_key: MINE.into(), id }, ctx(&gw_tx));
        assert_eq!(ops(&mut gw).last(), Some(&StreamOp::Delete { stream_key: MINE.into() }));
        assert_eq!(events(&ui), [VideoEvent::Error("Could not start the stream.".into()), VideoEvent::ScreenShareStopped]);
    }

    #[tokio::test]
    async fn watching_signalling() {
        let (mut v, ui) = video();
        let (gw_tx, mut gw) = mpsc::unbounded_channel();
        let watch = |key: &str| VideoCommand::WatchStream { stream_key: key.into() };

        for bad in ["guild:1:3:5", "call:2:5", MINE, "nope"] {
            v.command(watch(bad), ctx(&gw_tx));
        }
        let evs = events(&ui);
        assert!(evs.len() == 4 && evs.iter().all(is_error), "other channel, other call, mine, garbage: {evs:?}");
        assert!(ops(&mut gw).is_empty());

        // Op 20, once; shown before it connects: decoded from the start.
        v.command(watch(KEY), ctx(&gw_tx));
        v.command(watch(KEY), ctx(&gw_tx));
        assert_eq!(ops(&mut gw), [StreamOp::Watch { stream_key: KEY.into() }]);
        v.command(VideoCommand::SetVideoVisible { key: VideoKey::Stream(KEY.into()), visible: true }, ctx(&gw_tx));
        v.on_gateway(create(KEY), ctx(&gw_tx));
        v.on_gateway(server(KEY, Some("127.0.0.1:9")), ctx(&gw_tx));
        let id = v.watching[0].id;
        v.on_msg(StreamMsg::Connected { stream_key: KEY.into(), id: 0 }, ctx(&gw_tx));
        assert!(events(&ui).is_empty(), "an older attempt");
        v.on_msg(StreamMsg::Connected { stream_key: KEY.into(), id }, ctx(&gw_tx));
        v.on_msg(StreamMsg::Timeout { stream_key: KEY.into(), id }, ctx(&gw_tx));
        assert_eq!(events(&ui), [VideoEvent::RemoteVideo { key: VideoKey::Stream(KEY.into()), width: 0, height: 0 }]);

        // Stop: op 19, ended, forgotten.
        v.command(VideoCommand::StopWatching { stream_key: KEY.into() }, ctx(&gw_tx));
        assert_eq!(ops(&mut gw), [StreamOp::Delete { stream_key: KEY.into() }]);
        assert_eq!(events(&ui), [VideoEvent::RemoteVideoEnded { key: VideoKey::Stream(KEY.into()) }]);
        assert!(v.watching.is_empty() && v.shown.is_empty());

        // Refused by the server: its reason, no op 19.
        v.command(watch(KEY), ctx(&gw_tx));
        v.on_gateway(delete(KEY, "stream_full"), ctx(&gw_tx));
        assert_eq!(ops(&mut gw), [StreamOp::Watch { stream_key: KEY.into() }]);
        assert_eq!(events(&ui), [VideoEvent::Error("This stream is full.".into()), VideoEvent::RemoteVideoEnded { key: VideoKey::Stream(KEY.into()) }]);

        // Bounded.
        for owner in 10..10 + MAX_WATCHED {
            v.command(watch(&format!("guild:1:2:{owner}")), ctx(&gw_tx));
        }
        v.command(watch(KEY), ctx(&gw_tx));
        assert_eq!(ops(&mut gw).len(), MAX_WATCHED);
        assert!(matches!(&events(&ui)[..], [VideoEvent::Error(_)]));

        // The call ends: every stream ends with op 19, shown cameras end too.
        v.command(VideoCommand::SetVideoVisible { key: VideoKey::User(Id(5)), visible: true }, ctx(&gw_tx));
        v.command(VideoCommand::SetVideoVisible { key: VideoKey::User(Id(9)), visible: true }, ctx(&gw_tx));
        v.command(VideoCommand::StartScreenShare { source: ScreenSource::Window(3), fps: 30, max_height: 720, audio: false }, ctx(&gw_tx));
        ops(&mut gw);
        v.call_ended(ctx(&gw_tx));
        let deletes = ops(&mut gw);
        assert_eq!(deletes.len(), MAX_WATCHED + 1);
        assert!(deletes.iter().all(|op| matches!(op, StreamOp::Delete { .. })));
        let evs = events(&ui);
        assert_eq!(evs[0], VideoEvent::ScreenShareStopped);
        assert_eq!(evs.iter().filter(|e| matches!(e, VideoEvent::RemoteVideoEnded { key: VideoKey::Stream(_) })).count(), MAX_WATCHED);
        assert_eq!(evs.last(), Some(&VideoEvent::RemoteVideoEnded { key: VideoKey::User(Id(5)) }), "not my own preview");
        assert!(v.watching.is_empty() && v.share.is_none() && v.shown.is_empty());
    }
}
