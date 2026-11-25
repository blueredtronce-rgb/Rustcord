//! Voice transport: voice WebSocket, UDP, IP discovery, transport encryption, RTP.
//!
//! OWNERSHIP: transport engineer. Keep `ConnectInfo`, `End` and `run` as they are (the manager
//! in mod.rs uses them). Packet helpers (discovery, RTP, AEAD) live in `transport/packet.rs`.
//! Spec: VOICE_NOTES §2.1–2.9 (`wss://{endpoint}/?v=9` – the notes supersede "v8"), DAVE
//! opcode routing §3.3 (handled by `dave::Dave`).
//!
//! Tasks inside `run` (all tokio, owned by `run`'s future so a stop or drop ends them):
//! - WebSocket loop (`run` itself): Identify/Resume, heartbeat with `seq_ack`, ops
//!   2/3/4/5/6/8/9/11/13, DAVE JSON ops 21/22/24 and binary frames into `Dave`, sending its
//!   `Reply`s; op 5 speaking whenever the send task's `speaking` flag changes.
//! - UDP receive: demux (§2.6), transport-decrypt, RTP parse, SSRC -> user (op 5 map; unknown
//!   SSRCs wait ≤200 ms, never guessed), `Dave::decrypt`, `Playback::push`.
//! - UDP send: `media.captured` -> `Dave::encrypt_opus` -> RTP -> encrypt -> socket; keepalive
//!   ping every 5 s.
//!
//! Secrets: the voice token, `secret_key` and DAVE keys are never logged, Debug-printed or put
//! into `End::Failed` text. Memory: one reusable packet buffer per direction (≤1500 bytes).

mod packet;
mod video;

use self::packet::{Crypt, Mode, OPUS_PT, RTP_HEADER, RtpHeader, discovery_request, parse_discovery, parse_rtp, uuid_v4, write_rtp_header};
use super::audio::{Captured, FRAME_SAMPLES, MediaLink, Playback, SAMPLE_RATE};
use super::video::trace::Trace;
use super::dave::{self, Dave, Reply};
use super::video::{H264_PT, H264_RTX_PT, StreamKind, VideoCtl, VideoLink, receive::VideoRx};
use crate::api::props::ClientProps;
use crate::auth::Secret;
use crate::model::Id;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::header::{self, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zeroize::Zeroize;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_TRIES: u32 = 3;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);
/// Resume attempts in a row (1, 2, 4, 8, 16 s apart) before giving up.
const MAX_RESUMES: u32 = 5;
/// A connection that lived this long resets the resume counter.
const STABLE: Duration = Duration::from_secs(60);
/// UDP ping `13 37 CA FE` (NAT keepalive while silent).
const KEEPALIVE: Duration = Duration::from_secs(5);
/// Packets of an SSRC no op 5 has named yet: kept this long, at most this many in total.
const UNKNOWN_WAIT: Duration = Duration::from_millis(200);
const UNKNOWN_MAX: usize = 25;
const MTU: usize = 1500;
/// Tests talk to a plain local WebSocket (the fake voice server below).
const SCHEME: &str = if cfg!(test) { "ws" } else { "wss" };

/// Everything needed to open one voice connection (from my VOICE_STATE_UPDATE and the
/// VOICE_SERVER_UPDATE).
#[derive(Debug)]
pub struct ConnectInfo {
    /// Identify/Resume `server_id`: the guild id, or the DM/group channel id for calls.
    pub server_id: Id,
    /// Identify `channel_id`.
    pub channel_id: Id,
    /// Me.
    pub user_id: Id,
    /// Main-gateway session id.
    pub session_id: String,
    /// Voice token (Debug-redacted). Never log it.
    pub token: Secret,
    /// `host:port` without a scheme; connect to `wss://{endpoint}/?v=9`.
    pub endpoint: String,
    /// DAVE MLS group id: `channel_id` for voice, `rtc_server_id - 1` for Go Live.
    pub dave_group: Id,
    /// Video of this connection (docs/VIDEO_DESIGN.md); None = audio only (no H264 declared).
    pub video: Option<VideoLink>,
}

/// How a connection ended; the manager decides what follows.
#[derive(Debug, PartialEq)]
pub enum End {
    /// `stop` resolved (leave, channel switch, new token, logout).
    Stopped,
    /// Close 4014 (kicked, moved, main session gone). Never reconnect on our own: the main
    /// gateway's next VOICE_* dispatches decide.
    Kicked,
    /// Close 4004/4006/4009/4011, or a failed resume: the manager sends op 4 again.
    Rejoin,
    /// Give up (4017 E2EE required, 4021 rate limited, 4022 call ended, protocol errors,
    /// resume retries used up after 1, 2, 4 … ≤30 s, ≤5 tries). User-facing text, no secrets.
    Failed(String),
}

/// Runs one voice connection until it ends.
///
/// - `info`: from the manager; `info.token` is used for Identify/Resume only.
/// - `media`: the call's audio link. Read `media.captured` (20 ms Opus frames, `End` after the
///   trailing silence frames) and feed received frames to `media.playback`.
/// - `stop`: resolves (with `Ok` or `Err`) when the manager drops the call; then close the
///   WebSocket with 1000 and return `End::Stopped` promptly.
/// - `connected`: call once, after Session Description (op 4) when media can flow.
///
/// Resumes by itself on 4015/4013/network loss (§2.4, keeping UDP and keys); everything else
/// is returned as `End`. Runs on the tokio runtime; the returned future must be `Send`.
pub async fn run(info: ConnectInfo, media: MediaLink, stop: oneshot::Receiver<()>, connected: impl FnOnce() + Send + 'static) -> End {
    let mut ws = None;
    let mut conn = Conn::new(info, media, Box::new(connected));
    let end = tokio::select! {
        end = conn.run(&mut ws) => end,
        _ = stop => End::Stopped,
    };
    if let Some(ws) = &mut ws {
        let frame = CloseFrame { code: 1000.into(), reason: Default::default() };
        let _ = timeout(CLOSE_TIMEOUT, ws.close(Some(frame))).await;
    }
    end // dropping `conn` aborts the UDP tasks
}

/// Why one WebSocket connection ended.
#[derive(Debug, PartialEq)]
enum Exit {
    /// Network loss, zombie or a resumable close: resume (or rejoin before media was set up).
    Lost,
    End(End),
}

fn failed(msg: &str) -> Exit {
    Exit::End(End::Failed(msg.into()))
}

/// What a close code from the voice server means (§2.5).
fn close_exit(code: u16) -> Exit {
    match code {
        4014 => Exit::End(End::Kicked),
        4004 | 4006 | 4009 | 4011 => Exit::End(End::Rejoin),
        4017 => failed("This call needs end-to-end encryption, which could not be set up."),
        4021 => failed("Voice is rate limited right now. Try again later."),
        4022 => failed("The call has ended."),
        4001 | 4002 | 4003 | 4005 | 4012 | 4016 | 4020 => Exit::End(End::Failed(format!("The voice connection failed (error {code})."))),
        // 4013/4015 (voice server crashed), 1000/1001/1006 and anything unknown.
        _ => Exit::Lost,
    }
}

/// Delay before the n-th resume attempt (n ≥ 1): 1, 2, 4 … ≤ 30 s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.saturating_sub(1).min(5)).min(30))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `d` of a JSON payload; None if it doesn't parse. Errors are never printed: serde quotes
/// values, and op 4 carries the `secret_key`.
fn data<T: DeserializeOwned>(text: &str) -> Option<T> {
    #[derive(Deserialize)]
    struct Payload<T> {
        d: T,
    }
    serde_json::from_str::<Payload<T>>(text).ok().map(|p| p.d)
}

/// The connection state that survives resumes.
struct Conn {
    info: ConnectInfo,
    props: ClientProps,
    playback: Playback,
    /// Moved into the send task at Session Description.
    captured: Option<mpsc::Receiver<Captured>>,
    connected: Option<Box<dyn FnOnce() + Send>>,
    dave: Arc<Mutex<Dave>>,
    /// SSRC -> user, from op 5.
    users: Arc<Mutex<HashMap<u32, Id>>>,
    /// Last `seq` of a JSON payload or binary frame; -1 = none.
    seq_ack: i64,
    /// From Ready: the connected socket and my SSRC.
    udp: Option<(Arc<UdpSocket>, u32)>,
    /// From Session Description; shared with the UDP tasks. Some = media is set up (resumable).
    crypt: Option<Arc<Mutex<Crypt>>>,
    /// Set by the send task (Some until it starts); op 5 follows it.
    speaking_tx: Option<watch::Sender<bool>>,
    speaking: watch::Receiver<bool>,
    sent_speaking: Option<bool>,
    beat_interval: Option<Duration>,
    next_beat: Instant,
    acked: bool,
    resumes: u32,
    tasks: JoinSet<()>,
    /// Video hooks (`transport/video.rs`); None for audio-only connections.
    video: Option<video::VideoConn>,
}

impl Conn {
    fn new(mut info: ConnectInfo, media: MediaLink, connected: Box<dyn FnOnce() + Send>) -> Conn {
        let (speaking_tx, speaking) = watch::channel(false);
        Conn {
            // Same upgrade headers as the main gateway (user agent, locale).
            props: ClientProps::new(&crate::config::Settings::load()),
            playback: media.playback,
            captured: Some(media.captured),
            connected: Some(connected),
            dave: Arc::new(Mutex::new(Dave::new(info.user_id, info.dave_group))),
            video: info.video.take().map(video::VideoConn::new),
            info,
            users: Arc::default(),
            seq_ack: -1,
            udp: None,
            crypt: None,
            speaking_tx: Some(speaking_tx),
            speaking,
            sent_speaking: None,
            beat_interval: None,
            next_beat: Instant::now(),
            acked: true,
            resumes: 0,
            tasks: JoinSet::new(),
        }
    }

    /// Connects, and resumes after losses, until the connection ends. `slot` holds the open
    /// WebSocket so `run` can close it (1000) after a stop or an `End`.
    async fn run(&mut self, slot: &mut Option<Ws>) -> End {
        let url = format!("{SCHEME}://{}/?v=9", self.info.endpoint);
        loop {
            let started = Instant::now();
            let exit = match connect(&url, &self.props).await {
                Some(ws) => {
                    let Err(exit) = self.serve(slot.insert(ws)).await;
                    exit
                }
                None => Exit::Lost,
            };
            if let Exit::End(end) = exit {
                return end;
            }
            *slot = None; // lost: dropped without a close frame, the session stays resumable
            // Before Session Description there is nothing to resume: a fresh op 4 instead.
            if self.crypt.is_none() {
                return End::Rejoin;
            }
            if started.elapsed() >= STABLE {
                self.resumes = 0;
            }
            self.resumes += 1;
            if self.resumes > MAX_RESUMES {
                return End::Failed("Lost the voice connection.".into());
            }
            sleep(backoff(self.resumes)).await;
        }
    }

    /// One WebSocket connection: Identify (or Resume once media is set up), then the loop.
    async fn serve(&mut self, ws: &mut Ws) -> Result<Infallible, Exit> {
        let hello = match self.crypt {
            Some(_) => resume_payload(&self.info, self.seq_ack),
            None => identify_payload(&self.info, self.video.as_ref().map(video::VideoConn::kind)),
        };
        send(ws, hello).await?;
        (self.beat_interval, self.next_beat, self.acked) = (None, Instant::now() + HELLO_TIMEOUT, true);
        loop {
            tokio::select! {
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(t))) => self.on_text(ws, t.as_str()).await?,
                    Some(Ok(Message::Binary(b))) => self.on_binary(ws, &b).await?,
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map_or(1005, |f| u16::from(f.code));
                        let reason = frame.as_ref().map(|f| f.reason.as_str()).unwrap_or_default();
                        eprintln!("voice: closed by server ({code} {reason})");
                        return Err(close_exit(code));
                    }
                    Some(Ok(_)) => {} // ping/pong are answered by tungstenite
                    Some(Err(e)) => {
                        eprintln!("voice: connection error: {e}");
                        return Err(Exit::Lost);
                    }
                    None => return Err(Exit::Lost),
                },
                _ = sleep_until(self.next_beat) => {
                    let Some(interval) = self.beat_interval else {
                        eprintln!("voice: no hello received");
                        return Err(Exit::Lost);
                    };
                    if !self.acked {
                        eprintln!("voice: heartbeat not acknowledged, resuming");
                        return Err(Exit::Lost);
                    }
                    self.acked = false;
                    send(ws, heartbeat_payload(self.seq_ack)).await?;
                    self.next_beat = Instant::now() + interval;
                }
                Ok(()) = self.speaking.changed() => {
                    let on = *self.speaking.borrow_and_update();
                    self.send_speaking(ws, on).await?;
                }
                ctl = video::next_ctl(&mut self.video) => self.on_video_ctl(ws, ctl).await?,
                // A panicked UDP task would leave a silent call behind.
                Some(Err(_)) = self.tasks.join_next() => return Err(failed("Voice stopped after an internal error.")),
            }
        }
    }

    async fn on_text(&mut self, ws: &mut Ws, text: &str) -> Result<(), Exit> {
        #[derive(Deserialize)]
        struct Head {
            op: u8,
            seq: Option<i64>,
        }
        let Ok(head) = serde_json::from_str::<Head>(text) else {
            eprintln!("voice: unreadable payload");
            return Ok(());
        };
        if let Some(seq) = head.seq {
            self.seq_ack = seq;
        }
        match head.op {
            2 => self.on_ready(ws, text).await?,
            3 => send(ws, heartbeat_payload(self.seq_ack)).await?,
            4 => self.on_session_description(ws, text).await?,
            5 => {
                #[derive(Deserialize)]
                struct Speaking {
                    user_id: Id,
                    ssrc: u32,
                }
                if let Some(s) = data::<Speaking>(text) {
                    lock(&self.users).insert(s.ssrc, s.user_id);
                    if let Some(v) = &mut self.video {
                        v.set_stream_audio(s.user_id, self.info.user_id, s.ssrc);
                        if let Some(wants) = v.wants() { send(ws, wants).await?; }
                    }
                }
            }
            6 => self.acked = true,
            8 => {
                #[derive(Deserialize)]
                struct Hello {
                    heartbeat_interval: f64,
                }
                let ms = data::<Hello>(text).map_or(13_750.0, |h| h.heartbeat_interval).clamp(1_000.0, 60_000.0);
                let interval = Duration::from_secs_f64(ms / 1_000.0);
                (self.beat_interval, self.next_beat) = (Some(interval), Instant::now() + interval);
            }
            11 => {
                #[derive(Deserialize)]
                struct Clients {
                    user_ids: Vec<Id>,
                }
                if let Some(c) = data::<Clients>(text) {
                    lock(&self.dave).on_clients_connect(&c.user_ids);
                }
            }
            13 => {
                #[derive(Deserialize)]
                struct Client {
                    user_id: Id,
                }
                if let Some(Client { user_id }) = data::<Client>(text) {
                    lock(&self.dave).on_client_disconnect(user_id);
                    if let Some(v) = &mut self.video {
                        v.on_client_disconnect(user_id);
                        if let Some(wants) = v.wants() { send(ws, wants).await?; }
                    }
                    lock(&self.users).retain(|&ssrc, user| {
                        let gone = *user == user_id;
                        if gone {
                            self.playback.remove(ssrc);
                        }
                        !gone
                    });
                }
            }
            21 | 22 | 24 => {
                if let Some(d) = data::<serde_json::Value>(text) {
                    let replies = lock(&self.dave).on_json(head.op, &d);
                    send_replies(ws, replies).await?;
                }
            }
            12 => {
                if let (Some(v), Some(d)) = (&mut self.video, data::<serde_json::Value>(text)) {
                    if let (Some(user), Some(ssrc)) = (
                        serde_json::from_value::<Id>(d["user_id"].clone()).ok(),
                        d["audio_ssrc"].as_u64().and_then(|ssrc| u32::try_from(ssrc).ok()).filter(|&ssrc| ssrc != 0),
                    ) {
                        lock(&self.users).insert(ssrc, user);
                        v.set_stream_audio(user, self.info.user_id, ssrc);
                    }
                    if let Some(wants) = v.on_op12(&d) {
                        send(ws, wants).await?;
                    }
                }
            }
            _ => {} // 9 Resumed, 14/15/18/20: nothing to do
        }
        Ok(())
    }

    /// Server binary frame `[u16 BE seq][op][payload]`: DAVE MLS ops.
    async fn on_binary(&mut self, ws: &mut Ws, frame: &[u8]) -> Result<(), Exit> {
        let [s0, s1, op, payload @ ..] = frame else { return Ok(()) };
        self.seq_ack = i64::from(u16::from_be_bytes([*s0, *s1]));
        let replies = lock(&self.dave).on_binary(*op, payload);
        send_replies(ws, replies).await
    }

    /// Ready: open UDP, IP discovery, Select Protocol (once per session).
    async fn on_ready(&mut self, ws: &mut Ws, text: &str) -> Result<(), Exit> {
        #[derive(Deserialize)]
        struct Ready {
            ssrc: u32,
            ip: std::net::IpAddr,
            port: u16,
            #[serde(default)]
            modes: Vec<String>,
        }
        if self.udp.is_some() {
            return Ok(()); // a second Select Protocol closes with 4006
        }
        let ready = data::<Ready>(text).ok_or_else(|| failed("The voice server sent an unreadable reply."))?;
        let mode = Mode::pick(&ready.modes).ok_or_else(|| failed("The voice server offers no supported encryption."))?;
        let sock = open_udp(SocketAddr::new(ready.ip, ready.port), ready.ssrc)
            .await
            .ok_or_else(|| failed("Could not reach the voice server over UDP. A firewall may be blocking it."))?;
        let (ip, port) = sock.1;
        self.udp = Some((Arc::new(sock.0), ready.ssrc));
        if let (Some(v), Some(d)) = (&mut self.video, data::<serde_json::Value>(text)) {
            v.on_ready(&d);
        }
        let mut id = [0u8; 16];
        let _ = getrandom::fill(&mut id);
        send(ws, select_protocol_payload(&ip, port, mode, &uuid_v4(id), self.video.is_some())).await
    }

    /// Session Description: keys, DAVE, media tasks, `connected`.
    async fn on_session_description(&mut self, ws: &mut Ws, text: &str) -> Result<(), Exit> {
        #[derive(Deserialize)]
        struct Description {
            mode: String,
            secret_key: Vec<u8>,
            #[serde(default)]
            dave_protocol_version: u16,
        }
        let bad = || failed("The voice server sent an unreadable session.");
        let mut desc = data::<Description>(text).ok_or_else(bad)?;
        let key: Option<[u8; 32]> = desc.secret_key.as_slice().try_into().ok();
        desc.secret_key.zeroize();
        let (Some(mode), Some(mut key), Some((sock, ssrc))) = (Mode::from_wire(&desc.mode), key, self.udp.clone()) else {
            return Err(bad());
        };
        let crypt = Crypt::new(mode, &key);
        key.zeroize();
        // DAVE state first: the send task must never see the initial "DAVE off".
        let replies = lock(&self.dave).on_session_description(desc.dave_protocol_version);
        match &self.crypt {
            Some(shared) => *lock(shared) = crypt,
            None => {
                let shared = Arc::new(Mutex::new(crypt));
                self.spawn_media(sock, ssrc, shared.clone());
                self.crypt = Some(shared);
            }
        }
        send_replies(ws, replies).await?;
        if self.sent_speaking.is_none() {
            // Receive-only clients still announce their SSRC (§2.3 step 6).
            let on = *self.speaking.borrow();
            self.send_speaking(ws, on).await?;
        }
        if let Some(wants) = self.video.as_mut().and_then(video::VideoConn::wants) { send(ws, wants).await?; }
        if let Some(connected) = self.connected.take() {
            connected();
        }
        Ok(())
    }

    fn spawn_media(&mut self, sock: Arc<UdpSocket>, ssrc: u32, crypt: Arc<Mutex<Crypt>>) {
        let video = self.video.as_ref().map(video::VideoConn::rx);
        let stream_owner = video.as_ref().and_then(VideoRx::stream_owner).filter(|&owner| owner != self.info.user_id);
        let rx = Receiver {
            sock: sock.clone(),
            crypt: crypt.clone(),
            dave: self.dave.clone(),
            users: self.users.clone(),
            playback: self.playback.clone(),
            video,
            stream_owner,
        };
        self.tasks.spawn(rx.run());
        if let Some(v) = &mut self.video {
            v.start_media(&mut self.tasks, sock.clone(), crypt.clone(), self.dave.clone());
        }
        if let (Some(captured), Some(speaking)) = (self.captured.take(), self.speaking_tx.take()) {
            let tx = Sender { sock, ssrc, crypt, dave: self.dave.clone(), clock: RtpClock::new(), speaking };
            self.tasks.spawn(tx.run(captured));
        }
    }

    /// A video request from the manager: send what the video hooks answer (op 12, op 15).
    async fn on_video_ctl(&mut self, ws: &mut Ws, ctl: VideoCtl) -> Result<(), Exit> {
        let Some(v) = &mut self.video else { return Ok(()) };
        for payload in v.on_ctl(ctl, self.udp.as_ref().map(|u| u.1)) {
            send(ws, payload).await?;
        }
        Ok(())
    }

    async fn send_speaking(&mut self, ws: &mut Ws, on: bool) -> Result<(), Exit> {
        let Some((_, ssrc)) = self.udp else { return Ok(()) };
        if self.sent_speaking != Some(on) {
            let soundshare = self.video.as_ref().is_some_and(|v| v.kind() == StreamKind::Screen);
            send(ws, speaking_payload(on, ssrc, soundshare)).await?;
            self.sent_speaking = Some(on);
        }
        Ok(())
    }
}

async fn connect(url: &str, props: &ClientProps) -> Option<Ws> {
    let req = match upgrade_request(url, props) {
        Ok(req) => req,
        Err(e) => {
            eprintln!("voice: bad endpoint: {e}");
            return None;
        }
    };
    match timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Ok(Ok((ws, _))) => Some(ws),
        Ok(Err(e)) => {
            eprintln!("voice: connect failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("voice: connect timed out");
            None
        }
    }
}

/// The upgrade request a browser sends (same headers as the main gateway's).
fn upgrade_request(url: &str, props: &ClientProps) -> Result<Request, tokio_tungstenite::tungstenite::Error> {
    let mut req = url.into_client_request()?;
    let h = req.headers_mut();
    h.insert(header::USER_AGENT, HeaderValue::from_str(props.user_agent())?);
    h.insert(header::ORIGIN, HeaderValue::from_static("https://discord.com"));
    h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_str(&props.accept_language())?);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok(req)
}

async fn send(ws: &mut Ws, text: String) -> Result<(), Exit> {
    ws.send(Message::text(text)).await.map_err(|e| {
        eprintln!("voice: send failed: {e}");
        Exit::Lost
    })
}

async fn send_replies(ws: &mut Ws, replies: Vec<Reply>) -> Result<(), Exit> {
    for reply in replies {
        let msg = match reply {
            Reply::Json(v) => Message::text(v.to_string()),
            Reply::Binary(b) => Message::binary(b),
        };
        ws.send(msg).await.map_err(|_| Exit::Lost)?;
    }
    Ok(())
}

/// A UDP socket connected to the voice server, and my external `(ip, port)` from IP discovery
/// (3 tries, 1 s each).
async fn open_udp(server: SocketAddr, ssrc: u32) -> Option<(UdpSocket, (String, u16))> {
    let local: SocketAddr = if server.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let sock = UdpSocket::bind(local).await.ok()?;
    sock.connect(server).await.ok()?;
    let mut buf = [0u8; MTU];
    for _ in 0..DISCOVERY_TRIES {
        sock.send(&discovery_request(ssrc)).await.ok()?;
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        while let Ok(Ok(n)) = timeout_at(deadline, sock.recv(&mut buf)).await {
            if let Some(me) = parse_discovery(&buf[..n]) {
                return Some((sock, me));
            }
        }
    }
    None
}

/// With video: `video: true` and one stream of that kind, so Ready allocates its SSRCs.
fn identify_payload(info: &ConnectInfo, video: Option<StreamKind>) -> String {
    let (video, streams) = match video {
        Some(kind) => (true, kind.identify_streams()),
        None => (false, json!([])),
    };
    json!({"op": 0, "d": {
        "server_id": info.server_id, "channel_id": info.channel_id, "user_id": info.user_id,
        "session_id": info.session_id, "token": info.token.expose(),
        "video": video, "streams": streams, "max_dave_protocol_version": dave::MAX_PROTOCOL_VERSION,
    }})
    .to_string()
}

fn resume_payload(info: &ConnectInfo, seq_ack: i64) -> String {
    json!({"op": 7, "d": {
        "server_id": info.server_id, "channel_id": info.channel_id,
        "session_id": info.session_id, "token": info.token.expose(), "seq_ack": seq_ack,
    }})
    .to_string()
}

fn heartbeat_payload(seq_ack: i64) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    json!({"op": 3, "d": {"t": t, "seq_ack": seq_ack}}).to_string()
}

/// Opus/120, plus H264/103 (RTX 104) when `video`: the only video codec declared, so every
/// sender sends us H264 (VOICE_NOTES §5.1).
fn select_protocol_payload(ip: &str, port: u16, mode: Mode, rtc_connection_id: &str, video: bool) -> String {
    let mut codecs = vec![json!({"name": "opus", "type": "audio", "priority": 1000, "payload_type": OPUS_PT})];
    if video {
        codecs.push(json!({"name": "H264", "type": "video", "priority": 1000, "payload_type": H264_PT,
            "rtx_payload_type": H264_RTX_PT, "encode": true, "decode": true}));
    }
    json!({"op": 1, "d": {
        "protocol": "udp",
        "data": {"address": ip, "port": port, "mode": mode.wire_name()},
        "codecs": codecs,
        "rtc_connection_id": rtc_connection_id,
        "experiments": [],
    }})
    .to_string()
}

/// Op 5 with flag 1 (VOICE), 2 (SOUNDSHARE), or 0.
fn speaking_payload(on: bool, ssrc: u32, soundshare: bool) -> String {
    let flags = u8::from(on) << u8::from(soundshare);
    json!({"op": 5, "d": {"speaking": flags, "delay": 0, "ssrc": ssrc}}).to_string()
}

/// UDP receive task.
struct Receiver {
    sock: Arc<UdpSocket>,
    crypt: Arc<Mutex<Crypt>>,
    dave: Arc<Mutex<Dave>>,
    users: Arc<Mutex<HashMap<u32, Id>>>,
    playback: Playback,
    video: Option<VideoRx>,
    /// A watched Go Live carries the selected owner's audio, even if no speaking event
    /// arrived for that SSRC. Ordinary calls still require an announced sender.
    stream_owner: Option<Id>,
}

impl Receiver {
    async fn run(self) {
        let mut buf = vec![0u8; MTU];
        let mut held = Held::default();
        let mut trace = self.stream_owner.and_then(|_| Trace::new("stream audio", &["rtp", "transport_open", "dave_open", "mixer_queue"]));
        loop {
            match self.sock.recv(&mut buf).await {
                Ok(n) => self.on_packet(&mut buf[..n], &mut held, trace.as_mut()),
                // Windows reports an ICMP "port unreachable" as a recv error: keep going.
                Err(_) => sleep(Duration::from_millis(20)).await,
            }
        }
    }

    /// Demux (§2.6): discovery and ping replies and RTCP fail `parse_rtp`; Opus is played, the
    /// rest goes to the video hooks.
    fn on_packet(&self, pkt: &mut [u8], held: &mut Held, mut trace: Option<&mut Trace>) {
        if let Some(trace) = trace.as_deref_mut() { trace.flush(std::time::Instant::now()); }
        let rtp = parse_rtp(pkt);
        let Some(h) = rtp.filter(|h| h.payload_type == OPUS_PT || self.stream_owner.is_some_and(|owner| {
            (96..=127).contains(&h.payload_type) && lock(&self.users).get(&h.ssrc) == Some(&owner)
        })) else {
            if let Some(v) = &self.video {
                video::on_packet(v, &self.crypt, pkt, rtp.as_ref());
            }
            return;
        };
        if let Some(trace) = trace.as_deref_mut() { trace.record(0, Duration::ZERO); }
        let Some(plain) = lock(&self.crypt).open(pkt, h.clear_len) else { return };
        let Some(frame) = h.payload(plain) else { return };
        if let Some(trace) = trace.as_deref_mut() { trace.record(1, Duration::ZERO); }
        let now = std::time::Instant::now();
        let user = self.stream_owner.or_else(|| lock(&self.users).get(&h.ssrc).copied());
        match user {
            None => held.hold(now, h, frame),
            Some(user) => {
                for (_, early, early_frame) in held.take(now, h.ssrc) {
                    self.deliver(user, &early, &early_frame, None);
                }
                self.deliver(user, &h, frame, trace);
            }
        }
    }

    fn deliver(&self, user: Id, h: &RtpHeader, frame: &[u8], trace: Option<&mut Trace>) {
        let Some(opus) = lock(&self.dave).decrypt(user, frame) else { return };
        let queued = self.playback.push(user, h.ssrc, h.seq, opus);
        if let Some(trace) = trace {
            trace.record(2, Duration::ZERO);
            if queued { trace.record(3, Duration::ZERO); }
        }
    }
}

/// Frames of SSRCs no op 5 has named yet, oldest first (never guessed: dropped after
/// `UNKNOWN_WAIT`).
#[derive(Default)]
struct Held(VecDeque<(std::time::Instant, RtpHeader, Vec<u8>)>);

impl Held {
    fn hold(&mut self, now: std::time::Instant, h: RtpHeader, frame: &[u8]) {
        self.expire(now);
        if self.0.len() >= UNKNOWN_MAX {
            self.0.pop_front();
        }
        self.0.push_back((now, h, frame.to_vec()));
    }

    /// The held frames of `ssrc`, in arrival order.
    fn take(&mut self, now: std::time::Instant, ssrc: u32) -> VecDeque<(std::time::Instant, RtpHeader, Vec<u8>)> {
        self.expire(now);
        if !self.0.iter().any(|p| p.1.ssrc == ssrc) {
            return VecDeque::new();
        }
        let (mine, rest) = std::mem::take(&mut self.0).into_iter().partition(|p| p.1.ssrc == ssrc);
        self.0 = rest;
        mine
    }

    fn expire(&mut self, now: std::time::Instant) {
        while self.0.front().is_some_and(|p| now.saturating_duration_since(p.0) > UNKNOWN_WAIT) {
            self.0.pop_front();
        }
    }
}

/// Outgoing RTP sequence number and timestamp (random starts, §2.8).
struct RtpClock {
    seq: u16,
    ts: u32,
    last: Option<std::time::Instant>,
}

impl RtpClock {
    fn new() -> RtpClock {
        let r = getrandom::u64().unwrap_or(0);
        RtpClock { seq: r as u16, ts: (r >> 32) as u32, last: None }
    }

    /// `(seq, timestamp)` for the next frame. After a pause the timestamp jumps by the elapsed
    /// samples, so receivers keep their timing.
    fn next(&mut self, now: std::time::Instant, after_pause: bool) -> (u16, u32) {
        const FRAME: u32 = FRAME_SAMPLES as u32;
        if after_pause && let Some(last) = self.last {
            // Wraps like the timestamp itself.
            let gap = (now.saturating_duration_since(last).as_millis() as u64 * u64::from(SAMPLE_RATE / 1000)) as u32;
            self.ts = self.ts.wrapping_add(gap.saturating_sub(FRAME));
        }
        let out = (self.seq, self.ts);
        (self.seq, self.ts, self.last) = (self.seq.wrapping_add(1), self.ts.wrapping_add(FRAME), Some(now));
        out
    }
}

/// UDP send task.
struct Sender {
    sock: Arc<UdpSocket>,
    ssrc: u32,
    crypt: Arc<Mutex<Crypt>>,
    dave: Arc<Mutex<Dave>>,
    clock: RtpClock,
    /// Read by the WebSocket loop, which sends op 5.
    speaking: watch::Sender<bool>,
}

impl Sender {
    async fn run(mut self, mut captured: mpsc::Receiver<Captured>) {
        let mut pkt = Vec::with_capacity(MTU);
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        let mut ping_seq = 0u32;
        loop {
            tokio::select! {
                frame = captured.recv() => match frame {
                    Some(Captured::Frame(opus)) => {
                        // Speaking only once a packet goes out: DAVE drops frames until ready.
                        let after_pause = !*self.speaking.borrow();
                        if self.packet(&mut pkt, &opus, after_pause) {
                            self.speaking.send_replace(true);
                            let _ = self.sock.send(&pkt).await;
                        }
                    }
                    Some(Captured::End) => {
                        self.speaking.send_replace(false);
                    }
                    None => return, // the audio engine is gone
                },
                _ = keepalive.tick() => {
                    let mut ping = [0x13, 0x37, 0xCA, 0xFE, 0, 0, 0, 0];
                    ping[4..].copy_from_slice(&ping_seq.to_be_bytes());
                    ping_seq = ping_seq.wrapping_add(1);
                    let _ = self.sock.send(&ping).await;
                }
            }
        }
    }

    /// Builds the encrypted packet for one Opus frame into `pkt`; false = drop the frame (DAVE
    /// not ready: never sent unencrypted).
    fn packet(&mut self, pkt: &mut Vec<u8>, opus: &[u8], after_pause: bool) -> bool {
        let Some(frame) = lock(&self.dave).encrypt_opus(opus) else { return false };
        let (seq, ts) = self.clock.next(std::time::Instant::now(), after_pause);
        pkt.clear();
        write_rtp_header(pkt, seq, ts, self.ssrc);
        pkt.extend_from_slice(&frame);
        lock(&self.crypt).seal(pkt, RTP_HEADER).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn info() -> ConnectInfo {
        ConnectInfo {
            server_id: Id(1),
            channel_id: Id(2),
            user_id: Id(3),
            session_id: "sess".into(),
            token: Secret::new("v-secret".into()),
            endpoint: "host:443".into(),
            dave_group: Id(2),
            video: None,
        }
    }

    fn parse(s: String) -> Value {
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn payloads() {
        assert_eq!(
            parse(identify_payload(&info(), None)),
            json!({"op":0,"d":{"server_id":"1","channel_id":"2","user_id":"3","session_id":"sess","token":"v-secret",
                "video":false,"streams":[],"max_dave_protocol_version":1}})
        );
        let go_live = parse(identify_payload(&info(), Some(StreamKind::Screen)));
        assert_eq!((&go_live["d"]["video"], &go_live["d"]["streams"]), (&json!(true), &json!([{"type":"screen","rid":"100","quality":100}])));
        assert_eq!(
            parse(resume_payload(&info(), 7)),
            json!({"op":7,"d":{"server_id":"1","channel_id":"2","session_id":"sess","token":"v-secret","seq_ack":7}})
        );
        let beat = parse(heartbeat_payload(-1));
        assert_eq!((beat["op"].as_u64(), beat["d"]["seq_ack"].as_i64()), (Some(3), Some(-1)));
        assert!(beat["d"]["t"].as_u64().unwrap() > 1_700_000_000_000);
        let select = parse(select_protocol_payload("203.0.113.7", 5000, Mode::Aes256Gcm, "uuid", false));
        assert_eq!(select["d"]["data"], json!({"address":"203.0.113.7","port":5000,"mode":"aead_aes256_gcm_rtpsize"}));
        assert_eq!(select["d"]["codecs"], json!([{"name":"opus","type":"audio","priority":1000,"payload_type":120}]));
        assert_eq!(select["d"]["rtc_connection_id"], "uuid");
        let select = parse(select_protocol_payload("203.0.113.7", 5000, Mode::Aes256Gcm, "uuid", true));
        assert_eq!(
            select["d"]["codecs"][1],
            json!({"name":"H264","type":"video","priority":1000,"payload_type":103,"rtx_payload_type":104,"encode":true,"decode":true})
        );
        assert_eq!(parse(speaking_payload(true, 9, false)), json!({"op":5,"d":{"speaking":1,"delay":0,"ssrc":9}}));
        assert_eq!(parse(speaking_payload(true, 9, true)), json!({"op":5,"d":{"speaking":2,"delay":0,"ssrc":9}}));
        assert!(!format!("{:?}", info()).contains("v-secret"), "token redacted in Debug");
    }

    #[test]
    fn data_extracts_d() {
        #[derive(Deserialize)]
        struct Speaking {
            user_id: Id,
            ssrc: u32,
        }
        let s: Speaking = data(r#"{"op":5,"seq":4,"d":{"user_id":"77","ssrc":12,"speaking":1}}"#).unwrap();
        assert_eq!((s.user_id, s.ssrc), (Id(77), 12));
        assert!(data::<Speaking>(r#"{"op":5,"d":null}"#).is_none());
    }

    #[test]
    fn close_codes() {
        assert_eq!(close_exit(4014), Exit::End(End::Kicked));
        for c in [4004, 4006, 4009, 4011] {
            assert_eq!(close_exit(c), Exit::End(End::Rejoin), "{c}");
        }
        for c in [4001, 4002, 4003, 4005, 4012, 4016, 4017, 4020, 4021, 4022] {
            assert!(matches!(close_exit(c), Exit::End(End::Failed(_))), "{c}");
        }
        for c in [1000, 1001, 1005, 1006, 4013, 4015, 4999] {
            assert_eq!(close_exit(c), Exit::Lost, "{c}");
        }
        let secs: Vec<u64> = (1..=MAX_RESUMES + 2).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
    }

    fn header(ssrc: u32, seq: u16) -> RtpHeader {
        RtpHeader { payload_type: OPUS_PT, seq, timestamp: 0, ssrc, clear_len: RTP_HEADER, ext_words: 0, padding: false }
    }

    #[test]
    fn unknown_ssrcs_wait_briefly() {
        let t0 = std::time::Instant::now();
        let mut held = Held::default();
        held.hold(t0, header(1, 10), b"a");
        held.hold(t0, header(2, 50), b"x");
        held.hold(t0 + Duration::from_millis(20), header(1, 11), b"b");
        assert!(held.take(t0, 3).is_empty());
        let got: Vec<_> = held.take(t0 + Duration::from_millis(40), 1).into_iter().map(|(_, h, f)| (h.seq, f)).collect();
        assert_eq!(got, [(10, b"a".to_vec()), (11, b"b".to_vec())]);
        assert!(held.take(t0 + Duration::from_millis(40), 1).is_empty(), "taken once");
        assert!(held.take(t0 + UNKNOWN_WAIT + Duration::from_millis(1), 2).is_empty(), "expired");
        for seq in 0..UNKNOWN_MAX as u16 + 5 {
            held.hold(t0, header(4, seq), b"");
        }
        let kept = held.take(t0, 4);
        assert_eq!((kept.len(), kept[0].1.seq), (UNKNOWN_MAX, 5), "bounded, oldest dropped");
    }

    #[test]
    fn rtp_clock_steps_and_jumps_after_pauses() {
        let t0 = std::time::Instant::now();
        let mut c = RtpClock { seq: u16::MAX, ts: u32::MAX - 100, last: None };
        assert_eq!(c.next(t0, true), (u16::MAX, u32::MAX - 100));
        assert_eq!(c.next(t0 + Duration::from_millis(20), false), (0, 859));
        // 1 s of silence after the frame at 20 ms: 48 000 samples later.
        assert_eq!(c.next(t0 + Duration::from_millis(1_020), true), (1, 859 + 48_000));
        // Frames arriving in a burst still advance by one frame each.
        assert_eq!(c.next(t0 + Duration::from_millis(1_020), false), (2, 859 + 48_960));
    }

    /// Local UDP "voice server": IP discovery, then one sealed Opus packet and the speaking flag.
    #[tokio::test]
    async fn udp_discovery_and_send_path() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::spawn(open_udp(server.local_addr().unwrap(), 77));
        let mut buf = [0u8; MTU];
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(buf[..n], discovery_request(77));
        let mut resp = [0u8; 74];
        resp[..2].copy_from_slice(&[0, 2]);
        resp[8..17].copy_from_slice(b"127.0.0.1");
        resp[72..].copy_from_slice(&from.port().to_be_bytes());
        server.send_to(&resp, from).await.unwrap();
        let (sock, me) = client.await.unwrap().unwrap();
        assert_eq!(me, ("127.0.0.1".to_owned(), from.port()));

        let key = [5u8; 32];
        let (frames, captured) = mpsc::channel(4);
        let (speaking, mut speaking_rx) = watch::channel(false);
        let sender = Sender {
            sock: Arc::new(sock),
            ssrc: 77,
            crypt: Arc::new(Mutex::new(Crypt::new(Mode::XChaCha20, &key))),
            dave: Arc::new(Mutex::new(Dave::new(Id(1), Id(2)))), // DAVE off: passthrough
            clock: RtpClock::new(),
            speaking,
        };
        let task = tokio::spawn(sender.run(captured));
        frames.send(Captured::Frame(vec![1, 2, 3])).await.unwrap();
        let rx = Crypt::new(Mode::XChaCha20, &key);
        loop {
            let n = server.recv(&mut buf).await.unwrap();
            if buf[..4] == [0x13, 0x37, 0xCA, 0xFE] {
                continue; // keepalive ping
            }
            let h = parse_rtp(&buf[..n]).unwrap();
            assert_eq!((h.ssrc, h.payload_type, h.clear_len), (77, OPUS_PT, RTP_HEADER));
            let plain = rx.open(&mut buf[..n], h.clear_len).unwrap();
            assert_eq!(h.payload(plain), Some(&[1u8, 2, 3][..]));
            break;
        }
        assert!(*speaking_rx.borrow_and_update());
        frames.send(Captured::End).await.unwrap();
        speaking_rx.changed().await.unwrap();
        assert!(!*speaking_rx.borrow());
        drop(frames);
        task.await.unwrap(); // ends with the audio engine
    }

    #[tokio::test]
    async fn no_speaking_for_frames_dave_drops() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(sock.local_addr().unwrap()).await.unwrap();
        let mut dave = Dave::new(Id(1), Id(2));
        dave.on_session_description(1); // DAVE on, no group key yet
        let (speaking, speaking_rx) = watch::channel(false);
        let crypt = Arc::new(Mutex::new(Crypt::new(Mode::XChaCha20, &[5; 32])));
        let sender = Sender { sock: Arc::new(sock), ssrc: 77, crypt, dave: Arc::new(Mutex::new(dave)), clock: RtpClock::new(), speaking };
        let (frames, captured) = mpsc::channel(4);
        let task = tokio::spawn(sender.run(captured));
        frames.send(Captured::Frame(vec![1, 2, 3])).await.unwrap();
        drop(frames);
        task.await.unwrap();
        assert!(!*speaking_rx.borrow(), "no op 5 speaking:1 while nothing is sent");
    }

    /// Offline self-test of the whole media path with real Opus, AEAD and UDP: capture frame ->
    /// DAVE (off) -> RTP -> seal -> fake SFU relay -> demux -> open -> SSRC map -> DAVE ->
    /// jitter buffer -> Opus decode -> mixer.
    #[tokio::test]
    async fn media_loopback_plays_a_tone() {
        media_loopback(None, OPUS_PT).await;
    }

    #[tokio::test]
    async fn stream_loopback_plays_audio_without_a_speaking_event() {
        media_loopback(Some(Id(5)), OPUS_PT).await;
    }

    #[tokio::test]
    async fn stream_loopback_accepts_an_announced_audio_ssrc_with_remapped_payload() {
        media_loopback(Some(Id(5)), 111).await;
    }

    async fn media_loopback(stream_owner: Option<Id>, payload_type: u8) {
        use crate::voice::audio::{CAPTURE_QUEUE, StreamGain, test_hooks};
        let sfu = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut socks = Vec::new();
        for _ in 0..2 {
            let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            s.connect(sfu.local_addr().unwrap()).await.unwrap();
            socks.push(Arc::new(s));
        }
        let (rx_sock, tx_sock) = (socks.pop().unwrap(), socks.pop().unwrap());
        let crypt = || Arc::new(Mutex::new(Crypt::new(Mode::Aes256Gcm, &[9; 32])));
        let dave = Arc::new(Mutex::new(Dave::new(Id(1), Id(2))));
        let (playback, mut speaker) = test_hooks::speaker();
        let (playback, users) = match stream_owner {
            Some(owner) => {
                let users = if payload_type == OPUS_PT { HashMap::new() } else { HashMap::from([(77, owner)]) };
                (playback.for_stream(1, StreamGain::default()), users)
            }
            None => (playback, HashMap::from([(77, Id(5))])),
        };
        let users = Arc::new(Mutex::new(users));
        let receiver = Receiver { sock: rx_sock.clone(), crypt: crypt(), dave: dave.clone(), users, playback, video: None, stream_owner };
        let sender = Sender { sock: tx_sock, ssrc: 77, crypt: crypt(), dave, clock: RtpClock::new(), speaking: watch::channel(false).0 };
        let (frames, captured) = mpsc::channel(CAPTURE_QUEUE);
        let mut tasks = JoinSet::new();
        tasks.spawn(receiver.run());
        tasks.spawn(sender.run(captured));

        let mut encode = test_hooks::encoder();
        let tone: [f32; FRAME_SAMPLES] = std::array::from_fn(|i| 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin());
        const FRAMES: usize = 6;
        for _ in 0..FRAMES {
            frames.send(Captured::Frame(encode(&tone))).await.unwrap();
        }
        let (mut buf, mut relayed) = ([0u8; MTU], 0);
        let inbound = crypt();
        let rewritten = crypt();
        while relayed < FRAMES {
            let n = sfu.recv(&mut buf).await.unwrap();
            if buf[..4] != [0x13, 0x37, 0xCA, 0xFE] {
                let target = rx_sock.local_addr().unwrap();
                if payload_type == OPUS_PT {
                    sfu.send_to(&buf[..n], target).await.unwrap();
                } else {
                    let mut packet = buf[..RTP_HEADER].to_vec();
                    packet[1] = payload_type;
                    packet.extend_from_slice(lock(&inbound).open(&mut buf[..n], RTP_HEADER).unwrap());
                    lock(&rewritten).seal(&mut packet, RTP_HEADER).unwrap();
                    sfu.send_to(&packet, target).await.unwrap();
                }
                relayed += 1;
            }
        }
        let mut peak = 0f32;
        for _ in 0..200 {
            if let Some(mix) = speaker() {
                peak = mix.iter().fold(peak, |p, s| p.max(s.abs()));
            }
            if peak > 0.3 {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!((0.3..=0.8).contains(&peak), "the 0.5 tone comes out of the mixer, peak {peak}");
    }

    /// The WebSocket loop against a local fake voice server: Identify, Hello, Ready + IP
    /// discovery, Select Protocol, Session Description with DAVE (op 26 key package), op 5,
    /// `connected`; then a network loss is resumed (op 7 with `seq_ack`) and 4014 ends it.
    #[tokio::test]
    async fn websocket_handshake_dave_resume_and_kick() {
        use tokio_tungstenite::accept_async;
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_port = udp.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut b = [0u8; MTU];
            while let Ok((n, from)) = udp.recv_from(&mut b).await {
                if n == 74 {
                    let mut r = [0u8; 74];
                    r[..2].copy_from_slice(&[0, 2]);
                    r[8..17].copy_from_slice(b"127.0.0.1");
                    r[72..].copy_from_slice(&from.port().to_be_bytes());
                    let _ = udp.send_to(&r, from).await;
                }
            }
        });
        let events = crate::voice::VoiceEvents(crate::backend::EventSink::new(std::sync::mpsc::channel().0, egui::Context::default()));
        let (_ctl, ctl) = mpsc::unbounded_channel();
        let video = VideoLink { kind: StreamKind::Camera, rx: VideoRx::new(Arc::default(), events, None), ctl };
        let info = ConnectInfo { endpoint: tcp.local_addr().unwrap().to_string(), video: Some(video), ..info() };
        let (_frames, captured) = mpsc::channel(1);
        let media = MediaLink { captured, playback: crate::voice::audio::test_hooks::speaker().0 };
        let (_stop, stop) = oneshot::channel();
        let (connected_tx, connected_rx) = oneshot::channel();
        let client = tokio::spawn(run(info, media, stop, move || _ = connected_tx.send(())));

        async fn text(ws: &mut WebSocketStream<TcpStream>) -> Value {
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
                    Message::Binary(b) => assert_eq!(b[0], 26, "only the key package is binary"),
                    _ => {}
                }
            }
        }
        let say = |v: Value| Message::text(v.to_string());

        let mut ws = accept_async(tcp.accept().await.unwrap().0).await.unwrap();
        let identify = text(&mut ws).await;
        assert_eq!((identify["op"].as_u64(), identify["d"]["token"].as_str()), (Some(0), Some("v-secret")));
        assert_eq!(identify["d"]["streams"][0]["type"], "video", "the video link reaches Identify");
        ws.send(say(json!({"op": 8, "d": {"heartbeat_interval": 45000}}))).await.unwrap();
        let modes = ["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize"];
        ws.send(say(json!({"op": 2, "d": {"ssrc": 77, "ip": "127.0.0.1", "port": udp_port, "modes": modes}}))).await.unwrap();
        let select = text(&mut ws).await;
        assert_eq!(select["op"], 1);
        assert_eq!(select["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
        assert_eq!(select["d"]["data"]["address"], "127.0.0.1");
        assert_eq!(select["d"]["codecs"][1]["name"], "H264");
        let desc = json!({"op": 4, "seq": 3, "d": {"mode": "aead_aes256_gcm_rtpsize", "secret_key": vec![7u8; 32], "dave_protocol_version": 1}});
        ws.send(say(desc)).await.unwrap();
        let Message::Binary(key_package) = ws.next().await.unwrap().unwrap() else { panic!("op 26 first") };
        assert_eq!(key_package[0], 26);
        assert_eq!(text(&mut ws).await, json!({"op": 5, "d": {"speaking": 0, "delay": 0, "ssrc": 77}}));
        connected_rx.await.unwrap();

        drop(ws); // network loss: no close frame
        let mut ws = accept_async(tcp.accept().await.unwrap().0).await.unwrap();
        let resume = text(&mut ws).await;
        assert_eq!((resume["op"].as_u64(), resume["d"]["seq_ack"].as_i64()), (Some(7), Some(3)));
        assert_eq!(resume["d"]["token"], "v-secret");
        ws.send(say(json!({"op": 8, "d": {"heartbeat_interval": 45000}}))).await.unwrap();
        ws.send(say(json!({"op": 9, "d": null}))).await.unwrap();
        ws.close(Some(CloseFrame { code: 4014.into(), reason: Default::default() })).await.unwrap();
        assert_eq!(client.await.unwrap(), End::Kicked);
    }
}
