//! Voice plugins: VcNarrator (PLUGINS_NOTES S42, reads out who joins, leaves or moves through my
//! voice channel; Windows SAPI on its own thread, silent in Streamer Mode, a no-op elsewhere) and
//! VolumeBooster (S41, a per-user volume up to 400 %, applied by the local mixer only and kept in
//! plugins.toml; nothing is sent).

use super::{Plugin, enabled, opt_raw};
use crate::model::{GatewayEvent, Id, channel_type};
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// VolumeBooster
// ---------------------------------------------------------------------------

pub const MAX_VOLUME: u32 = 400;

/// Per-user volume in percent (only users set away from 100). Read by the audio thread.
static VOLUMES: RwLock<Option<HashMap<Id, u32>>> = RwLock::new(None);

fn clamp(percent: i64) -> u32 {
    percent.clamp(0, i64::from(MAX_VOLUME)) as u32
}

fn volumes<R>(f: impl FnOnce(&mut HashMap<Id, u32>) -> R) -> R {
    let mut v = VOLUMES.write().unwrap_or_else(|e| e.into_inner());
    let map = v.get_or_insert_with(|| {
        let Some(toml::Value::Table(t)) = opt_raw(Plugin::VolumeBooster, "volumes") else { return HashMap::new() };
        t.iter().filter_map(|(k, v)| Some((Id(k.parse().ok()?), clamp(v.as_integer()?)))).collect()
    });
    f(map)
}

/// A user's volume in percent (100 = as received).
pub fn volume(user: Id) -> u32 {
    if let Some(map) = VOLUMES.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return map.get(&user).copied().unwrap_or(100);
    }
    volumes(|m| m.get(&user).copied().unwrap_or(100))
}

/// The mixer's gain for `user` (`voice/audio.rs`): 1.0 unless VolumeBooster set one.
pub fn gain(user: Id) -> f32 {
    if !enabled(Plugin::VolumeBooster) {
        return 1.0;
    }
    volume(user) as f32 / 100.0
}

/// Sets a user's volume (clamped to 0..=400 %); `save` writes plugins.toml (once a drag ends).
pub fn set_volume(user: Id, percent: i64, save: bool) -> Result<u32, String> {
    let p = clamp(percent);
    let table: toml::Table = volumes(|m| {
        if p == 100 {
            m.remove(&user);
        } else {
            m.insert(user, p);
        }
        m.iter().map(|(id, v)| (id.0.to_string(), toml::Value::Integer(i64::from(*v)))).collect()
    });
    if save {
        super::set(Plugin::VolumeBooster, "volumes", toml::Value::Table(table))?;
    }
    Ok(p)
}

/// Someone else in my voice channel, with VolumeBooster on.
fn can_boost(state: &AppState, user: Id) -> bool {
    let with_me = state.voice.channel_id.is_some_and(|ch| state.voice_states.get(&ch).is_some_and(|u| u.contains_key(&user)));
    enabled(Plugin::VolumeBooster) && with_me && state.me.as_ref().is_some_and(|m| m.id != user)
}

/// In the user menu: "User Volume" for someone in my voice channel (not me); `sep`: a line
/// above it, apart from the menu's other rows.
pub fn volume_slider(ui: &mut egui::Ui, state: &AppState, user: Id, sep: bool) -> Result<(), String> {
    if !can_boost(state, user) {
        return Ok(());
    }
    if sep {
        ui.separator();
    }
    let mut v = volume(user);
    ui.label(egui::RichText::new("User Volume").size(12.0).color(ui.visuals().weak_text_color()));
    let resp = ui.add(egui::Slider::new(&mut v, 0..=MAX_VOLUME).suffix(" %"));
    if resp.changed() || resp.drag_stopped() {
        set_volume(user, i64::from(v), resp.drag_stopped() || !resp.dragged())?;
    }
    Ok(())
}

/// A call tile's right-click menu (`ui/call.rs`): the volume slider alone, kept open by a click
/// on it (Esc or a click outside closes it).
pub fn tile_menu(resp: &egui::Response, cx: &mut crate::ui::UiCx<'_>, user: Id) {
    if !can_boost(cx.state, user) {
        return;
    }
    egui::Popup::context_menu(resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        if let Err(e) = volume_slider(ui, cx.state, user, false) {
            cx.view.toast(format!("Could not save plugin settings: {e}"), true);
        }
    });
}

// ---------------------------------------------------------------------------
// VcNarrator
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Narration {
    Join,
    Leave,
    Move(Id),
}

/// What a voice state change from `old` to `new` channel means for my channel `mine`.
fn narration(mine: Id, old: Option<Id>, new: Option<Id>) -> Option<Narration> {
    if old == new {
        return None;
    }
    match (old, new) {
        (None, Some(n)) if n == mine => Some(Narration::Join),
        (Some(_), Some(n)) if n == mine => Some(Narration::Move(n)),
        (Some(o), None) if o == mine => Some(Narration::Leave),
        (Some(o), Some(n)) if o == mine => Some(Narration::Move(n)),
        _ => None,
    }
}

/// After a READY the voice states arrive again one by one: not news, so not read out.
const AFTER_READY: std::time::Duration = std::time::Duration::from_secs(15);
static READY_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Before a VOICE_STATE_UPDATE reaches the state (which still has the old channel).
pub fn narrate(state: &AppState, settings: &crate::config::Settings, ev: &GatewayEvent) {
    if !enabled(Plugin::VcNarrator) {
        return;
    }
    let mut ready = READY_AT.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(ev, GatewayEvent::Ready(_)) {
        *ready = Some(std::time::Instant::now());
    }
    let GatewayEvent::VoiceStateUpdate(v) = ev else { return };
    if settings.streamer.enabled || ready.is_some_and(|t| t.elapsed() < AFTER_READY) {
        return;
    }
    drop(ready);
    let Some(mine) = state.voice.channel_id else { return };
    if state.me.as_ref().is_none_or(|m| m.id == v.user_id) || state.channels.get(&mine).is_some_and(|c| c.kind == channel_type::GUILD_STAGE_VOICE) {
        return;
    }
    let old = state.voice_states.iter().find(|(_, users)| users.get(&v.user_id).is_some_and(|o| o.guild_id == v.guild_id)).map(|(ch, _)| *ch);
    let Some(what) = narration(mine, old, v.channel_id) else { return };
    let user = state.users.get(&v.user_id).or_else(|| v.member.as_ref().and_then(|m| m.user.as_ref()));
    let name = user.map_or_else(|| "Someone".to_owned(), |u| state.member_display_name(v.guild_id, u, v.member.as_ref()));
    say(match what {
        Narration::Join => format!("{name} joined"),
        Narration::Leave => format!("{name} left"),
        Narration::Move(ch) => format!("{name} moved to {}", state.channel_name(ch)),
    });
}

/// Speaks `text` on the narrator thread (started on first use, then idle on its queue).
#[cfg(windows)]
fn say(text: String) {
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};
    static TX: Mutex<Option<Sender<String>>> = Mutex::new(None);
    let mut tx = TX.lock().unwrap_or_else(|e| e.into_inner());
    if tx.is_none() {
        let (send, recv) = channel();
        if std::thread::Builder::new().name("vc-narrator".into()).spawn(move || speak_all(recv)).is_err() {
            return;
        }
        *tx = Some(send);
    }
    // The thread is gone (no speech engine): forget it, the next event tries again.
    if tx.as_ref().is_some_and(|t| t.send(text).is_err()) {
        *tx = None;
    }
}

#[cfg(windows)]
fn speak_all(texts: std::sync::mpsc::Receiver<String>) {
    use windows::Win32::Media::Speech::{ISpVoice, SPF_IS_NOT_XML, SpVoice};
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    // SAFETY: COM stays initialised for this thread's whole life (it never uninitialises).
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        return;
    }
    // SAFETY: SpVoice is SAPI's in-process voice class; the interface lives on this thread.
    let Ok(voice) = (unsafe { CoCreateInstance::<_, ISpVoice>(&SpVoice, None, CLSCTX_ALL) }) else { return };
    for text in texts {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        // Synchronous on this thread; plain text, so names can't carry SAPI XML.
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let _ = unsafe { voice.Speak(windows::core::PCWSTR(wide.as_ptr()), SPF_IS_NOT_XML.0 as u32, None) };
    }
}

#[cfg(not(windows))]
fn say(_text: String) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_booster_clamps() {
        assert_eq!((clamp(-5), clamp(0), clamp(250), clamp(400), clamp(9000)), (0, 0, 250, 400, 400));
        assert_eq!(set_volume(Id(77), 1000, false), Ok(400));
        assert_eq!(volume(Id(77)), 400);
        // Off by default: the mixer leaves everyone at 1.0.
        assert_eq!(gain(Id(77)), 1.0);
        assert_eq!(set_volume(Id(77), 100, false), Ok(100));
        assert_eq!(volume(Id(78)), 100);
    }

    #[test]
    fn narrations() {
        let (mine, other) = (Id(1), Id(2));
        assert_eq!(narration(mine, None, Some(mine)), Some(Narration::Join));
        assert_eq!(narration(mine, Some(mine), None), Some(Narration::Leave));
        assert_eq!(narration(mine, Some(mine), Some(other)), Some(Narration::Move(other)));
        assert_eq!(narration(mine, Some(other), Some(mine)), Some(Narration::Move(mine)));
        assert_eq!(narration(mine, Some(mine), Some(mine)), None);
        assert_eq!(narration(mine, None, Some(other)), None);
    }
}
