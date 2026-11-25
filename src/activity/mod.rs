//! Activities like the official desktop client: game detection (a background thread lists
//! processes, `scan.rs`, and matches them against the detectable-games list, `detect.rs`, plus
//! the games I added), my game in op 3 while "Share detected activity" (settings 11.3) is on,
//! and a picture for everyone's activity: rich-presence art, Spotify album art, else the
//! application's icon, else the type icon (drawn by the UI).
//!
//! Traffic, all through the rate limiter: `GET /games/detectable` at most once a day (saved
//! compact on disk), `GET /applications/public` once per application id that is shown without
//! art (batched; answers saved on disk), and one op 3 per change of the game I share. The
//! Registered Games list stays on this device.
//!
//! OWNERSHIP: activities engineer.

mod detect;
mod scan;
#[cfg(windows)]
pub(crate) mod pulse_rpc;

pub use detect::Table;
pub use scan::{Service, Window, table_path};
// The game overlay's foreground watcher asks the same two questions as the scan (ui/overlay).
#[cfg(windows)]
pub(crate) use scan::{image as process_image, processes};

use crate::api::cdn;
use crate::backend::{Command, CommandSender, ConnState};
use crate::model::{Activity, Id, RichPresence};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

pub use detect::{file_name, stale as table_stale};

/// Past this many answered app icons the memory cache starts over.
const MAX_ICONS: usize = 2000;
/// App icons asked for but not sent yet (rows shown while connected).
const MAX_WANTED: usize = 100;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// My game as op 3 carries it (type 0).
#[derive(Clone, Debug, PartialEq)]
pub struct GameActivity {
    pub name: String,
    pub application_id: Option<Id>,
    /// When the game's process started (unix ms).
    pub start_ms: u64,
}

impl GameActivity {
    /// As a presence shows it.
    pub fn to_activity(&self) -> Activity {
        let rich = RichPresence { start: Some(self.start_ms), application_id: self.application_id, ..Default::default() };
        Activity { kind: 0, name: self.name.clone(), rich: Some(Box::new(rich)) }
    }
}

/// A program I added on the Registered Games page ("Add it!").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddedGame {
    pub name: String,
    /// `detect::normalize`d full path.
    pub exe: String,
}

#[derive(Debug)]
pub enum ActivityCommand {
    /// My game in op 3 (None: no game), keeping my status and custom status.
    SetGame(Option<GameActivity>),
    SetListening(Option<Activity>),
    #[cfg(windows)]
    ProxyPulseCover(String),
    /// `GET /applications/public` for these ids (≤ 25) -> `ActivityEvent::Apps`.
    FetchApps(Vec<Id>),
    /// Save my added games (local only) and scan again.
    SetAdded(Vec<AddedGame>),
    /// The windows the "Add it!" picker offers -> `ActivityEvent::Windows`.
    ListWindows,
}

#[derive(Debug)]
pub enum ActivityEvent {
    #[cfg(windows)]
    PulseSync(Option<Activity>),
    #[cfg(windows)]
    PulseCover(Arc<egui::ColorImage>),
    #[cfg(windows)]
    PulseCoverProxy { original: String, asset: Option<String> },
    ListeningConfirmation(Option<bool>),
    /// The games list (from disk, or fetched).
    Table(Arc<Table>),
    /// The game I am playing changed.
    Detected(Option<GameActivity>),
    /// My added games (at start).
    Added(Vec<AddedGame>),
    /// App icon hashes (None: none, or unknown app).
    Apps(Vec<(Id, Option<String>)>),
    Windows(Vec<Window>),
}

/// `AppState::activity`.
#[derive(Debug, Default)]
pub struct ActivityState {
    #[cfg(windows)]
    pub pulse_sync: Option<Activity>,
    #[cfg(windows)]
    pub pulse_cover: Option<Arc<egui::ColorImage>>,
    pub listening_confirmation: Option<bool>,
    #[cfg(windows)]
    pulse_cover_proxy: Option<(String, Option<String>)>,
    pub table: Option<Arc<Table>>,
    /// The game running now (shared or not).
    pub detected: Option<GameActivity>,
    pub added: Vec<AddedGame>,
    /// The "Add it!" picker's list, once answered.
    pub windows: Option<Vec<Window>>,
    /// `/applications/public` answers; None = no icon, or asked and not answered yet.
    icons: HashMap<Id, Option<String>>,
    /// Asked for by drawn rows, sent by `logic`.
    wanted: Vec<Id>,
    /// The game my session's presence carries (the last `SetGame`).
    sent: Option<GameActivity>,
    sent_listening: Option<Activity>,
    listening_dirty: bool,
    listening_shared: bool,
}

impl ActivityState {
    pub fn apply(&mut self, ev: ActivityEvent) {
        match ev {
            #[cfg(windows)]
            ActivityEvent::PulseSync(activity) => {
                if self.pulse_sync != activity {
                    self.pulse_cover = None;
                    self.listening_dirty = true;
                }
                self.pulse_sync = activity;
            }
            #[cfg(windows)]
            ActivityEvent::PulseCover(cover) => self.pulse_cover = Some(cover),
            #[cfg(windows)]
            ActivityEvent::PulseCoverProxy { original, asset } => {
                if let Some((url, proxy)) = self.pulse_cover_proxy.as_mut() && *url == original {
                    *proxy = asset;
                    self.listening_dirty = true;
                }
            }
            ActivityEvent::ListeningConfirmation(confirmation) => self.listening_confirmation = confirmation,
            ActivityEvent::Table(t) => self.table = Some(t),
            ActivityEvent::Detected(g) => self.detected = g,
            ActivityEvent::Added(a) => self.added = a,
            ActivityEvent::Apps(list) => {
                if self.icons.len() + list.len() > MAX_ICONS {
                    self.icons.clear();
                }
                self.icons.extend(list);
            }
            ActivityEvent::Windows(w) => self.windows = Some(w),
        }
    }

    /// What survives a logout: this device's games, not the account's session or its friends'
    /// app icons.
    pub fn after_logout(self) -> Self {
        Self {
            table: self.table, detected: self.detected, added: self.added,
            #[cfg(windows)]
            pulse_sync: self.pulse_sync,
            #[cfg(windows)]
            pulse_cover: self.pulse_cover,
            listening_dirty: true,
            ..Default::default()
        }
    }

    /// Application `id`'s icon hash from the games list or an answer; else it is asked for
    /// (when `ask`). Returns (hash, newly asked).
    fn app_icon(&mut self, id: Id, ask: bool) -> (Option<String>, bool) {
        if let Some(h) = self.table.as_ref().and_then(|t| t.icon(id)) {
            return (Some(h), false);
        }
        match self.icons.get(&id) {
            Some(h) => (h.clone(), false),
            None if ask && !self.wanted.contains(&id) && self.wanted.len() < MAX_WANTED => {
                self.wanted.push(id);
                (None, true)
            }
            None => (None, false),
        }
    }
}

/// "Share detected activity" (settings proto 11.3, default on).
pub fn sharing(state: &AppState) -> bool {
    state.account.settings.proto_varint(11, &[3], true).is_none_or(|v| v != 0)
}

/// My detected game while I share it, as others see it.
pub fn my_game(state: &AppState) -> Option<Activity> {
    state.activity.detected.as_ref().filter(|_| sharing(state)).map(GameActivity::to_activity)
}

/// Someone's activity as rows and cards show it: their presence's; mine falls back to my shared
/// game (my presence echo lags, and offline it never comes).
pub fn activity_of(state: &AppState, user: Id) -> Option<Cow<'_, Activity>> {
    #[cfg(windows)]
    if state.me.as_ref().is_some_and(|me| me.id == user) && let Some(activity) = &state.activity.pulse_sync {
        return Some(Cow::Borrowed(activity));
    }
    if let Some(a) = state.presence_of(user).and_then(|p| p.activity.as_ref()) {
        return Some(Cow::Borrowed(a));
    }
    state.me.as_ref().filter(|m| m.id == user).and_then(|_| my_game(state)).map(Cow::Owned)
}

fn online(state: &AppState) -> bool {
    state.conn == ConnState::Connected && !crate::config::offline_mode()
}

/// The picture of `a` for a `px` square: its rich-presence image (Spotify album art included),
/// else its application's icon; None = draw the type icon. An unknown application is asked
/// for once (batched by `logic` on the next frame).
pub fn art_url(ctx: &egui::Context, state: &mut AppState, a: &Activity, px: u32) -> Option<String> {
    #[cfg(windows)]
    if state.activity.pulse_sync.as_ref() == Some(a)
        && let Some(url) = a.rich.as_ref().and_then(|r| r.large_image.as_ref()).filter(|url| pulse_rpc::cover_url(url))
    {
        return Some(url.clone());
    }
    if let Some(url) = cdn::activity_image_url(a, false, px) {
        return Some(url);
    }
    let id = a.rich.as_ref()?.application_id?;
    let ask = online(state);
    let (hash, asked) = state.activity.app_icon(id, ask);
    if asked {
        ctx.request_repaint();
    }
    cdn::app_icon_url(id, &hash?, px)
}

/// Each frame (`App::logic`, also while minimized), only while connected (READY's settings are
/// in by then): one op 3 when the game to share changed, and the batched app-icon request.
pub fn logic(state: &mut AppState, cmd: &CommandSender, share_listening: bool) {
    if !online(state) {
        return;
    }
    let want = state.activity.detected.clone().filter(|_| sharing(state));
    if want != state.activity.sent {
        state.activity.sent = want.clone();
        cmd.send(Command::Activity(ActivityCommand::SetGame(want)));
    }
    let allow_listening = share_listening && sharing(state);
    let activity = &mut state.activity;
    let dirty = std::mem::take(&mut activity.listening_dirty);
    if dirty || allow_listening != activity.listening_shared {
        activity.listening_shared = allow_listening;
        let listening: Option<Activity> = if allow_listening {
            #[cfg(windows)]
            { listening_activity(activity, cmd) }
            #[cfg(not(windows))]
            { None }
        } else { None };
        if listening != activity.sent_listening {
            activity.listening_confirmation = None;
            activity.sent_listening = listening.clone();
            cmd.send(Command::Activity(ActivityCommand::SetListening(listening)));
        }
    }
    let a = &mut state.activity;
    if !a.wanted.is_empty() {
        let ids: Vec<Id> = a.wanted.drain(..a.wanted.len().min(crate::api::apps::MAX_APP_IDS)).collect();
        // Asked once: an unknown app or a failed request is not asked again this session.
        a.icons.extend(ids.iter().map(|&id| (id, None)));
        cmd.send(Command::Activity(ActivityCommand::FetchApps(ids)));
    }
}

#[cfg(windows)]
fn listening_activity(state: &mut ActivityState, cmd: &CommandSender) -> Option<Activity> {
    let mut activity = state.pulse_sync.clone()?;
    if let Some(rich) = &mut activity.rich
        && let Some(url) = rich.large_image.as_ref().filter(|url| pulse_rpc::cover_url(url))
    {
        if state.pulse_cover_proxy.as_ref().is_none_or(|(asked, _)| asked != url) {
            state.pulse_cover_proxy = Some((url.clone(), None));
            cmd.send(Command::Activity(ActivityCommand::ProxyPulseCover(url.clone())));
        }
        rich.large_image = state.pulse_cover_proxy.as_ref().and_then(|(_, asset)| asset.clone());
    }
    Some(activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::settings_proto::SettingValue;

    fn sent(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok()).map(|c| format!("{c:?}")).collect()
    }

    #[cfg(windows)]
    #[test]
    fn pulse_sync_is_only_published_with_both_sharing_controls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cmd = CommandSender(tx);
        let mut s = AppState::default();
        s.conn = ConnState::Connected;
        let activity = Activity { kind: 2, name: "Yandex Music".into(), rich: Some(Box::new(RichPresence {
            details: Some("Track".into()), application_id: Some(Id(1124055337234858005)), ..Default::default()
        })) };
        s.activity.apply(ActivityEvent::PulseSync(Some(activity.clone())));
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty());
        logic(&mut s, &cmd, true);
        assert!(matches!(rx.try_recv(), Ok(Command::Activity(ActivityCommand::SetListening(Some(a)))) if a == activity));
        s.account.settings.set_local(11, &[(vec![3], SettingValue::Wrapped(0))]);
        logic(&mut s, &cmd, true);
        assert!(matches!(rx.try_recv(), Ok(Command::Activity(ActivityCommand::SetListening(None)))));
    }

    #[test]
    fn op3_once_per_change_and_only_while_shared() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cmd = CommandSender(tx);
        let mut s = AppState::default();
        let game = GameActivity { name: "Factorio".into(), application_id: Some(Id(7)), start_ms: 5 };
        s.activity.apply(ActivityEvent::Detected(Some(game.clone())));
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty(), "not connected: READY's settings may not be in");
        s.conn = ConnState::Connected;
        logic(&mut s, &cmd, false);
        assert_eq!(sent(&mut rx), [format!("{:?}", Command::Activity(ActivityCommand::SetGame(Some(game.clone()))))]);
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty(), "no change, no op 3");
        // Sharing switched off: the game goes, once.
        s.account.settings.set_local(11, &[(vec![3], SettingValue::Wrapped(0))]);
        assert!(my_game(&s).is_none());
        logic(&mut s, &cmd, false);
        logic(&mut s, &cmd, false);
        assert_eq!(sent(&mut rx), ["Activity(SetGame(None))"]);
        // While off, a new game sends nothing.
        s.activity.apply(ActivityEvent::Detected(Some(GameActivity { name: "Celeste".into(), ..game })));
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty());
        // A new login starts without a game: sent again once sharing is on.
        s.account.settings.set_local(11, &[(vec![3], SettingValue::Wrapped(1))]);
        s.activity = std::mem::take(&mut s.activity).after_logout();
        assert!(s.activity.table.is_none() && s.activity.detected.is_some());
        logic(&mut s, &cmd, false);
        assert_eq!(sent(&mut rx).len(), 1);
    }

    #[test]
    fn app_icons_are_asked_once_in_batches() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cmd = CommandSender(tx);
        let mut s = AppState::default();
        s.conn = ConnState::Connected;
        let ctx = egui::Context::default();
        let game = |id: u64| GameActivity { name: "x".into(), application_id: Some(Id(id)), start_ms: 0 }.to_activity();
        for id in [1, 2, 1, 3, 2] {
            assert_eq!(art_url(&ctx, &mut s, &game(id), 64), None);
        }
        logic(&mut s, &cmd, false);
        assert_eq!(sent(&mut rx), ["Activity(FetchApps([1, 2, 3]))"]);
        for id in [1, 2, 3] {
            assert_eq!(art_url(&ctx, &mut s, &game(id), 64), None);
        }
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty(), "asked: not again, even before the answer");
        s.activity.apply(ActivityEvent::Apps(vec![(Id(1), Some("abc".into())), (Id(2), None)]));
        assert_eq!(art_url(&ctx, &mut s, &game(1), 64).as_deref(), Some("https://cdn.discordapp.com/app-icons/1/abc.png?size=64"));
        // The games list answers without a request.
        let json = r#"[{"id":"9","name":"x","icon_hash":"0123456789abcdef0123456789abcdef"}]"#;
        s.activity.apply(ActivityEvent::Table(Arc::new(Table::from_json(json.as_bytes()).unwrap())));
        assert!(art_url(&ctx, &mut s, &game(9), 64).is_some_and(|u| u.contains("/app-icons/9/0123456789abcdef0123456789abcdef.png")));
        // Rich-presence art first; offline nothing is asked.
        let mut spotify = game(4);
        spotify.rich.as_mut().unwrap().large_image = Some("spotify:ab67616d0000b2734f8ef2c2c1b3a9a1c4d5e6f7".into());
        assert!(art_url(&ctx, &mut s, &spotify, 64).is_some_and(|u| u.starts_with("https://i.scdn.co/image/")));
        s.conn = ConnState::Offline;
        assert_eq!(art_url(&ctx, &mut s, &game(5), 64), None);
        s.conn = ConnState::Connected;
        logic(&mut s, &cmd, false);
        assert!(sent(&mut rx).is_empty());
    }
}
