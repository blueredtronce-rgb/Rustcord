//! Video paths of one voice or Go Live connection (docs/VIDEO_DESIGN.md "Transport"): Ready's
//! video SSRCs, op 12/15, video RTP out (`VideoSend` frames → `Dave::encrypt_h264` →
//! `rtp_h264::packetize` → `write_video_header` → seal → socket, RTX history, keyframe flag on
//! PLI) and in (`VideoRx::wants` before decrypting, then `VideoRx::push`), RTCP both ways.
//!
//! OWNERSHIP: video RTP engineer. transport.rs calls exactly these hooks; keep their signatures.

use super::lock;
use super::packet::{Crypt, RtpHeader};
use crate::model::Id;
use crate::voice::dave::Dave;
use crate::voice::video::receive::VideoRx;
use crate::voice::video::rtp_h264::{self, Feedback, MAX_PAYLOAD, RtpPacket, RtxHistory};
use crate::voice::video::{EncodedFrame, H264_PT, H264_RTX_PT, StreamKind, VIDEO_CLOCK_HZ, VideoCtl, VideoLink, VideoSend, op12_payload, op15_payload, parse_op12};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

/// Clear part of an RTCP packet under rtpsize AEAD: V/P/RC, PT, length, sender SSRC.
const RTCP_HEADER: usize = 8;
/// PLIs closer together than this ask for one keyframe (many viewers, one encoder).
const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(500);
/// RTCP Sender Report cadence while my video is sent (VOICE_NOTES §5.3).
const SR_INTERVAL: Duration = Duration::from_secs(5);
/// Seconds from the NTP epoch (1900) to the Unix epoch.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// To the send task: Some = send this pipeline (replacing the running one), None = stop.
type SendReq = Option<VideoSend>;

/// The video state of one connection (survives resumes, like `Conn`).
pub(super) struct VideoConn {
    link: VideoLink,
    /// Ready: my audio SSRC and `streams[0]`'s video / RTX SSRCs (0 = none).
    ssrcs: Option<Ssrcs>,
    sends: mpsc::UnboundedSender<SendReq>,
    /// Moves into the send task at `start_media`.
    sends_rx: Option<mpsc::UnboundedReceiver<SendReq>>,
    /// The SSRCs of the last op 15 (None = none sent yet).
    sent_wants: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Ssrcs {
    audio: u32,
    video: u32,
    rtx: u32,
}

impl VideoConn {
    pub fn new(link: VideoLink) -> VideoConn {
        let (sends, sends_rx) = mpsc::unbounded_channel();
        VideoConn { link, ssrcs: None, sends, sends_rx: Some(sends_rx), sent_wants: None }
    }

    /// Identify `streams[].type`.
    pub fn kind(&self) -> StreamKind {
        self.link.kind
    }

    /// Ready `d`: keep my stream's `ssrc` / `rtx_ssrc` (`streams[0]`) for op 12 and RTP.
    pub fn on_ready(&mut self, d: &Value) {
        self.ssrcs = ready_ssrcs(d);
    }

    /// Received op 12 `d` (`video::parse_op12` → `VideoRx::on_remote`). Some = op 15 to send.
    pub fn on_op12(&mut self, d: &Value) -> Option<String> {
        let v = parse_op12(d)?;
        self.link.rx.on_remote(v);
        self.wants()
    }

    /// Op 15 when the wanted SSRCs differ from the last one sent; never before Ready, and not
    /// for an empty set before anything was asked for.
    fn wants(&mut self) -> Option<String> {
        let wanted = self.link.rx.wanted_ssrcs();
        let last = self.sent_wants.as_deref().unwrap_or_default();
        if self.ssrcs.is_none() || wanted == last {
            return None;
        }
        let payload = op15_payload(&wanted);
        self.sent_wants = Some(wanted);
        Some(payload)
    }

    /// Op 13 client disconnect.
    pub fn on_client_disconnect(&mut self, user_id: Id) {
        self.link.rx.remove_user(user_id);
    }

    /// A manager request. `audio_ssrc`: Ready's SSRC (None before Ready). Returns the payloads
    /// to send on the WebSocket (op 12, op 15) in order.
    pub fn on_ctl(&mut self, ctl: VideoCtl, audio_ssrc: Option<u32>) -> Vec<String> {
        match ctl {
            VideoCtl::Send(send) => {
                // Without Ready's SSRCs nothing can be sent: dropping `send` stops the
                // pipeline, which reports the closed connection.
                let (Some(audio), Some(s)) = (audio_ssrc, self.ssrcs.filter(|s| s.video != 0)) else { return Vec::new() };
                let op12 = op12_payload(audio, Some((s.video, s.rtx, send.kind, send.limits)));
                let _ = self.sends.send(Some(send));
                vec![op12]
            }
            VideoCtl::StopSending => {
                let _ = self.sends.send(None);
                audio_ssrc.map(|a| op12_payload(a, None)).into_iter().collect()
            }
            VideoCtl::Wants => self.wants().into_iter().collect(),
        }
    }

    /// Session Description (once per connection, media set up): `VideoRx::attach` and the
    /// video send task, spawned into `tasks` so it ends with the connection.
    pub fn start_media(&mut self, tasks: &mut JoinSet<()>, sock: Arc<UdpSocket>, crypt: Arc<Mutex<Crypt>>, dave: Arc<Mutex<Dave>>) {
        let (rtcp_tx, rtcp) = mpsc::unbounded_channel();
        let (feedback_tx, feedback) = mpsc::unbounded_channel();
        self.link.rx.attach(dave.clone(), rtcp_tx);
        self.link.rx.route_feedback(feedback_tx);
        let (Some(sends), Some(ssrcs)) = (self.sends_rx.take(), self.ssrcs) else { return };
        let task = SendTask::new(sock, crypt, dave, ssrcs, self.link.kind == StreamKind::Screen);
        tasks.spawn(task.run(sends, rtcp, feedback));
    }

    /// The receive side, for the UDP receive task.
    pub fn rx(&self) -> VideoRx {
        self.link.rx.clone()
    }
}

/// Ready `d` → my SSRCs (video 0 without a video stream: receive only).
fn ready_ssrcs(d: &Value) -> Option<Ssrcs> {
    #[derive(Deserialize)]
    struct Ready {
        ssrc: u32,
        #[serde(default)]
        streams: Vec<Stream>,
    }
    #[derive(Deserialize)]
    struct Stream {
        #[serde(default)]
        ssrc: u32,
        #[serde(default)]
        rtx_ssrc: u32,
    }
    let r = Ready::deserialize(d).ok()?;
    let (video, rtx) = r.streams.first().map_or((0, 0), |s| (s.ssrc, s.rtx_ssrc));
    Some(Ssrcs { audio: r.ssrc, video, rtx })
}

/// The next manager request; pending forever without a video link or once the manager let go.
pub(super) async fn next_ctl(video: &mut Option<VideoConn>) -> VideoCtl {
    match video {
        Some(v) => match v.link.ctl.recv().await {
            Some(ctl) => ctl,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// UDP receive task, for every packet that is not Opus RTP: `h` = Some for other RTP payload
/// types (video PT 103, RTX PT 104), None for RTCP and anything else. `pkt` is still
/// transport-encrypted; check `VideoRx::wants` before opening it.
pub(super) fn on_packet(rx: &VideoRx, crypt: &Mutex<Crypt>, pkt: &mut [u8], h: Option<&RtpHeader>) {
    match h {
        Some(h) if matches!(h.payload_type, H264_PT | H264_RTX_PT) => {
            if !rx.wants(h.ssrc) {
                return;
            }
            let marker = pkt[1] & 0x80 != 0;
            let Some(plain) = lock(crypt).open(pkt, h.clear_len) else { return };
            let Some(payload) = h.payload(plain) else { return };
            let p = RtpPacket { ssrc: h.ssrc, seq: h.seq, timestamp: h.timestamp, marker, payload };
            rx.push(h.payload_type == H264_RTX_PT, p);
        }
        // RTCP (discovery and ping replies start with 0x00 / 0x13).
        None if pkt.len() > RTCP_HEADER && pkt[0] >> 6 == 2 && (192..=223).contains(&pkt[1]) => {
            let Some(len) = lock(crypt).open(pkt, RTCP_HEADER).map(<[u8]>::len) else { return };
            for fb in rtp_h264::parse_feedback(&pkt[..RTCP_HEADER + len]) {
                rx.feedback(fb);
            }
        }
        _ => {}
    }
}

/// The video send task of one connection: my video out, RTX on NACK, keyframes on PLI, Sender
/// Reports, and the receive thread's NACK/PLI out.
struct SendTask {
    sock: Arc<UdpSocket>,
    crypt: Arc<Mutex<Crypt>>,
    dave: Arc<Mutex<Dave>>,
    ssrcs: Ssrcs,
    /// Screen content (header extension 7).
    screen: bool,
    seq: u16,
    rtx_seq: u16,
    /// Capture time of the first frame and its RTP timestamp (random start).
    clock: Option<(Instant, u32)>,
    ts_start: u32,
    history: RtxHistory,
    /// RTP packets and payload octets sent on the video SSRC (Sender Report; wrap like RTCP's).
    packets: u32,
    octets: u32,
    /// Header + extension bytes of every packet this task writes (constant per connection).
    header_len: usize,
    /// One reusable packet buffer.
    pkt: Vec<u8>,
    last_keyframe_request: Option<Instant>,
    /// A frame was dropped: deltas would decode into garbage (the sequence numbers have no
    /// gap), so only a keyframe goes out next.
    wait_keyframe: bool,
}

impl SendTask {
    fn new(sock: Arc<UdpSocket>, crypt: Arc<Mutex<Crypt>>, dave: Arc<Mutex<Dave>>, ssrcs: Ssrcs, screen: bool) -> SendTask {
        let r = getrandom::u64().unwrap_or(0);
        let mut pkt = Vec::with_capacity(1500);
        rtp_h264::write_video_header(&mut pkt, H264_PT, 0, 0, 0, false, screen);
        let header_len = pkt.len();
        SendTask {
            sock,
            crypt,
            dave,
            ssrcs,
            screen,
            seq: r as u16,
            rtx_seq: (r >> 16) as u16,
            clock: None,
            ts_start: (r >> 32) as u32,
            history: RtxHistory::default(),
            packets: 0,
            octets: 0,
            header_len,
            pkt,
            last_keyframe_request: None,
            wait_keyframe: false,
        }
    }

    async fn run(mut self, mut sends: mpsc::UnboundedReceiver<SendReq>, mut rtcp: mpsc::UnboundedReceiver<Vec<u8>>, mut feedback: mpsc::UnboundedReceiver<Feedback>) {
        let mut current: Option<VideoSend> = None;
        let mut reports = tokio::time::interval_at(tokio::time::Instant::now() + SR_INTERVAL, SR_INTERVAL);
        reports.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                req = sends.recv() => match req {
                    Some(Some(send)) => {
                        send.keyframe.store(true, Ordering::Relaxed); // receivers start with a keyframe
                        current = Some(send);
                    }
                    Some(None) => current = self.stopped(),
                    None => return,
                },
                frame = next_frame(&mut current) => match frame {
                    Some(f) => {
                        if !self.send_frame(&f).await
                            && let Some(c) = &current
                        {
                            c.keyframe.store(true, Ordering::Relaxed); // a gap: restart the reference chain
                        }
                    }
                    None => current = self.stopped(), // the pipeline stopped
                },
                _ = reports.tick(), if current.is_some() => self.send_report().await,
                Some(plain) = rtcp.recv() => self.send_rtcp(plain).await,
                Some(fb) = feedback.recv() => self.on_feedback(fb, current.as_ref()).await,
            }
        }
    }

    /// Nothing is sent any more: free the RTX history (no NACK is answered from it). The clock
    /// stays, so a restarted video's timestamps keep increasing on the same SSRC. Returns the
    /// new `current`.
    fn stopped(&mut self) -> Option<VideoSend> {
        self.history = RtxHistory::default();
        None
    }

    /// 90 kHz timestamp of a frame captured at `at`.
    fn timestamp(&mut self, at: Instant) -> u32 {
        let (start, ts) = *self.clock.get_or_insert((at, self.ts_start));
        let ticks = at.saturating_duration_since(start).as_micros() * u128::from(VIDEO_CLOCK_HZ) / 1_000_000;
        ts.wrapping_add(ticks as u32) // wraps like the timestamp itself
    }

    /// One frame: DAVE, packetize, seal, send. False = dropped (DAVE not ready: never sent in
    /// clear; or a delta after a dropped frame).
    async fn send_frame(&mut self, f: &EncodedFrame) -> bool {
        if self.wait_keyframe && !f.keyframe {
            return false;
        }
        let encrypted = lock(&self.dave).encrypt_h264(&f.annexb);
        let Some(frame) = encrypted else {
            self.wait_keyframe = true;
            return false;
        };
        self.wait_keyframe = false;
        let ts = self.timestamp(f.captured);
        let payloads = rtp_h264::packetize(&frame, MAX_PAYLOAD);
        let now = Instant::now();
        for (i, payload) in payloads.iter().enumerate() {
            let marker = i + 1 == payloads.len();
            let clear = rtp_h264::write_video_header(&mut self.pkt, H264_PT, self.seq, ts, self.ssrcs.video, marker, self.screen);
            self.pkt.extend_from_slice(payload);
            self.history.keep(self.seq, &self.pkt, now); // plain: header (timestamp, marker) + payload
            self.seq = self.seq.wrapping_add(1);
            self.packets = self.packets.wrapping_add(1);
            self.octets = self.octets.wrapping_add(payload.len() as u32);
            if lock(&self.crypt).seal(&mut self.pkt, clear).is_ok() {
                let _ = self.sock.send(&self.pkt).await;
            }
        }
        true
    }

    /// Sender Report for my video (wallclock to RTP timestamp, counts), sealed; none before the
    /// first frame went out.
    async fn send_report(&mut self) {
        if self.clock.is_none() {
            return;
        }
        let ts = self.timestamp(Instant::now());
        let mut sr = sender_report(self.ssrcs.video, SystemTime::now(), ts, self.packets, self.octets);
        if lock(&self.crypt).seal(&mut sr, RTCP_HEADER).is_ok() {
            let _ = self.sock.send(&sr).await;
        }
    }

    /// NACK/PLI from my receive thread: my SSRC as sender, sealed.
    async fn send_rtcp(&mut self, mut plain: Vec<u8>) {
        if plain.len() < RTCP_HEADER {
            return;
        }
        plain[4..8].copy_from_slice(&self.ssrcs.audio.to_be_bytes());
        if lock(&self.crypt).seal(&mut plain, RTCP_HEADER).is_ok() {
            let _ = self.sock.send(&plain).await;
        }
    }

    async fn on_feedback(&mut self, fb: Feedback, current: Option<&VideoSend>) {
        match fb {
            Feedback::Pli { media_ssrc } if media_ssrc == self.ssrcs.video => {
                let now = Instant::now();
                if let Some(c) = current
                    && self.last_keyframe_request.is_none_or(|t| now.saturating_duration_since(t) >= MIN_KEYFRAME_GAP)
                {
                    self.last_keyframe_request = Some(now);
                    c.keyframe.store(true, Ordering::Relaxed);
                }
            }
            Feedback::Nack { media_ssrc, lost } if current.is_some() && media_ssrc == self.ssrcs.video && self.ssrcs.rtx != 0 => {
                for osn in lost {
                    if self.rtx_packet(osn) {
                        let _ = self.sock.send(&self.pkt).await;
                    }
                }
            }
            _ => {}
        }
    }

    /// The RTX packet (RFC 4588: PT 104 on the RTX SSRC, own sequence, original timestamp and
    /// marker, payload = original sequence number + original payload) for `osn` into `pkt`.
    /// False when it is no longer kept.
    fn rtx_packet(&mut self, osn: u16) -> bool {
        let Some(orig) = self.history.get(osn) else { return false };
        let (ts, marker) = (u32::from_be_bytes([orig[4], orig[5], orig[6], orig[7]]), orig[1] & 0x80 != 0);
        let clear = rtp_h264::write_video_header(&mut self.pkt, H264_RTX_PT, self.rtx_seq, ts, self.ssrcs.rtx, marker, self.screen);
        self.pkt.extend_from_slice(&osn.to_be_bytes());
        self.pkt.extend_from_slice(&orig[self.header_len..]);
        self.rtx_seq = self.rtx_seq.wrapping_add(1);
        lock(&self.crypt).seal(&mut self.pkt, clear).is_ok()
    }
}

/// Plain RTCP Sender Report (PT 200) without report blocks: NTP wallclock `now`, the RTP
/// timestamp of that instant, my packet and payload octet counts.
fn sender_report(ssrc: u32, now: SystemTime, rtp_ts: u32, packets: u32, octets: u32) -> Vec<u8> {
    let unix = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = (unix.as_secs() + NTP_UNIX_OFFSET) as u32; // wraps with the NTP era, as it should
    let frac = ((u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut out = Vec::with_capacity(28 + 32); // + AEAD tag and nonce
    out.extend_from_slice(&[0x80, 200, 0, 6]);
    for word in [ssrc, secs, frac, rtp_ts, packets, octets] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out
}

/// The running pipeline's next frame; None = it stopped; pending while nothing is sent.
async fn next_frame(current: &mut Option<VideoSend>) -> Option<EncodedFrame> {
    match current {
        Some(s) => s.frames.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Receiver;
    use super::super::packet::{Mode, parse_rtp};
    use super::*;
    use crate::backend::{BackendEvent, EventSink};
    use crate::voice::video::codec::{self, Encoder, I420};
    use crate::voice::video::{ENCODED_QUEUE, PixelFormat, RawFrame, SendLimits, VideoEvent, VideoFrames, VideoKey};
    use crate::voice::{VoiceEvent, VoiceEvents};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn sink() -> (VoiceEvents, std::sync::mpsc::Receiver<BackendEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (VoiceEvents(EventSink::new(tx, egui::Context::default())), rx)
    }

    fn conn(rx: &VideoRx) -> VideoConn {
        VideoConn::new(VideoLink { kind: StreamKind::Camera, rx: rx.clone(), ctl: mpsc::unbounded_channel().1 })
    }

    fn sending() -> (mpsc::Sender<EncodedFrame>, Arc<AtomicBool>, VideoSend) {
        let (tx, frames) = mpsc::channel(ENCODED_QUEUE);
        let keyframe = Arc::new(AtomicBool::new(false));
        (tx, keyframe.clone(), VideoSend { kind: StreamKind::Camera, limits: SendLimits::CAMERA, frames, keyframe })
    }

    #[test]
    fn ready_ssrcs_and_manager_requests() {
        let ready = json!({"ssrc":1,"ip":"203.0.113.7","port":5,"modes":[],"streams":[{"type":"video","rid":"100","ssrc":2,"rtx_ssrc":3,"active":false}]});
        assert_eq!(ready_ssrcs(&ready), Some(Ssrcs { audio: 1, video: 2, rtx: 3 }));
        let receive_only = Some(Ssrcs { audio: 1, video: 0, rtx: 0 });
        assert_eq!(ready_ssrcs(&json!({"ssrc":1,"streams":[]})), receive_only, "NACK/PLI still go out");
        assert_eq!(ready_ssrcs(&json!({"ssrc":1})), receive_only);
        assert_eq!(ready_ssrcs(&json!({"streams":[]})), None);

        let rx = VideoRx::new(Arc::default(), sink().0, None);
        let mut v = conn(&rx);
        let (tx, _, send) = sending();
        assert!(v.on_ctl(VideoCtl::Send(send), Some(1)).is_empty(), "no SSRCs before Ready");
        assert!(tx.is_closed(), "the pipeline learns it at once");
        v.on_ready(&json!({"ssrc":1,"streams":[]}));
        let (tx, _, send) = sending();
        assert!(v.on_ctl(VideoCtl::Send(send), Some(1)).is_empty() && tx.is_closed(), "no video SSRC");
        v.on_ready(&ready);
        let (tx, _, send) = sending();
        assert_eq!(v.on_ctl(VideoCtl::Send(send), Some(1)), [op12_payload(1, Some((2, 3, StreamKind::Camera, SendLimits::CAMERA)))]);
        assert!(!tx.is_closed(), "kept for the send task");
        assert_eq!(v.on_ctl(VideoCtl::StopSending, Some(1)), [op12_payload(1, None)]);

        rx.set_wanted(&VideoKey::User(Id(5)), true);
        assert!(v.on_ctl(VideoCtl::Wants, Some(1)).is_empty(), "nothing to ask for yet");
        assert_eq!(v.on_op12(&json!({"user_id":"5","video_ssrc":7,"streams":[{"ssrc":7,"rtx_ssrc":8}]})), Some(op15_payload(&[7])));
        assert_eq!(v.on_op12(&json!({"user_id":"6","video_ssrc":9})), None, "not shown: no op 15");
        assert_eq!(v.on_op12(&json!({"video_ssrc":9})), None);
        assert!(v.on_ctl(VideoCtl::Wants, Some(1)).is_empty(), "unchanged");
        v.on_client_disconnect(Id(5));
        assert!(!rx.wants(7) && !rx.wants(8));
        assert_eq!(v.on_ctl(VideoCtl::Wants, Some(1)), [op15_payload(&[])], "the last one went away");

        let mut early = conn(&VideoRx::new(Arc::default(), sink().0, None));
        early.link.rx.set_wanted(&VideoKey::User(Id(5)), true);
        early.link.rx.on_remote(crate::voice::video::RemoteVideo { user_id: Id(5), ssrc: 7, rtx_ssrc: 0, active: true });
        assert!(early.on_ctl(VideoCtl::Wants, None).is_empty(), "never before Ready");
    }

    #[tokio::test]
    async fn nothing_in_clear_and_no_deltas_after_a_dropped_frame() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let task = |dave: Dave| async {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            sock.connect(peer.local_addr().unwrap()).await.unwrap();
            let crypt = Arc::new(Mutex::new(Crypt::new(Mode::XChaCha20, &[5; 32])));
            SendTask::new(Arc::new(sock), crypt, Arc::new(Mutex::new(dave)), Ssrcs { audio: 1, video: 2, rtx: 3 }, false)
        };
        let frame = |keyframe| EncodedFrame { annexb: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }, 1, 2, 3], keyframe, captured: Instant::now() };
        let mut buf = [0u8; 1500];

        let mut dave = Dave::new(Id(1), Id(2));
        dave.on_session_description(1); // DAVE on, no group key yet
        let mut on = task(dave).await;
        assert!(!on.send_frame(&frame(true)).await, "DAVE not ready: dropped");
        assert!(peer.try_recv(&mut buf).is_err(), "nothing went out");

        let mut off = task(Dave::new(Id(1), Id(2))).await;
        off.wait_keyframe = true;
        assert!(!off.send_frame(&frame(false)).await, "a delta after a gap");
        assert!(off.send_frame(&frame(true)).await);
        let n = peer.recv(&mut buf).await.unwrap();
        let h = parse_rtp(&buf[..n]).unwrap();
        assert_eq!((h.payload_type, h.ssrc, buf[1] & 0x80 != 0), (H264_PT, 2, true), "one packet, marker set");
        let plain = Crypt::new(Mode::XChaCha20, &[5; 32]);
        assert_eq!(h.payload(plain.open(&mut buf[..n], h.clear_len).unwrap()), Some(&[0x65, 1, 2, 3][..]));

        // RTX: original timestamp and marker, the original sequence number first.
        assert!(off.rtx_packet(h.seq));
        let mut rtx = off.pkt.clone();
        let r = parse_rtp(&rtx).unwrap();
        assert_eq!((r.payload_type, r.ssrc, r.timestamp, rtx[1] & 0x80 != 0), (H264_RTX_PT, 3, h.timestamp, true));
        let body = r.payload(plain.open(&mut rtx, r.clear_len).unwrap()).unwrap();
        assert_eq!(rtp_h264::unwrap_rtx(body), Some((h.seq, &[0x65, 1, 2, 3][..])));
        assert!(!off.rtx_packet(h.seq.wrapping_sub(5)), "never sent");
        assert_eq!((off.packets, off.octets), (1, 4), "counted for the Sender Report");

        // Stopped: the history is freed and NACKs are not answered.
        let _ = off.stopped();
        assert!(!off.rtx_packet(h.seq));
        off.on_feedback(Feedback::Nack { media_ssrc: 2, lost: vec![h.seq] }, None).await;
        assert!(peer.try_recv(&mut buf).is_err(), "nothing resent");
    }

    #[test]
    fn sender_report_layout() {
        let at = UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000);
        let sr = sender_report(0xAABB_CCDD, at, 0x0102_0304, 7, 0x1_0000);
        let word = |i: usize| u32::from_be_bytes([sr[i], sr[i + 1], sr[i + 2], sr[i + 3]]);
        assert_eq!(sr.len(), 28, "header + sender info, no report blocks");
        assert_eq!(sr[..4], [0x80, 200, 0, 6], "V=2, RC=0, PT 200, 6 words after the first");
        assert_eq!(word(4), 0xAABB_CCDD, "sender SSRC (clear under rtpsize AEAD)");
        assert_eq!((word(8), word(12)), ((1_700_000_000 + NTP_UNIX_OFFSET) as u32, 1 << 31), "NTP seconds, half a second");
        assert_eq!((word(16), word(20), word(24)), (0x0102_0304, 7, 0x1_0000));
        assert!(rtp_h264::parse_feedback(&sr).is_empty(), "not feedback");
    }

    /// One side of the loopback: a video connection with Ready SSRCs `ssrc` (audio), `ssrc + 1`
    /// (video), `ssrc + 2` (RTX), media started, and its UDP receive task.
    fn side(tasks: &mut JoinSet<()>, sock: Arc<UdpSocket>, ssrc: u32, frames: Arc<VideoFrames>, events: VoiceEvents) -> VideoConn {
        let crypt = || Arc::new(Mutex::new(Crypt::new(Mode::Aes256Gcm, &[9; 32])));
        let dave = Arc::new(Mutex::new(Dave::new(Id(u64::from(ssrc)), Id(2)))); // DAVE off: passthrough
        let rx = VideoRx::new(frames, events, None);
        let mut v = conn(&rx);
        v.on_ready(&json!({"ssrc": ssrc, "streams": [{"ssrc": ssrc + 1, "rtx_ssrc": ssrc + 2}]}));
        v.start_media(tasks, sock.clone(), crypt(), dave.clone());
        let playback = crate::voice::audio::test_hooks::speaker().0;
        let receiver = Receiver { sock, crypt: crypt(), dave, users: Arc::default(), playback, video: Some(rx), stream_owner: None };
        tasks.spawn(receiver.run());
        v
    }

    /// Offline self-test of the whole video path: synthetic picture → openh264 encode → DAVE
    /// (off) → packetize → seal → UDP → fake SFU, which loses one packet of the keyframe →
    /// open → depacketize; the gap is NACKed over sealed RTCP, the sender answers with RTX →
    /// depacketize → decode → frame slot + size event.
    #[tokio::test]
    async fn video_loopback_recovers_a_lost_packet_by_rtx() {
        const W: u32 = 320;
        const H: u32 = 240;
        let sfu = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut socks = Vec::new();
        for _ in 0..2 {
            let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            s.connect(sfu.local_addr().unwrap()).await.unwrap();
            socks.push(Arc::new(s));
        }
        let (a_addr, b_addr) = (socks[0].local_addr().unwrap(), socks[1].local_addr().unwrap());
        let rtx_relayed = Arc::new(AtomicUsize::new(0));
        let counter = rtx_relayed.clone();
        tokio::spawn(async move {
            let (mut buf, mut video) = ([0u8; 1500], 0);
            while let Ok((n, from)) = sfu.recv_from(&mut buf).await {
                if from != a_addr {
                    let _ = sfu.send_to(&buf[..n], a_addr).await; // B's RTCP
                    continue;
                }
                match buf[1] & 0x7F {
                    H264_PT => {
                        video += 1;
                        if video == 3 {
                            continue; // lost
                        }
                    }
                    H264_RTX_PT => _ = counter.fetch_add(1, Ordering::Relaxed),
                    _ => {}
                }
                let _ = sfu.send_to(&buf[..n], b_addr).await;
            }
        });

        let mut tasks = JoinSet::new();
        // A (user 10) sends its camera; B shows it.
        let mut a = side(&mut tasks, socks[0].clone(), 10, Arc::default(), sink().0);
        let (b_frames, (b_events, b_ui)) = (Arc::new(VideoFrames::default()), sink());
        let mut b = side(&mut tasks, socks[1].clone(), 20, b_frames.clone(), b_events);
        let key = VideoKey::User(Id(10));
        b.rx().set_wanted(&key, true);
        assert!(b.on_op12(&json!({"user_id": "10", "video_ssrc": 11, "streams": [{"ssrc": 11, "rtx_ssrc": 12}]})).is_some());
        let (tx, keyframe, send) = sending();
        assert_eq!(a.on_ctl(VideoCtl::Send(send), Some(10)).len(), 1, "op 12");

        // Left half noise (a keyframe of many packets), right half one colour.
        let mut seed = 1u32;
        let bgra: Vec<u8> = (0..W * H)
            .flat_map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (seed >> 24) as u8;
                if i % W < W / 2 { [n, n, n, 255] } else { [60, 128, 200, 255] }
            })
            .collect();
        let mut picture = I420::new(W, H);
        codec::to_i420(&RawFrame { width: W, height: H, stride: W as usize * 4, format: PixelFormat::Bgra, data: &bgra }, &mut picture);
        let mut enc = Encoder::new(W, H, SendLimits::CAMERA, false).unwrap();
        let t0 = Instant::now();
        for i in 0..6u32 {
            let at = t0 + Duration::from_millis(u64::from(33 * i));
            if let Some(f) = enc.encode(&picture, keyframe.swap(false, Ordering::Relaxed), at).unwrap() {
                assert!(i > 0 || (f.keyframe && f.annexb.len() > 3 * MAX_PAYLOAD), "the lost packet is inside the first keyframe");
                tx.send(f).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(33)).await;
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let image = loop {
            if let Some((_, image)) = b_frames.latest(&key) {
                break image;
            }
            assert!(Instant::now() < deadline, "nothing decoded");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(rtx_relayed.load(Ordering::Relaxed) >= 1, "the lost packet came back as RTX");
        assert_eq!(image.size, [W as usize, H as usize]);
        let px = image.pixels[(H / 2 * W + W * 3 / 4) as usize];
        assert!(px.r().abs_diff(200) <= 8 && px.g().abs_diff(128) <= 8 && px.b().abs_diff(60) <= 8, "colour survives: {px:?}");
        let events: Vec<_> = b_ui.try_iter().filter_map(|e| if let BackendEvent::Voice(VoiceEvent::Video(v)) = e { Some(v) } else { None }).collect();
        let sized = VideoEvent::RemoteVideo { key: key.clone(), width: W, height: H };
        assert_eq!(events, [VideoEvent::RemoteVideo { key, width: 0, height: 0 }, sized], "announced, then its size");
    }
}
