//! RelationshipNotifier (PLUGINS_NOTES S43): a toast (and a desktop notification) when someone
//! removes me as a friend, or I'm removed from a server or a group DM. My own removals (asked
//! in a confirm dialog: Remove Friend, Leave Server, Leave Group) are not announced.
//! OnePingPerDM (S44): one desktop notification per DM until it is read.

use super::{Plugin, enabled, opt_bool};
use crate::backend::{AccountCommand, Command, SocialCommand};
use crate::model::{GatewayEvent, Id, channel_type, relationship_type};
use crate::state::AppState;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A removal asked for in a confirm dialog counts as mine for this long.
const MINE_FOR: Duration = Duration::from_secs(120);

static MINE: Mutex<Vec<(Id, Instant)>> = Mutex::new(Vec::new());

/// Called every frame: remembers the target of a removal the user is being asked about (the
/// dialog's command is mine whether or not it is confirmed; a stranger's removal of the same
/// thing in the next two minutes is not announced: rare and harmless).
pub fn watch_confirm(view: &crate::ui::ViewState) {
    if !enabled(Plugin::RelationshipNotifier) {
        return;
    }
    let target = match view.confirm.as_ref().map(|c| &c.then) {
        Some(Command::Social(SocialCommand::RemoveRelationship { user_id, .. })) => *user_id,
        Some(Command::Social(SocialCommand::CloseDm { channel_id, .. })) => *channel_id,
        Some(Command::Account(AccountCommand::LeaveGuild { guild_id })) => *guild_id,
        _ => return,
    };
    let mut mine = MINE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    mine.retain(|(_, t)| now.duration_since(*t) < MINE_FOR);
    match mine.iter_mut().find(|(id, _)| *id == target) {
        Some(e) => e.1 = now,
        None => mine.push((target, now)),
    }
}

fn is_mine(id: Id) -> bool {
    MINE.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(m, t)| *m == id && t.elapsed() < MINE_FOR)
}

/// The notice for an event, read from the state before the event changes it.
fn notice(state: &AppState, ev: &GatewayEvent) -> Option<String> {
    match ev {
        GatewayEvent::RelationshipRemove { id } if opt_bool(Plugin::RelationshipNotifier, "friends") => {
            let r = state.social.relationships.get(id)?;
            (r.kind == relationship_type::FRIEND).then(|| format!("{} removed you as a friend.", state.friend_name(*id)))
        }
        GatewayEvent::GuildDelete { id, unavailable: false } if opt_bool(Plugin::RelationshipNotifier, "servers") => {
            Some(format!("You were removed from the server {}.", state.guild(*id)?.name))
        }
        GatewayEvent::ChannelDelete { id, .. } if opt_bool(Plugin::RelationshipNotifier, "groups") => {
            let c = state.channels.get(id).filter(|c| c.kind == channel_type::GROUP_DM)?;
            Some(format!("You were removed from the group {}.", c.name.clone().unwrap_or_else(|| state.channel_name(*id))))
        }
        _ => None,
    }
}

/// OnePingPerDM: per DM, my read position when it last notified. A DM notifies again only once
/// it was read since (the position moved).
static PINGED: Mutex<Option<HashMap<Id, Option<Id>>>> = Mutex::new(None);

/// Whether a message notification for `channel_id` may show (`ui/mod.rs`, after the state made
/// it): with OnePingPerDM only the first per DM until that DM is read.
pub fn one_ping(state: &AppState, channel_id: Id) -> bool {
    if !enabled(Plugin::OnePingPerDm) || !state.channels.get(&channel_id).is_some_and(|c| matches!(c.kind, channel_type::DM | channel_type::GROUP_DM)) {
        return true;
    }
    let read = state.read_states.get(&channel_id).and_then(|r| r.last_message_id);
    gate(PINGED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new), channel_id, read)
}

fn gate(pinged: &mut HashMap<Id, Option<Id>>, ch: Id, read: Option<Id>) -> bool {
    if pinged.get(&ch) == Some(&read) {
        return false;
    }
    pinged.insert(ch, read);
    true
}

pub fn on_gateway(state: &AppState, view: &mut crate::ui::ViewState, settings: &crate::config::Settings, ev: &GatewayEvent) {
    if !enabled(Plugin::RelationshipNotifier) {
        return;
    }
    let target = match ev {
        GatewayEvent::RelationshipRemove { id } | GatewayEvent::GuildDelete { id, .. } | GatewayEvent::ChannelDelete { id, .. } => *id,
        _ => return,
    };
    if is_mine(target) {
        return;
    }
    let Some(text) = notice(state, ev) else { return };
    // Through `notification` so Streamer Mode silences or anonymizes it like a message's.
    if opt_bool(Plugin::RelationshipNotifier, "notify")
        && let Some((title, body)) = settings.notification(crate::config::APP_NAME, &text)
    {
        crate::notify::show(title, body);
    }
    view.toast(text, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_ping_per_dm_until_read() {
        let mut pinged = HashMap::new();
        let (dm, other) = (Id(1), Id(2));
        // First message: notifies; more while unread: silent.
        assert!(gate(&mut pinged, dm, Some(Id(10))));
        assert!(!gate(&mut pinged, dm, Some(Id(10))));
        assert!(!gate(&mut pinged, dm, Some(Id(10))));
        // Other DMs are separate; a never-read DM counts too.
        assert!(gate(&mut pinged, other, None));
        assert!(!gate(&mut pinged, other, None));
        // Read since (position moved): the next one notifies again.
        assert!(gate(&mut pinged, dm, Some(Id(15))));
        assert!(!gate(&mut pinged, dm, Some(Id(15))));
        // Off by default: everything notifies.
        assert!(one_ping(&AppState::default(), dm));
    }
}
