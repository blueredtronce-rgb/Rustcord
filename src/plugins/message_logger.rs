//! MessageLogger (PLUGINS_NOTES S19): deleted messages stay in the list, greyed and tinted;
//! edits keep their earlier texts, shown greyed above the current one. Memory only (never
//! written to the cache, gone on restart or when the plugin is turned off) and bounded.

use super::{Plugin, enabled, opt_bool};
use crate::model::{GatewayEvent, Id, Message};
use crate::state::AppState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

/// Deleted messages kept; the oldest really goes when one more arrives.
pub const MAX_DELETED: usize = 300;
/// Messages with an edit history kept.
pub const MAX_EDITED: usize = 300;
/// Earlier versions kept per message.
pub const MAX_VERSIONS: usize = 8;
/// Characters kept of each earlier version.
const MAX_TEXT: usize = 2000;
/// Message flag EPHEMERAL: never logged (only I saw it, and it was never meant to stay).
const EPHEMERAL: u64 = 1 << 6;

#[derive(Default)]
pub struct Log {
    /// (channel, message), oldest first.
    deleted: VecDeque<(Id, Id)>,
    deleted_ids: HashSet<Id>,
    /// Earlier texts, oldest first.
    edits: HashMap<Id, Vec<String>>,
    edit_order: VecDeque<Id>,
}

impl Log {
    /// Marks a message deleted; returns the (channel, message) that falls out of the bound.
    fn delete(&mut self, ch: Id, id: Id) -> Option<(Id, Id)> {
        if !self.deleted_ids.insert(id) {
            return None;
        }
        self.deleted.push_back((ch, id));
        if self.deleted.len() <= MAX_DELETED {
            return None;
        }
        let old = self.deleted.pop_front()?;
        self.deleted_ids.remove(&old.1);
        Some(old)
    }

    fn edit(&mut self, id: Id, before: &str) {
        let list = self.edits.entry(id).or_insert_with(|| {
            self.edit_order.push_back(id);
            Vec::new()
        });
        list.push(before.chars().take(MAX_TEXT).collect());
        if list.len() > MAX_VERSIONS {
            list.remove(0);
        }
        while self.edit_order.len() > MAX_EDITED {
            if let Some(old) = self.edit_order.pop_front() {
                self.edits.remove(&old);
            }
        }
    }
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Log) -> R) -> R {
    let mut g = LOG.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Log::default))
}

/// Forgets everything (plugin turned off). Fallback for callers without the state: `purge`.
pub fn clear() {
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Plugin turned off: the kept deleted messages leave the list too (else they would show, and be
/// cached, as live ones), then the log is forgotten.
pub fn purge(state: &mut AppState) {
    let log = LOG.lock().unwrap_or_else(|e| e.into_inner()).take();
    for (ch, id) in log.map(|l| l.deleted).unwrap_or_default() {
        if let Some(e) = state.messages.get_mut(&ch) {
            e.msgs.retain(|m| m.id != id);
        }
    }
}

pub fn is_deleted(id: Id) -> bool {
    enabled(Plugin::MessageLogger) && with(|l| l.deleted_ids.contains(&id))
}

/// Earlier texts of a message, oldest first.
pub fn versions(id: Id) -> Vec<String> {
    if !enabled(Plugin::MessageLogger) {
        return Vec::new();
    }
    with(|l| l.edits.get(&id).cloned().unwrap_or_default())
}

/// Whether a message is logged at all (bots / me / ephemeral excluded per options).
fn loggable(state: &AppState, m: &Message) -> bool {
    let mine = state.me.as_ref().is_some_and(|u| u.id == m.author.id);
    m.flags & EPHEMERAL == 0 && !(m.author.bot && opt_bool(Plugin::MessageLogger, "ignoreBots")) && !(mine && opt_bool(Plugin::MessageLogger, "ignoreSelf"))
}

fn find(state: &AppState, ch: Id, id: Id) -> Option<&Message> {
    let msgs = &state.messages.get(&ch)?.msgs;
    msgs.binary_search_by_key(&id, |m| m.id).ok().map(|i| &msgs[i])
}

/// Keeps deletes of loaded messages (the event is swallowed or trimmed) and records edits.
pub fn on_gateway(state: &mut AppState, ev: GatewayEvent) -> Option<GatewayEvent> {
    if !enabled(Plugin::MessageLogger) {
        return Some(ev);
    }
    let deletes = opt_bool(Plugin::MessageLogger, "logDeletes");
    match ev {
        GatewayEvent::MessageDelete { id, channel_id } if deletes => {
            let rest = keep(state, channel_id, vec![id]);
            (!rest.is_empty()).then_some(GatewayEvent::MessageDelete { id, channel_id })
        }
        GatewayEvent::MessageDeleteBulk { ids, channel_id } if deletes => {
            let ids = keep(state, channel_id, ids);
            (!ids.is_empty()).then_some(GatewayEvent::MessageDeleteBulk { ids, channel_id })
        }
        GatewayEvent::MessageUpdate(u) => {
            if opt_bool(Plugin::MessageLogger, "logEdits")
                && let (Some(new), Some(m)) = (u.content.as_deref(), find(state, u.channel_id, u.id))
                && new != m.content
                && !m.content.is_empty()
                && loggable(state, m)
            {
                let before = m.content.clone();
                with(|l| l.edit(u.id, &before));
            }
            Some(GatewayEvent::MessageUpdate(u))
        }
        ev => Some(ev),
    }
}

/// Logs the loaded, loggable ones of `ids`; returns the ids the state should still remove.
fn keep(state: &mut AppState, ch: Id, ids: Vec<Id>) -> Vec<Id> {
    let mut rest = Vec::new();
    for id in ids {
        if !find(state, ch, id).is_some_and(|m| loggable(state, m)) {
            rest.push(id);
            continue;
        }
        // Over the bound: the oldest logged message really goes now.
        if let Some((old_ch, old)) = with(|l| l.delete(ch, id))
            && let Some(e) = state.messages.get_mut(&old_ch)
        {
            e.msgs.retain(|m| m.id != old);
        }
    }
    rest
}

/// Earlier versions above a message's text, greyed (`messages.rs` body).
pub fn show_versions(ui: &mut egui::Ui, theme: &crate::theme::Theme, id: Id) {
    let list = versions(id);
    if list.is_empty() {
        return;
    }
    let muted = theme.colors.text_muted.to_color32().gamma_multiply(0.8);
    for text in list {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).color(muted)).wrap());
            ui.label(egui::RichText::new("(edited)").size(theme.font.size - 3.0).color(muted));
        });
    }
}

/// Removes a deleted message from the list now (message menu).
pub fn forget(state: &mut AppState, ch: Id, id: Id) {
    with(|l| {
        l.deleted.retain(|d| d.1 != id);
        l.deleted_ids.remove(&id);
        l.edits.remove(&id);
    });
    if let Some(e) = state.messages.get_mut(&ch) {
        e.msgs.retain(|m| m.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_and_edited_are_bounded() {
        let mut l = Log::default();
        for i in 0..MAX_DELETED as u64 {
            assert_eq!(l.delete(Id(1), Id(i)), None);
        }
        // A repeated delete changes nothing; one more pushes the oldest out.
        assert_eq!(l.delete(Id(1), Id(5)), None);
        assert_eq!(l.delete(Id(2), Id(9999)), Some((Id(1), Id(0))));
        assert_eq!((l.deleted.len(), l.deleted_ids.len()), (MAX_DELETED, MAX_DELETED));
        assert!(!l.deleted_ids.contains(&Id(0)) && l.deleted_ids.contains(&Id(9999)));

        for v in 0..MAX_VERSIONS + 3 {
            l.edit(Id(7), &format!("v{v}"));
        }
        let kept = &l.edits[&Id(7)];
        assert_eq!(kept.len(), MAX_VERSIONS);
        assert_eq!(kept[0], "v3");
        l.edit(Id(8), &"x".repeat(MAX_TEXT * 2));
        assert_eq!(l.edits[&Id(8)][0].len(), MAX_TEXT);
        for i in 0..MAX_EDITED as u64 + 10 {
            l.edit(Id(100 + i), "a");
        }
        assert_eq!((l.edits.len(), l.edit_order.len()), (MAX_EDITED, MAX_EDITED));
        assert!(!l.edits.contains_key(&Id(7)));
    }

    #[test]
    fn purge_drops_kept_deletions_from_the_state() {
        let mut s = AppState::default();
        let msgs = [1, 2, 3].map(|i| Message { id: Id(i), channel_id: Id(9), ..Default::default() }).to_vec();
        s.messages.insert(Id(9), crate::state::ChannelMessages { msgs, ..Default::default() });
        with(|l| l.delete(Id(9), Id(2)));
        purge(&mut s);
        assert_eq!(s.messages[&Id(9)].msgs.iter().map(|m| m.id).collect::<Vec<_>>(), [Id(1), Id(3)]);
        assert!(LOG.lock().unwrap().is_none());
    }
}
