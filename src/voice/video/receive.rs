//! Received video of one connection (voice or Go Live viewer): which videos are wanted, the
//! SSRC → video map (op 12; on a Go Live connection only the stream owner's), RTX mapping, the
//! receive thread (`rtp_h264::Depacketizer` → `Dave::decrypt_video` → `codec::Decoder` →
//! `FrameWriter`), NACK/PLI out, `RemoteVideo`/`RemoteVideoEnded` events.
//!
//! OWNERSHIP: video RTP engineer. Keep the pub signatures (transport/video.rs and stream.rs use
//! them).
//!
//! Threads and cost: `push` runs on the transport's UDP task: a map lookup, then a drop (not
//! wanted) or a `try_send` into a bounded queue (`RTP_QUEUE`, full = drop + NACK later). One
//! "rustcord-video-rx" std thread per connection, started with the first wanted video and ended
//! when none is wanted, does everything else. No wanted video = no thread, no allocation.

use super::codec::Decoder;
use super::rtp_h264::{self, Depacketizer, Feedback, RtpPacket};
use super::stream::parse_stream_key;
use super::{FrameWriter, RemoteVideo, VideoEvent, VideoFrames, VideoKey, lock};
use crate::model::Id;
use crate::voice::dave::Dave;
use crate::voice::{VoiceEvent, VoiceEvents};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Received video packets waiting for the receive thread (~1 s of 1080p30).
pub const RTP_QUEUE: usize = 512;
/// NACK / keyframe-request cadence of the receive thread.
const TICK: Duration = Duration::from_millis(20);
/// Keyframe requests (PLI) only for a video whose packets arrived this recently.
const FLOWING: Duration = Duration::from_secs(1);

/// Receive side of one connection. Cheap to clone; every method returns at once.
#[derive(Clone)]
pub struct VideoRx {
    inner: Arc<Mutex<State>>,
}

struct State {
    frames: Arc<VideoFrames>,
    events: VoiceEvents,
    /// Go Live viewer connection: every video on it is this stream.
    stream_key: Option<String>,
    /// What the UI shows (`set_wanted`).
    wanted: HashSet<VideoKey>,
    /// Active videos announced by op 12, by user.
    remote: HashMap<Id, RemoteVideo>,
    /// Video and RTX SSRCs of the wanted announced videos: all that `wants` lets through.
    routes: HashMap<u32, Route>,
    /// +1 per change of `routes` (the thread re-reads them then).
    version: u64,
    /// From `attach`: DAVE and the RTCP way out.
    media: Option<Media>,
    /// Where RTCP feedback about my own video goes (the transport's send task).
    feedback: Option<mpsc::UnboundedSender<Feedback>>,
    /// The receive thread's queue; None = no thread.
    queue: Option<std_mpsc::SyncSender<Item>>,
    /// `end` was called: nothing is decoded or reported any more.
    ended: bool,
}

/// A connection's DAVE and where plain RTCP goes.
type Media = (Arc<Mutex<Dave>>, mpsc::UnboundedSender<Vec<u8>>);

#[derive(Clone, PartialEq)]
struct Route {
    video_ssrc: u32,
    key: VideoKey,
    /// Whose DAVE key decrypts it.
    user: Id,
}

/// One packet for the receive thread (RTX already unwrapped).
struct Item {
    ssrc: u32,
    seq: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
}

impl State {
    fn key_of(&self, user: Id) -> VideoKey {
        match &self.stream_key {
            Some(k) => VideoKey::Stream(k.clone()),
            None => VideoKey::User(user),
        }
    }

    fn wanted_ssrcs(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.routes.values().map(|r| r.video_ssrc).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn report(&self, ev: VideoEvent) {
        if !self.ended {
            self.events.send(VoiceEvent::Video(ev));
        }
    }

    /// Recompute the routes and start/stop the thread; true = the wanted SSRC set changed.
    fn update(&mut self, me: Weak<Mutex<State>>) -> bool {
        let before = self.wanted_ssrcs();
        let mut routes = HashMap::new();
        for v in self.remote.values().filter(|_| !self.ended) {
            let key = self.key_of(v.user_id);
            if self.wanted.contains(&key) {
                let route = Route { video_ssrc: v.ssrc, key, user: v.user_id };
                if v.rtx_ssrc != 0 {
                    routes.insert(v.rtx_ssrc, route.clone());
                }
                routes.insert(v.ssrc, route);
            }
        }
        if routes != self.routes {
            (self.routes, self.version) = (routes, self.version + 1);
        }
        if self.routes.is_empty() {
            self.queue = None; // the thread ends
        } else if let (None, Some((dave, rtcp))) = (&self.queue, &self.media) {
            self.queue = RxThread::spawn(me, dave.clone(), rtcp.clone(), self.frames.clone());
        }
        before != self.wanted_ssrcs()
    }
}

impl VideoRx {
    /// `stream_key`: Some on a Go Live connection (its videos are `VideoKey::Stream(key)`),
    /// None on the voice connection (`VideoKey::User(op 12 user_id)`).
    pub fn new(frames: Arc<VideoFrames>, events: VoiceEvents, stream_key: Option<String>) -> VideoRx {
        let state = State {
            frames,
            events,
            stream_key,
            wanted: HashSet::new(),
            remote: HashMap::new(),
            routes: HashMap::new(),
            version: 0,
            media: None,
            feedback: None,
            queue: None,
            ended: false,
        };
        VideoRx { inner: Arc::new(Mutex::new(state)) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State, Weak<Mutex<State>>) -> R) -> R {
        f(&mut lock(&self.inner), Arc::downgrade(&self.inner))
    }

    /// `SetVideoVisible`: decode `key` only while wanted; unwanting frees its frame slot
    /// (`VideoFrames::remove`). True = the wanted SSRC set changed (send op 15).
    pub fn set_wanted(&self, key: &VideoKey, on: bool) -> bool {
        self.with(|s, me| {
            let changed = if on { s.wanted.insert(key.clone()) } else { s.wanted.remove(key) };
            if !on {
                s.frames.remove(key);
            }
            changed && s.update(me)
        })
    }

    /// Whether `key` is wanted (`set_wanted`, and its video has not ended since).
    pub fn is_wanted(&self, key: &VideoKey) -> bool {
        lock(&self.inner).wanted.contains(key)
    }

    /// The video SSRCs to ask for in op 15.
    pub fn wanted_ssrcs(&self) -> Vec<u32> {
        lock(&self.inner).wanted_ssrcs()
    }

    /// Received op 12: emits `RemoteVideo` (0×0) when a video appears, `RemoteVideoEnded` when
    /// it stops. True = op 15 must be re-sent. On a Go Live connection only the owner's op 12
    /// counts and the stream's events come from `stream::Video`.
    pub fn on_remote(&self, v: RemoteVideo) -> bool {
        self.with(|s, me| {
            let owner = s.stream_key.as_deref().and_then(parse_stream_key).map(|k| k.2);
            if s.ended || owner.is_some_and(|o| o != v.user_id) {
                return false;
            }
            let was = if v.active { s.remote.insert(v.user_id, v) } else { s.remote.remove(&v.user_id) };
            if s.stream_key.is_none() {
                let key = s.key_of(v.user_id);
                match (was.is_some(), v.active) {
                    (false, true) => s.report(VideoEvent::RemoteVideo { key, width: 0, height: 0 }),
                    (true, false) => {
                        // Its tile is gone: a later camera-on is not decoded until shown again.
                        s.wanted.remove(&key);
                        s.frames.remove(&key);
                        s.report(VideoEvent::RemoteVideoEnded { key });
                    }
                    _ => {}
                }
            }
            s.update(me)
        })
    }

    /// Op 13: the user left; their video ended.
    pub fn remove_user(&self, user_id: Id) {
        let off = RemoteVideo { user_id, ssrc: 0, rtx_ssrc: 0, active: false };
        self.on_remote(off);
    }

    /// Session Description: this connection's DAVE (for `decrypt_video`) and where plain RTCP
    /// feedback (NACK/PLI) goes; the transport seals and sends it.
    pub fn attach(&self, dave: Arc<Mutex<Dave>>, rtcp: mpsc::UnboundedSender<Vec<u8>>) {
        self.with(|s, me| {
            s.media = Some((dave, rtcp));
            s.update(me);
        });
    }

    /// Where NACK/PLI received about my own video go (the transport's video send task).
    pub fn route_feedback(&self, to: mpsc::UnboundedSender<Feedback>) {
        lock(&self.inner).feedback = Some(to);
    }

    /// RTCP feedback received on this connection (for my own video, if any).
    pub fn feedback(&self, fb: Feedback) {
        if let Some(to) = &lock(&self.inner).feedback {
            let _ = to.send(fb);
        }
    }

    /// Whether packets of `ssrc` (video or its RTX) are wanted: checked before transport
    /// decryption, so unwanted video costs nothing more.
    pub fn wants(&self, ssrc: u32) -> bool {
        lock(&self.inner).routes.contains_key(&ssrc)
    }

    /// One wanted video packet, transport-decrypted. `rtx`: PT 104 (unwrap and map to the video
    /// SSRC first).
    pub fn push(&self, rtx: bool, p: RtpPacket<'_>) {
        let s = lock(&self.inner);
        let (Some(route), Some(queue)) = (s.routes.get(&p.ssrc), &s.queue) else { return };
        if rtx == (p.ssrc == route.video_ssrc) {
            return; // video on the RTX SSRC or RTX on the video SSRC
        }
        let (seq, payload) = if rtx {
            let Some(unwrapped) = rtp_h264::unwrap_rtx(p.payload) else { return };
            unwrapped
        } else {
            (p.seq, p.payload)
        };
        let item = Item { ssrc: route.video_ssrc, seq, timestamp: p.timestamp, marker: p.marker, payload: payload.to_vec() };
        let _ = queue.try_send(item); // full: dropped, NACKed later
    }

    /// The manager is done with this connection (call ended, Go Live stopped): ends every
    /// video it announced (`RemoteVideoEnded`), stops decoding and ignores whatever the closing
    /// connection still delivers.
    pub fn end(&self) {
        self.with(|s, me| {
            if s.stream_key.is_none() {
                for user in std::mem::take(&mut s.remote).into_keys() {
                    let key = VideoKey::User(user);
                    s.frames.remove(&key);
                    s.report(VideoEvent::RemoteVideoEnded { key });
                }
            }
            s.ended = true;
            s.update(me);
        });
    }
}

/// One wanted video on the receive thread.
struct Decoding {
    key: VideoKey,
    user: Id,
    depack: Depacketizer,
    /// None if openh264 could not start (reported once).
    decoder: Option<Decoder>,
    /// None while all frame slots are taken.
    writer: Option<FrameWriter>,
    /// Last size reported to the UI.
    size: (u32, u32),
    last_packet: Option<Instant>,
}

/// The "rustcord-video-rx" thread of one connection.
struct RxThread {
    /// Weak: the thread never keeps the connection's state alive.
    state: Weak<Mutex<State>>,
    dave: Arc<Mutex<Dave>>,
    rtcp: mpsc::UnboundedSender<Vec<u8>>,
    frames: Arc<VideoFrames>,
    videos: HashMap<u32, Decoding>,
    /// `State::version` the videos match.
    version: Option<u64>,
}

impl RxThread {
    fn spawn(state: Weak<Mutex<State>>, dave: Arc<Mutex<Dave>>, rtcp: mpsc::UnboundedSender<Vec<u8>>, frames: Arc<VideoFrames>) -> Option<std_mpsc::SyncSender<Item>> {
        let (tx, rx) = std_mpsc::sync_channel(RTP_QUEUE);
        let mut t = RxThread { state, dave, rtcp, frames, videos: HashMap::new(), version: None };
        std::thread::Builder::new().name("rustcord-video-rx".into()).spawn(move || t.run(&rx)).ok()?;
        Some(tx)
    }

    /// Until the queue's sender is dropped (nothing wanted, connection gone).
    fn run(&mut self, queue: &std_mpsc::Receiver<Item>) {
        let mut next_tick = Instant::now();
        loop {
            match queue.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(item) => self.on_item(item),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if now >= next_tick {
                if !self.tick(now) {
                    return;
                }
                next_tick = now + TICK;
            }
        }
    }

    /// Follow the routes: new wanted videos get a decoder, unwanted ones are dropped. False =
    /// the connection is gone.
    fn sync(&mut self) -> bool {
        let Some(state) = self.state.upgrade() else { return false };
        let routes: HashMap<u32, (VideoKey, Id)> = {
            let s = lock(&state);
            if self.version == Some(s.version) {
                return true;
            }
            self.version = Some(s.version);
            s.routes.values().map(|r| (r.video_ssrc, (r.key.clone(), r.user))).collect()
        };
        self.videos.retain(|ssrc, d| routes.get(ssrc).is_some_and(|(k, u)| (k, *u) == (&d.key, d.user)));
        for (ssrc, (key, user)) in routes {
            let d = self.videos.entry(ssrc).or_insert_with(|| Decoding {
                key: key.clone(),
                user,
                depack: Depacketizer::default(),
                decoder: Decoder::new().inspect_err(|e| eprintln!("video: {e}")).ok(),
                writer: None,
                size: (0, 0),
                last_packet: None,
            });
            // The slot may have been freed and re-created meanwhile (hidden and shown again).
            let writer = self.frames.writer(key);
            if d.writer.is_none() && writer.is_some() {
                d.depack.wait_for_keyframe(); // the decoder missed frames without a slot
            }
            d.writer = writer;
        }
        true
    }

    fn on_item(&mut self, item: Item) {
        if !self.videos.contains_key(&item.ssrc) && !self.sync() {
            return;
        }
        let Some(d) = self.videos.get_mut(&item.ssrc) else { return };
        let now = Instant::now();
        d.last_packet = Some(now);
        let p = RtpPacket { ssrc: item.ssrc, seq: item.seq, timestamp: item.timestamp, marker: item.marker, payload: &item.payload };
        d.depack.push(p, now);
        while let Some(frame) = d.depack.next_frame() {
            if let Some(size) = decode(d, &self.dave, &frame)
                && let Some(state) = self.state.upgrade()
            {
                d.size = size;
                // Through the state: silent once `end` was called.
                lock(&state).report(VideoEvent::RemoteVideo { key: d.key.clone(), width: size.0, height: size.1 });
            }
        }
    }

    /// NACKs and keyframe requests. False = the connection is gone.
    fn tick(&mut self, now: Instant) -> bool {
        if !self.sync() {
            return false;
        }
        for (&ssrc, d) in &mut self.videos {
            if d.writer.is_none() {
                // A frame slot came free: the decoder missed frames, restart at a keyframe.
                d.writer = self.frames.writer(d.key.clone());
                if d.writer.is_some() {
                    d.depack.wait_for_keyframe();
                }
            }
            let lost = d.depack.take_nacks(now);
            if !lost.is_empty() {
                let _ = self.rtcp.send(rtp_h264::nack(0, ssrc, &lost));
            }
            let flowing = d.last_packet.is_some_and(|t| now.saturating_duration_since(t) < FLOWING);
            if flowing && d.depack.take_keyframe_request(now) {
                let _ = self.rtcp.send(rtp_h264::pli(0, ssrc));
            }
        }
        true
    }
}

/// One reassembled frame → DAVE → decoder → frame slot. Some(size) = a new picture size to
/// report. A frame that does not decrypt or decode waits for the next keyframe.
fn decode(d: &mut Decoding, dave: &Mutex<Dave>, frame: &[u8]) -> Option<(u32, u32)> {
    // No frame slot: not even decrypted; `tick` resyncs at a keyframe once one frees.
    d.writer.as_ref()?;
    // The DAVE lock is held for the decryption only, never across decoding.
    let plain = lock(dave).decrypt_video(d.user, frame);
    let Some(plain) = plain else {
        d.depack.wait_for_keyframe();
        return None;
    };
    let (Some(writer), Some(decoder)) = (&d.writer, &mut d.decoder) else { return None };
    match decoder.decode(&plain, writer) {
        Ok(Some(size)) if size != d.size => Some(size),
        Ok(_) => None,
        Err(_) => {
            d.depack.wait_for_keyframe();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendEvent, EventSink};

    fn rx(stream_key: Option<&str>) -> (VideoRx, Arc<VideoFrames>, std_mpsc::Receiver<BackendEvent>) {
        let (tx, events) = std_mpsc::channel();
        let frames = Arc::new(VideoFrames::default());
        let sink = VoiceEvents(EventSink::new(tx, egui::Context::default()));
        (VideoRx::new(frames.clone(), sink, stream_key.map(String::from)), frames, events)
    }

    fn events(rx: &std_mpsc::Receiver<BackendEvent>) -> Vec<VideoEvent> {
        rx.try_iter().filter_map(|e| if let BackendEvent::Voice(VoiceEvent::Video(v)) = e { Some(v) } else { None }).collect()
    }

    /// `user`'s camera on `ssrc` (RTX `ssrc + 1`); ssrc 0 = off.
    fn cam(user: u64, ssrc: u32) -> RemoteVideo {
        RemoteVideo { user_id: Id(user), ssrc, rtx_ssrc: if ssrc == 0 { 0 } else { ssrc + 1 }, active: ssrc != 0 }
    }

    fn thread_running(rx: &VideoRx) -> bool {
        lock(&rx.inner).queue.is_some()
    }

    fn attach(rx: &VideoRx) {
        rx.attach(Arc::new(Mutex::new(Dave::new(Id(9), Id(2)))), mpsc::unbounded_channel().0);
    }

    #[test]
    fn only_wanted_announced_videos_pass() {
        let (rx, frames, ui) = rx(None);
        let (five, six) = (VideoKey::User(Id(5)), VideoKey::User(Id(6)));
        assert!(!rx.set_wanted(&five, true), "wanted before announced: no SSRC to ask for yet");
        assert!(rx.on_remote(cam(5, 100)), "announced while wanted: op 15");
        assert!(!rx.on_remote(cam(6, 200)), "not wanted: no op 15");
        assert!(!rx.on_remote(cam(5, 100)), "repeated op 12 changes nothing");
        let shown = |key: &VideoKey| VideoEvent::RemoteVideo { key: key.clone(), width: 0, height: 0 };
        assert_eq!(events(&ui), [shown(&five), shown(&six)]);
        assert_eq!(rx.wanted_ssrcs(), [100]);
        assert!(rx.wants(100) && rx.wants(101), "the video and its RTX");
        assert!(!rx.wants(200) && !rx.wants(201) && !rx.wants(7), "not wanted or unknown: dropped before decrypting");
        assert!(!thread_running(&rx), "no thread before media is set up");

        attach(&rx);
        assert!(thread_running(&rx), "started with the first wanted video");
        frames.writer(five.clone()).unwrap().write(2, 2, |px| px.fill(255));
        assert!(rx.set_wanted(&five, false));
        assert!(frames.latest(&five).is_none(), "hiding frees the frame");
        assert!(!rx.wants(100) && !thread_running(&rx), "nothing wanted: no thread");
        assert!(rx.set_wanted(&six, true) && thread_running(&rx));

        // A new SSRC for the same camera re-routes it.
        assert!(rx.on_remote(cam(6, 300)));
        assert!(rx.wants(300) && !rx.wants(200));
        assert!(events(&ui).is_empty());

        // Camera off (op 12 with no video), and a user leaving (op 13).
        assert!(rx.on_remote(cam(6, 0)));
        assert!(!rx.wants(300) && !thread_running(&rx));
        rx.remove_user(Id(5));
        rx.remove_user(Id(5));
        let ended = |key: &VideoKey| VideoEvent::RemoteVideoEnded { key: key.clone() };
        assert_eq!(events(&ui), [ended(&six), ended(&five)], "once each");
    }

    #[test]
    fn an_ended_camera_is_not_wanted_when_it_comes_back() {
        let (rx, _, ui) = rx(None);
        attach(&rx);
        let five = VideoKey::User(Id(5));
        rx.set_wanted(&five, true);
        assert!(rx.on_remote(cam(5, 100)) && thread_running(&rx));
        assert!(rx.on_remote(cam(5, 0)), "camera off");
        assert!(!rx.is_wanted(&five));
        assert!(!rx.on_remote(cam(5, 100)), "camera on again: no op 15 until shown again");
        assert!(!rx.wants(100) && !thread_running(&rx));
        // The same when the user leaves (op 13).
        assert!(rx.set_wanted(&five, true) && thread_running(&rx));
        rx.remove_user(Id(5));
        assert!(!rx.is_wanted(&five) && !thread_running(&rx));
        let (on, off) = (VideoEvent::RemoteVideo { key: five.clone(), width: 0, height: 0 }, VideoEvent::RemoteVideoEnded { key: five });
        assert_eq!(events(&ui), [on.clone(), off.clone(), on, off]);
    }

    #[test]
    fn end_reports_every_camera_once_then_stays_silent() {
        let (rx, _, ui) = rx(None);
        attach(&rx);
        rx.set_wanted(&VideoKey::User(Id(5)), true);
        rx.on_remote(cam(5, 100));
        rx.on_remote(cam(6, 200));
        events(&ui);
        rx.end();
        let mut ended = events(&ui);
        ended.sort_by_key(|e| format!("{e:?}"));
        let end = |id| VideoEvent::RemoteVideoEnded { key: VideoKey::User(Id(id)) };
        assert_eq!(ended, [end(5), end(6)]);
        assert!(!rx.wants(100) && !thread_running(&rx));
        assert!(!rx.on_remote(cam(7, 300)) && !rx.wants(300), "a late op 12 of the closing connection");
        rx.end();
        assert!(events(&ui).is_empty());
    }

    #[test]
    fn go_live_takes_the_owners_video_only() {
        let (rx, _, ui) = rx(Some("guild:1:2:5"));
        let key = VideoKey::Stream("guild:1:2:5".into());
        rx.set_wanted(&key, true);
        assert!(!rx.on_remote(cam(7, 300)) && !rx.wants(300), "not the stream owner");
        assert!(rx.on_remote(cam(5, 100)));
        assert!(rx.wants(100) && rx.wants(101));
        rx.end();
        assert!(events(&ui).is_empty(), "the stream's events come from stream::Video");
    }
}
