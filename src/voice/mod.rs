//! Voice channels and DM calls. Protocol: docs/VOICE_NOTES.md. Data flow, tasks, threads,
//! buffer sizes and file ownership: docs/VOICE_DESIGN.md (audio), docs/VIDEO_DESIGN.md (camera,
//! screen share, Go Live; `video/`).
//!
//! OWNERSHIP: this file (contract owner): `VoiceEvent`, `InputMode`, `VoiceManager` and the
//! call orchestration (op 4, waiting for the two VOICE_* dispatches, reconnect policy).
//! `transport.rs`, `audio.rs`, `dave.rs`: one engine engineer each; keep their pub signatures.
//! The video side of the manager lives in `video::stream::Video`.
//!
//! Everything starts with a user command: nothing here joins, rejoins or reconnects on its own,
//! except one fresh op 4 after the voice server invalidated a running session.

pub mod audio;
pub mod dave;
pub mod transport;
pub mod video;

use crate::auth::Secret;
use crate::backend::{BackendEvent, EventSink};
use crate::gateway::GatewayCommand;
use crate::model::{GatewayEvent, Id, VoiceState};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

/// No answer to op 4 (a full channel sends nothing at all) or no new voice server after a
/// disconnect: give up and leave.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Fresh op 4s per user join after the voice server invalidated the session.
const MAX_REJOINS: u8 = 1;

/// Voice -> UI, inside `BackendEvent::Voice`.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceEvent {
    /// Op 4 sent (or the connection dropped and a new voice server is awaited).
    Connecting { channel_id: Id },
    /// Media connected (also after a move to another channel).
    Connected { channel_id: Id },
    /// Out of voice. `reason` None = the user left; Some = user-facing text for a toast.
    Disconnected { reason: Option<String> },
    /// Speaking indicator of a user (me included) turned on or off.
    Speaking { user_id: Id, speaking: bool },
    /// My microphone level 0.0..=1.0 (0 while muted); at most ~5 per second and only on a 5 %
    /// change, only while in a call.
    LocalLevel(f32),
    /// Something failed but the call goes on (e.g. no microphone). User-facing text.
    Error(String),
    /// Answer to `Command::ListVoiceDevices`: cpal device names.
    Devices { inputs: Vec<String>, outputs: Vec<String> },
    /// Camera, screen share, Go Live (answers to `Command::Video`, remote videos).
    Video(video::VideoEvent),
}

/// When the microphone transmits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// While the input level is above `threshold_db` (dBFS, -100.0..=0.0), ~300 ms hangover.
    VoiceActivity { threshold_db: f32 },
    /// While this key is held: Windows virtual-key code, polled globally every 10 ms.
    PushToTalk { vk: u16 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppression {
    #[default]
    Off,
    RnNoise,
}

impl NoiseSuppression {
    pub const OPTIONS: [Self; 2] = [Self::Off, Self::RnNoise];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::RnNoise => "RNNoise",
        }
    }
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::VoiceActivity { threshold_db: -50.0 }
    }
}

/// Sends voice events to the UI (wakes it). Cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct VoiceEvents(EventSink);

impl VoiceEvents {
    pub fn send(&self, ev: VoiceEvent) {
        self.0.send(BackendEvent::Voice(ev));
    }
}

/// Main gateway -> voice task: my VOICE_STATE_UPDATEs and every VOICE_SERVER_UPDATE.
#[derive(Clone)]
pub struct VoiceFeed(mpsc::UnboundedSender<Msg>);

impl VoiceFeed {
    pub fn send(&self, ev: GatewayEvent) {
        let _ = self.0.send(Msg::Gateway(ev));
    }

    /// READY: a new main-gateway session (not sent on RESUMED). A call of an older session
    /// is gone; voice states of other sessions are my other devices.
    pub fn session(&self, me: Id, session_id: String) {
        let _ = self.0.send(Msg::Session { me, session_id });
    }
}

/// Handle to the voice task (one per backend loop, outliving logins). Cheap to clone; every
/// method only queues a message and returns at once.
#[derive(Clone)]
pub struct VoiceManager {
    tx: mpsc::UnboundedSender<Msg>,
}

#[allow(clippy::large_enum_variant, reason = "a few per voice action; boxing GatewayEvent would allocate for each")]
enum Msg {
    Attach(Option<mpsc::UnboundedSender<GatewayCommand>>),
    Join { guild_id: Option<Id>, channel_id: Id },
    Leave,
    SelfMute(bool),
    SelfDeaf(bool),
    Devices { input: Option<String>, output: Option<String> },
    Mode(InputMode),
    NoiseSuppression(NoiseSuppression),
    Volume { mic: u8, playback: u8 },
    ListDevices,
    Video(video::VideoCommand),
    /// From tasks and threads of `video::stream::Video`.
    Stream(video::stream::StreamMsg),
    Gateway(GatewayEvent),
    Session { me: Id, session_id: String },
    /// From the transport task of connection number `.0`.
    Connected(u64),
    Ended(u64, transport::End),
}

impl VoiceManager {
    /// Starts the voice task. Must be called inside the tokio runtime. Decoded video frames go
    /// to `frames` (shared with the UI).
    pub fn spawn(sink: EventSink, frames: Arc<video::VideoFrames>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Manager::new(sink, tx.downgrade(), frames).run(rx));
        Self { tx }
    }

    /// For `gateway::spawn`: where my voice dispatches go.
    pub fn feed(&self) -> VoiceFeed {
        VoiceFeed(self.tx.clone())
    }

    /// The logged-in session's gateway (op 4 goes there). None on logout: hangs up without op 4
    /// (the session is closing anyway).
    pub fn attach_gateway(&self, gateway: Option<mpsc::UnboundedSender<GatewayCommand>>) {
        self.post(Msg::Attach(gateway));
    }

    /// Join (or switch to) a voice channel; `guild_id` None = DM/group call.
    pub fn join(&self, guild_id: Option<Id>, channel_id: Id) {
        self.post(Msg::Join { guild_id, channel_id });
    }

    pub fn leave(&self) {
        self.post(Msg::Leave);
    }

    pub fn set_self_mute(&self, on: bool) {
        self.post(Msg::SelfMute(on));
    }

    /// Deafen also mutes (op 4 sends both).
    pub fn set_self_deaf(&self, on: bool) {
        self.post(Msg::SelfDeaf(on));
    }

    /// cpal device names; None = system default. Kept for later calls.
    pub fn set_devices(&self, input: Option<String>, output: Option<String>) {
        self.post(Msg::Devices { input, output });
    }

    pub fn set_input_mode(&self, mode: InputMode) {
        self.post(Msg::Mode(mode));
    }

    pub fn set_noise_suppression(&self, mode: NoiseSuppression) {
        self.post(Msg::NoiseSuppression(mode));
    }

    pub fn set_volume(&self, mic: u8, playback: u8) {
        self.post(Msg::Volume { mic, playback });
    }

    /// Answered with `VoiceEvent::Devices`.
    pub fn list_devices(&self) {
        self.post(Msg::ListDevices);
    }

    /// Camera, screen share, Go Live; answered with `VoiceEvent::Video`.
    pub fn video(&self, cmd: video::VideoCommand) {
        self.post(Msg::Video(cmd));
    }

    fn post(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}

/// The channel I asked for and what the main gateway has said about it so far.
struct Target {
    guild_id: Option<Id>,
    channel_id: Id,
    /// From my VOICE_STATE_UPDATE.
    session_id: Option<String>,
    /// VOICE_SERVER_UPDATE `(endpoint, token)`, consumed by the next connect.
    server: Option<(String, Secret)>,
    rejoins: u8,
    /// Give up when nothing connects by then; None while connected.
    deadline: Option<Instant>,
}

impl Target {
    fn new(guild_id: Option<Id>, channel_id: Id) -> Self {
        Self { guild_id, channel_id, session_id: None, server: None, rejoins: 0, deadline: Some(Instant::now() + JOIN_TIMEOUT) }
    }

    /// Whether a VOICE_SERVER_UPDATE is for this target (DM calls carry the channel id).
    fn owns_server(&self, guild_id: Option<Id>, channel_id: Option<Id>) -> bool {
        match self.guild_id {
            Some(_) => guild_id == self.guild_id,
            None => guild_id.is_none() && channel_id == Some(self.channel_id),
        }
    }

    /// Connection parameters once both dispatches arrived; takes the token (one connect each).
    fn connect_info(&mut self, me: Id) -> Option<transport::ConnectInfo> {
        let session_id = self.session_id.clone()?;
        let (endpoint, token) = self.server.take()?;
        let server_id = self.guild_id.unwrap_or(self.channel_id);
        let channel_id = self.channel_id;
        Some(transport::ConnectInfo { server_id, channel_id, user_id: me, session_id, token, endpoint, dave_group: channel_id, video: None })
    }
}

/// A running connection. Dropping it stops the transport (its `stop` resolves) and the audio.
struct Call {
    _stop: oneshot::Sender<()>,
    audio: audio::Audio,
    /// For Go Live connections of this call (their audio plays in the call's mix).
    playback: audio::Playback,
}

struct Manager {
    events: VoiceEvents,
    /// Weak: the task ends once every `VoiceManager`/`VoiceFeed` is gone.
    tx: mpsc::WeakUnboundedSender<Msg>,
    gateway: Option<mpsc::UnboundedSender<GatewayCommand>>,
    me: Id,
    /// Main-gateway session (READY); None until the first READY of a login.
    session_id: Option<String>,
    self_mute: bool,
    self_deaf: bool,
    /// Server mute or stage suppress / server deafen, from my voice state.
    server_mute: bool,
    server_deaf: bool,
    /// Devices and input mode (`muted`/`deafened` are filled in by `audio_config`).
    prefs: audio::AudioConfig,
    target: Option<Target>,
    call: Option<Call>,
    /// Connection counter: events of replaced connections are ignored.
    conn_no: u64,
    /// Camera, screen share, Go Live.
    video: video::stream::Video,
}

impl Manager {
    fn new(sink: EventSink, tx: mpsc::WeakUnboundedSender<Msg>, frames: Arc<video::VideoFrames>) -> Self {
        let events = VoiceEvents(sink);
        Manager {
            video: video::stream::Video::new(events.clone(), frames, tx.clone()),
            events,
            tx,
            gateway: None,
            me: Id(0),
            session_id: None,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            prefs: audio::AudioConfig::default(),
            target: None,
            call: None,
            conn_no: 0,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        loop {
            let deadline = self.target.as_ref().and_then(|t| t.deadline);
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.handle(msg),
                    None => return,
                },
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.hang_up(Some("Could not connect to voice. The channel may be full.".into()), true);
                }
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Attach(gateway) => {
                if gateway.is_none() {
                    self.hang_up(None, false);
                }
                (self.gateway, self.session_id) = (gateway, None);
            }
            Msg::Join { guild_id, channel_id } => {
                if self.gateway.is_none() {
                    self.events.send(VoiceEvent::Error("Not connected.".into()));
                    return;
                }
                // Already there: a second op 4 changes nothing, so no new voice server would come.
                if self.target.as_ref().is_some_and(|t| (t.guild_id, t.channel_id) == (guild_id, channel_id)) {
                    return;
                }
                self.drop_call();
                self.target = Some(Target::new(guild_id, channel_id));
                self.op4(guild_id, Some(channel_id));
                self.events.send(VoiceEvent::Connecting { channel_id });
            }
            Msg::Leave => self.hang_up(None, true),
            Msg::SelfMute(on) => {
                self.self_mute = on;
                self.resend_op4();
            }
            Msg::SelfDeaf(on) => {
                self.self_deaf = on;
                self.resend_op4();
            }
            Msg::Devices { input, output } => {
                (self.prefs.input, self.prefs.output) = (input, output);
                self.apply_audio();
            }
            Msg::Mode(mode) => {
                self.prefs.mode = mode;
                self.apply_audio();
            }
            Msg::NoiseSuppression(mode) => {
                self.prefs.noise_suppression = mode;
                self.apply_audio();
            }
            Msg::Volume { mic, playback } => {
                self.prefs.gain = audio::Gain { mic: mic.min(100), playback: playback.min(100) };
                self.apply_audio();
            }
            Msg::ListDevices => {
                let events = self.events.clone();
                // cpal enumeration blocks (WASAPI COM calls).
                tokio::task::spawn_blocking(move || {
                    let (inputs, outputs) = audio::list_devices();
                    events.send(VoiceEvent::Devices { inputs, outputs });
                });
            }
            Msg::Video(cmd) => self.with_video(|v, ctx| v.command(cmd, ctx)),
            Msg::Stream(msg) => self.with_video(|v, ctx| v.on_msg(msg, ctx)),
            Msg::Gateway(ev @ (GatewayEvent::StreamCreate { .. } | GatewayEvent::StreamUpdate { .. } | GatewayEvent::StreamServerUpdate { .. } | GatewayEvent::StreamDelete { .. })) => {
                self.with_video(|v, ctx| v.on_gateway(ev, ctx));
            }
            Msg::Gateway(GatewayEvent::VoiceStateUpdate(v)) => self.on_my_state(*v),
            Msg::Gateway(GatewayEvent::VoiceServerUpdate { guild_id, channel_id, endpoint, token }) => {
                let Some(t) = self.target.as_mut().filter(|t| t.owns_server(guild_id, channel_id)) else { return };
                match endpoint {
                    Some(endpoint) => {
                        t.server = Some((endpoint, token));
                        self.try_connect();
                    }
                    // Reallocating: drop the connection and wait for the next server.
                    None => {
                        t.deadline = Some(Instant::now() + JOIN_TIMEOUT);
                        self.drop_call();
                    }
                }
            }
            Msg::Gateway(_) => {}
            Msg::Session { me, session_id } => {
                self.me = me;
                // Re-identified: the voice state belonged to the dead session. No op 4 (no
                // automatic rejoin); the user joins again.
                if self.session_id.replace(session_id).is_some() {
                    self.hang_up(Some("Voice was disconnected because the connection to the server was reset.".into()), false);
                }
            }
            Msg::Connected(conn_no) if conn_no == self.conn_no => {
                // `rejoins` is not reset: one automatic op 4 per user join (a Rejoin close right
                // after every connect would otherwise loop without backoff).
                if let Some(t) = &mut self.target {
                    t.deadline = None;
                    self.events.send(VoiceEvent::Connected { channel_id: t.channel_id });
                }
            }
            Msg::Ended(conn_no, end) if conn_no == self.conn_no => self.on_end(end),
            Msg::Connected(_) | Msg::Ended(..) => {} // a replaced connection
        }
    }

    fn on_my_state(&mut self, v: VoiceState) {
        self.me = v.user_id;
        if self.session_id.as_ref().is_some_and(|s| *s != v.session_id) {
            // Another device of mine joined voice: mine ended. No op 4, it would leave there too.
            if v.channel_id.is_some() {
                self.hang_up(Some("Voice continued on another device.".into()), false);
            }
            return;
        }
        let Some(t) = self.target.as_mut().filter(|t| t.guild_id == v.guild_id) else { return };
        match v.channel_id {
            Some(channel_id) => {
                t.channel_id = channel_id; // follows a move by a moderator
                t.session_id = Some(v.session_id);
                (self.server_mute, self.server_deaf) = (v.mute || v.suppress, v.deaf);
                self.apply_audio();
                self.try_connect();
            }
            // Kicked or disconnected elsewhere. Before our state arrived it is the echo of an
            // earlier leave, not about this join.
            None if t.session_id.is_some() => self.hang_up(Some("You were disconnected from voice.".into()), false),
            None => {}
        }
    }

    fn on_end(&mut self, end: transport::End) {
        self.end_call();
        let Some(t) = &mut self.target else { return };
        match end {
            transport::End::Stopped => {}
            // The gateway's next VOICE_* dispatches decide (move, or kick); else the deadline.
            transport::End::Kicked => {
                t.deadline = Some(Instant::now() + JOIN_TIMEOUT);
                self.events.send(VoiceEvent::Connecting { channel_id: t.channel_id });
            }
            transport::End::Rejoin if t.rejoins < MAX_REJOINS => {
                t.rejoins += 1;
                (t.server, t.deadline) = (None, Some(Instant::now() + JOIN_TIMEOUT));
                let (guild_id, channel_id) = (t.guild_id, t.channel_id);
                self.op4(guild_id, Some(channel_id));
                self.events.send(VoiceEvent::Connecting { channel_id });
            }
            transport::End::Rejoin => self.hang_up(Some("Lost the voice connection.".into()), true),
            transport::End::Failed(reason) => self.hang_up(Some(reason), true),
        }
    }

    fn try_connect(&mut self) {
        let me = self.me;
        let Some(mut info) = self.target.as_mut().and_then(|t| t.connect_info(me)) else { return };
        self.drop_call(); // a new token replaces the running connection (no resume)
        info.video = Some(self.video.link_for_call());
        let (audio, media) = audio::Audio::start(self.audio_config(), me, self.events.clone());
        let playback = media.playback.clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (tx, conn_no) = (self.tx.clone(), self.conn_no);
        tokio::spawn(async move {
            let notify = tx.clone();
            let connected = move || {
                if let Some(tx) = notify.upgrade() {
                    let _ = tx.send(Msg::Connected(conn_no));
                }
            };
            // A panic (e.g. in an MLS parser) must still end the call, not leave it hanging.
            let end = AssertUnwindSafe(transport::run(info, media, stop_rx, connected))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| transport::End::Failed("Voice stopped after an internal error.".into()));
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Msg::Ended(conn_no, end));
            }
        });
        self.call = Some(Call { _stop: stop_tx, audio, playback });
    }

    /// Stops the running connection; its late Connected/Ended no longer match.
    fn drop_call(&mut self) {
        self.end_call();
        self.conn_no += 1;
    }

    /// The connection is gone (stopped, or ended by itself): so is its video.
    fn end_call(&mut self) {
        if self.call.take().is_some() {
            self.with_video(|v, ctx| v.call_ended(ctx));
        }
    }

    /// Runs `f` on the video state; a camera switched on or off resends op 4 (`self_video`).
    fn with_video(&mut self, f: impl FnOnce(&mut video::stream::Video, video::stream::Ctx)) {
        let connected = self.call.as_ref().filter(|_| self.target.as_ref().is_some_and(|t| t.deadline.is_none()));
        let ctx = video::stream::Ctx {
            gateway: self.gateway.clone(),
            me: self.me,
            session_id: self.session_id.clone(),
            call: self.target.as_ref().filter(|_| connected.is_some()).map(|t| (t.guild_id, t.channel_id)),
            playback: connected.map(|c| c.playback.clone()),
        };
        let camera = self.video.camera_on();
        f(&mut self.video, ctx);
        if self.video.camera_on() != camera {
            self.resend_op4();
        }
    }

    /// Leaves voice: stops the call, optionally sends op 4 with no channel, tells the UI.
    fn hang_up(&mut self, reason: Option<String>, send_leave: bool) {
        let Some(t) = self.target.take() else { return };
        self.drop_call();
        (self.server_mute, self.server_deaf) = (false, false);
        if send_leave {
            self.op4(t.guild_id, None);
        }
        self.events.send(VoiceEvent::Disconnected { reason });
    }

    /// Mute/deafen changed: tell the server (when in voice) and the audio engine.
    fn resend_op4(&mut self) {
        if let Some(t) = &self.target {
            self.op4(t.guild_id, Some(t.channel_id));
        }
        self.apply_audio();
    }

    fn op4(&self, guild_id: Option<Id>, channel_id: Option<Id>) {
        if let Some(gw) = &self.gateway {
            let (self_mute, self_deaf) = (self.self_mute || self.self_deaf, self.self_deaf);
            let self_video = channel_id.is_some() && self.video.camera_on();
            let _ = gw.send(GatewayCommand::UpdateVoiceState { guild_id, channel_id, self_mute, self_deaf, self_video });
        }
    }

    fn audio_config(&self) -> audio::AudioConfig {
        audio::AudioConfig {
            muted: self.self_mute || self.self_deaf || self.server_mute,
            deafened: self.self_deaf || self.server_deaf,
            ..self.prefs.clone()
        }
    }

    fn apply_audio(&self) {
        if let Some(call) = &self.call {
            call.audio.set_config(self.audio_config());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_connects_once_both_dispatches_arrived() {
        let mut t = Target::new(Some(Id(1)), Id(2));
        t.server = Some(("host:443".into(), Secret::new("tok".into())));
        assert!(t.connect_info(Id(9)).is_none());
        assert!(t.server.is_some(), "token kept until the session id arrives");
        t.session_id = Some("sess".into());
        let info = t.connect_info(Id(9)).unwrap();
        assert_eq!((info.server_id, info.channel_id, info.user_id), (Id(1), Id(2), Id(9)));
        assert_eq!((info.session_id.as_str(), info.endpoint.as_str(), info.token.expose()), ("sess", "host:443", "tok"));
        assert!(t.connect_info(Id(9)).is_none(), "one connect per token");

        let mut dm = Target::new(None, Id(5));
        (dm.session_id, dm.server) = (Some("s".into()), Some(("h:1".into(), Secret::new("t".into()))));
        assert_eq!(dm.connect_info(Id(9)).unwrap().server_id, Id(5), "calls use the channel id");
    }

    #[test]
    fn server_updates_match_the_target() {
        let guild = Target::new(Some(Id(1)), Id(2));
        assert!(guild.owns_server(Some(Id(1)), None));
        assert!(!guild.owns_server(Some(Id(3)), None));
        assert!(!guild.owns_server(None, Some(Id(2))));
        let dm = Target::new(None, Id(5));
        assert!(dm.owns_server(None, Some(Id(5))));
        assert!(!dm.owns_server(None, Some(Id(6))));
        assert!(!dm.owns_server(Some(Id(1)), Some(Id(5))));
    }

    /// Lets the voice task drain its queue (current-thread runtime: it runs until it waits).
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    fn voice_events(rx: &std::sync::mpsc::Receiver<BackendEvent>) -> Vec<VoiceEvent> {
        rx.try_iter().filter_map(|e| if let BackendEvent::Voice(v) = e { Some(v) } else { None }).collect()
    }

    fn my_state(channel_id: Option<Id>) -> GatewayEvent {
        state_of("s", channel_id)
    }

    fn state_of(session_id: &str, channel_id: Option<Id>) -> GatewayEvent {
        let v = VoiceState { guild_id: Some(Id(1)), channel_id, user_id: Id(9), session_id: session_id.into(), ..Default::default() };
        GatewayEvent::VoiceStateUpdate(Box::new(v))
    }

    #[tokio::test]
    async fn other_devices_and_new_sessions_end_the_call_without_op4() {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let voice = VoiceManager::spawn(EventSink::new(ev_tx, egui::Context::default()), Arc::default());
        let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();
        let feed = voice.feed();
        voice.attach_gateway(Some(gw_tx));
        feed.session(Id(9), "s".into());
        voice.join(Some(Id(1)), Id(2));
        voice.join(Some(Id(1)), Id(2));
        settle().await;
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { channel_id: Some(Id(2)), .. })));
        assert!(gw_rx.try_recv().is_err(), "joining the same channel again sends nothing");
        assert_eq!(voice_events(&ev_rx), [VoiceEvent::Connecting { channel_id: Id(2) }]);

        // My phone leaving voice is not about this call; my phone joining voice ends it.
        feed.send(state_of("phone", None));
        feed.send(my_state(Some(Id(2))));
        settle().await;
        assert!(voice_events(&ev_rx).is_empty());
        feed.send(state_of("phone", Some(Id(3))));
        settle().await;
        assert_eq!(voice_events(&ev_rx), [VoiceEvent::Disconnected { reason: Some("Voice continued on another device.".into()) }]);
        assert!(gw_rx.try_recv().is_err(), "no op 4: it would leave on the phone too");

        // A re-identify (new session) ends the call; the old session's state is stale.
        voice.join(Some(Id(1)), Id(2));
        feed.session(Id(9), "s2".into());
        feed.send(my_state(Some(Id(2))));
        settle().await;
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { channel_id: Some(Id(2)), .. })));
        assert!(gw_rx.try_recv().is_err(), "no automatic rejoin");
        let evs = voice_events(&ev_rx);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[1], VoiceEvent::Disconnected { reason: Some(r) } if r.contains("reset")));
    }

    /// A manager driven by hand (no task): `(manager, UI events, op 4s)`.
    fn manager() -> (Manager, std::sync::mpsc::Receiver<BackendEvent>, mpsc::UnboundedReceiver<GatewayCommand>) {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let mut m = Manager::new(EventSink::new(ev_tx, egui::Context::default()), mpsc::unbounded_channel().0.downgrade(), Arc::default());
        let (gw_tx, gw_rx) = mpsc::unbounded_channel();
        m.gateway = Some(gw_tx);
        (m, ev_rx, gw_rx)
    }

    #[test]
    fn late_messages_of_a_replaced_connection_are_ignored() {
        let (mut m, ev_rx, _gw) = manager();
        m.handle(Msg::Join { guild_id: Some(Id(1)), channel_id: Id(2) });
        let old = m.conn_no; // what channel 2's connection reports with
        m.handle(Msg::Join { guild_id: Some(Id(1)), channel_id: Id(3) });
        voice_events(&ev_rx);
        m.handle(Msg::Connected(old));
        m.handle(Msg::Ended(old, transport::End::Failed("call ended".into())));
        assert!(voice_events(&ev_rx).is_empty());
        assert!(m.target.as_ref().is_some_and(|t| t.channel_id == Id(3) && t.deadline.is_some()), "the join deadline still runs");
    }

    #[test]
    fn one_automatic_rejoin_per_join() {
        let (mut m, ev_rx, mut gw_rx) = manager();
        m.handle(Msg::Join { guild_id: Some(Id(1)), channel_id: Id(2) });
        let n = m.conn_no;
        for _ in 0..2 {
            m.handle(Msg::Connected(n));
            m.handle(Msg::Ended(n, transport::End::Rejoin));
        }
        let mut op4s = Vec::new();
        while let Ok(GatewayCommand::UpdateVoiceState { channel_id, .. }) = gw_rx.try_recv() {
            op4s.push(channel_id);
        }
        assert_eq!(op4s, [Some(Id(2)), Some(Id(2)), None], "join, one rejoin, then leave");
        assert_eq!(voice_events(&ev_rx).last(), Some(&VoiceEvent::Disconnected { reason: Some("Lost the voice connection.".into()) }));
    }

    #[tokio::test]
    async fn join_mute_kick_and_leave() {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let voice = VoiceManager::spawn(EventSink::new(ev_tx, egui::Context::default()), Arc::default());
        let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();

        voice.join(Some(Id(1)), Id(2));
        settle().await;
        assert_eq!(voice_events(&ev_rx), [VoiceEvent::Error("Not connected.".into())]);

        voice.attach_gateway(Some(gw_tx));
        voice.set_self_deaf(true);
        voice.join(Some(Id(1)), Id(2));
        settle().await;
        assert!(matches!(
            gw_rx.try_recv(),
            Ok(GatewayCommand::UpdateVoiceState { guild_id: Some(Id(1)), channel_id: Some(Id(2)), self_mute: true, self_deaf: true, self_video: false })
        ));
        assert_eq!(voice_events(&ev_rx), [VoiceEvent::Connecting { channel_id: Id(2) }]);

        // The echo of an earlier leave is ignored; after our state, a null channel is a kick.
        let feed = voice.feed();
        feed.send(my_state(None));
        feed.send(my_state(Some(Id(2))));
        settle().await;
        assert!(voice_events(&ev_rx).is_empty());
        voice.set_self_deaf(false);
        settle().await;
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { self_mute: false, self_deaf: false, .. })));
        feed.send(my_state(None));
        settle().await;
        assert_eq!(voice_events(&ev_rx), [VoiceEvent::Disconnected { reason: Some("You were disconnected from voice.".into()) }]);
        assert!(gw_rx.try_recv().is_err(), "no op 4 after a kick");

        // A user leave sends op 4 without a channel.
        voice.join(Some(Id(1)), Id(2));
        voice.leave();
        voice.leave();
        settle().await;
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { channel_id: Some(_), .. })));
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { guild_id: Some(Id(1)), channel_id: None, .. })));
        assert!(gw_rx.try_recv().is_err(), "a second leave does nothing");
        let evs = voice_events(&ev_rx);
        assert_eq!(evs.last(), Some(&VoiceEvent::Disconnected { reason: None }));

        // Logout hangs up silently towards the server.
        voice.join(Some(Id(1)), Id(2));
        voice.attach_gateway(None);
        settle().await;
        assert!(matches!(gw_rx.try_recv(), Ok(GatewayCommand::UpdateVoiceState { channel_id: Some(_), .. })));
        assert!(gw_rx.try_recv().is_err());
        assert_eq!(voice_events(&ev_rx).last(), Some(&VoiceEvent::Disconnected { reason: None }));
    }
}
