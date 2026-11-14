//! Composer and message-list plugins: ClearURLs on send, SilentMessageToggle, SilentTyping,
//! NoReplyMention, ReplyTimestamp, NoBlockedMessages, WhoReacted, FavoriteEmojiFirst and
//! TypingTweaks.

use super::{Plugin, enabled, opt_bool, opt_text};
use crate::backend::{ChatCommand, Command};
use crate::model::{Emoji, Id, Message, User, relationship_type};
use crate::state::AppState;
use crate::ui::UiCx;
use egui::{Color32, Pos2, Rect, Sense, Vec2, pos2, vec2};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Sending: ClearURLs, SilentMessageToggle, SilentTyping, NoReplyMention
// ---------------------------------------------------------------------------

/// Text of a message I send or edit, links cleaned (ClearURLs).
pub fn outgoing(text: String) -> String {
    if !enabled(Plugin::ClearUrls) {
        return text;
    }
    match super::clear_urls::clean_text(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(t) => t,
    }
}

/// The composer's "silent" switch (this session).
static SILENT: AtomicBool = AtomicBool::new(false);
/// Nonces of messages sent silently (the last 32; a retry reuses its nonce).
static SILENT_NONCES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// Message flag SUPPRESS_NOTIFICATIONS ("@silent").
pub const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;

/// The composer's bell switch (left of the GIF button): 24 px, centred on the first text line
/// of height `row_h`.
pub fn silent_button(ui: &mut egui::Ui, row_h: f32, (text, muted, tint): (Color32, Color32, Color32)) {
    if !enabled(Plugin::SilentMessageToggle) {
        return;
    }
    let on = SILENT.load(Ordering::Relaxed);
    let (slot, _) = ui.allocate_exact_size(vec2(24.0, row_h), Sense::hover());
    let rect = Rect::from_center_size(slot.center(), Vec2::splat(24.0));
    let resp = ui.interact(rect, ui.make_persistent_id("rc_silent_toggle"), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4, tint);
    }
    let color = if on { text } else { muted };
    let bell = if on { crate::ui::icons::Icon::BellOff } else { crate::ui::icons::Icon::Bell };
    crate::ui::icons::paint(ui.painter(), bell, rect.shrink(4.0), color, Color32::TRANSPARENT);
    let tip = if on { "Silent: on (no notifications)" } else { "Silent: off" };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, tip));
    if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        SILENT.store(!on, Ordering::Relaxed);
    }
}

/// A message goes out with `nonce` (composer `send`): remembered as silent while the switch is
/// on (then turned off with "autoDisable").
pub fn on_send(nonce: &str) {
    if !enabled(Plugin::SilentMessageToggle) || !SILENT.load(Ordering::Relaxed) {
        return;
    }
    let mut list = SILENT_NONCES.lock().unwrap_or_else(|e| e.into_inner());
    list.push_back(nonce.to_owned());
    while list.len() > 32 {
        list.pop_front();
    }
    if opt_bool(Plugin::SilentMessageToggle, "autoDisable") {
        SILENT.store(false, Ordering::Relaxed);
    }
}

/// `flags` of the message body with `nonce` (`api::send_body`). Not removed here: a retry
/// resends the same nonce and must stay silent (`on_send` keeps the list bounded).
pub fn send_flags(nonce: &str) -> u64 {
    let list = SILENT_NONCES.lock().unwrap_or_else(|e| e.into_inner());
    if list.iter().any(|n| n == nonce) { SUPPRESS_NOTIFICATIONS } else { 0 }
}

/// SilentTyping: the composer sends no "is typing".
pub fn silent_typing() -> bool {
    enabled(Plugin::SilentTyping)
}

/// NoReplyMention: whether a new reply to `mid` pings its author (only listed users do).
pub fn reply_ping(state: &AppState, ch: Id, mid: Id) -> bool {
    if !enabled(Plugin::NoReplyMention) {
        return true;
    }
    let author = state.messages.get(&ch).and_then(|c| c.msgs.iter().find(|m| m.id == mid)).map(|m| m.author.id);
    author.is_some_and(|a| id_list(&opt_text(Plugin::NoReplyMention, "userList")).contains(&a))
}

/// Comma / space separated IDs (exact matches, unlike Vencord's substring test).
pub fn id_list(text: &str) -> HashSet<Id> {
    text.split([',', ' ', '\n']).filter_map(|s| s.trim().parse().ok()).map(Id).collect()
}

// ---------------------------------------------------------------------------
// Message list: ReplyTimestamp, NoBlockedMessages
// ---------------------------------------------------------------------------

/// ReplyTimestamp: the replied-to message's time after the reply preview.
pub fn reply_time(ui: &mut egui::Ui, rm: &Message, size: f32, color: Color32) {
    if !enabled(Plugin::ReplyTimestamp) {
        return;
    }
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(&rm.timestamp) else { return };
    let t = t.with_timezone(&chrono::Local);
    let text = if t.date_naive() == chrono::Local::now().date_naive() { t.format("%H:%M").to_string() } else { t.format("%d.%m.%Y").to_string() };
    ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).selectable(false)).on_hover_text(t.format("%A, %d %B %Y %H:%M").to_string());
}

/// NoBlockedMessages: messages of people I blocked (and, by option, ignored) are not shown.
pub fn hide_message(state: &AppState, m: &Message) -> bool {
    if !enabled(Plugin::NoBlockedMessages) {
        return false;
    }
    state.social.relationships.get(&m.author.id).is_some_and(|r| r.kind == relationship_type::BLOCKED || (r.user_ignored && opt_bool(Plugin::NoBlockedMessages, "ignored")))
}

// ---------------------------------------------------------------------------
// WhoReacted
// ---------------------------------------------------------------------------

/// Who reacted, per (message, emoji); fetched once when a reaction is hovered (as the official
/// client does for its tooltip), at most one request in flight.
#[derive(Default)]
struct Who {
    users: HashMap<(Id, String), Vec<User>>,
    order: VecDeque<(Id, String)>,
    /// The one request in flight, and when it went out.
    asked: Option<((Id, String), Instant)>,
}

const WHO_MAX: usize = 200;
const WHO_AVATARS: usize = 5;

static WHO: Mutex<Option<Who>> = Mutex::new(None);

fn who<R>(f: impl FnOnce(&mut Who) -> R) -> R {
    f(WHO.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Who::default))
}

fn emoji_key(e: &Emoji) -> String {
    e.id.map_or_else(|| e.name.clone().unwrap_or_default(), |id| id.0.to_string())
}

/// After a reaction chip (`messages.rs` reactions): the first reactors' avatars; hovering the
/// chip loads them.
pub fn who_reacted(ui: &mut egui::Ui, cx: &mut UiCx<'_>, ch: Id, mid: Id, emoji: &Emoji, chip: &egui::Response) {
    if !enabled(Plugin::WhoReacted) {
        return;
    }
    let key = (mid, emoji_key(emoji));
    let users = who(|w| w.users.get(&key).cloned());
    let Some(users) = users else {
        let busy = who(|w| w.asked.as_ref().is_some_and(|(_, t)| t.elapsed() < Duration::from_secs(10)));
        if chip.hovered() && !busy && cx.state.chat.reaction_users.is_none() {
            who(|w| w.asked = Some((key, Instant::now())));
            cx.state.chat.reaction_users = Some(crate::state::chat::ReactionUsersView { channel_id: ch, message_id: mid, emoji: emoji.clone(), loading: true, ..Default::default() });
            cx.cmd.send(Command::Chat(ChatCommand::FetchReactionUsers { channel_id: ch, message_id: mid, emoji: emoji.clone(), burst: false, after: None }));
        }
        return;
    };
    let blocked = |u: &User| cx.state.social.relationships.get(&u.id).is_some_and(|r| r.kind == relationship_type::BLOCKED);
    let shown: Vec<User> = users.into_iter().filter(|u| !blocked(u)).take(WHO_AVATARS).collect();
    if shown.is_empty() {
        return;
    }
    let side = 18.0;
    let (rect, _) = ui.allocate_exact_size(vec2(side + (shown.len() - 1) as f32 * (side - 6.0), chip.rect.height()), Sense::hover());
    let ring = ui.visuals().panel_fill;
    for (i, u) in shown.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + i as f32 * (side - 6.0), rect.center().y - side / 2.0), Vec2::splat(side));
        ui.painter().circle_filled(r.center(), side / 2.0 + 1.5, ring);
        let url = crate::ui::images::user_avatar_url(None, u, None, 32, false);
        crate::ui::avatar_at(ui, cx.images, &url, r, false, 1.0);
        let resp = ui.interact(r, ui.make_persistent_id(("rc_who", mid, i)), Sense::click()).on_hover_text(u.display_name());
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            cx.view.open_profile(u.id, cx.state.channels.get(&ch).and_then(|c| c.guild_id), r.right_top() + vec2(8.0, 0.0));
        }
    }
}

/// Every frame: a finished WhoReacted fetch moves into the cache (and frees the shared view).
pub fn sync_reactions(state: &mut AppState) {
    let asked = who(|w| w.asked.clone());
    let Some((key, _)) = asked else { return };
    let done = state.chat.reaction_users.as_ref().is_some_and(|v| !v.loading && (v.message_id, emoji_key(&v.emoji)) == key);
    if !done {
        return;
    }
    let Some(v) = state.chat.reaction_users.take() else { return };
    // A failure is cached empty: not asked again this session.
    let users: Vec<User> = v.users.into_iter().take(WHO_AVATARS * 2).collect();
    who(|w| {
        w.asked = None;
        w.order.push_back(key.clone());
        w.users.insert(key, users);
        while w.order.len() > WHO_MAX {
            if let Some(old) = w.order.pop_front() {
                w.users.remove(&old);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// FavoriteEmojiFirst
// ---------------------------------------------------------------------------

/// `:` suggestions with my most used emoji first (stable otherwise).
pub fn sort_emoji_rows(mut rows: Vec<crate::ui::autocomplete::Row>, frequent: &[(String, u32)]) -> Vec<crate::ui::autocomplete::Row> {
    if enabled(Plugin::FavoriteEmojiFirst) {
        let rank = |r: &crate::ui::autocomplete::Row| {
            let key = r.wire.clone().unwrap_or_else(|| r.insert.trim().to_owned());
            frequent.iter().position(|(k, _)| *k == key).unwrap_or(usize::MAX)
        };
        rows.sort_by_cached_key(rank);
    }
    rows
}

// ---------------------------------------------------------------------------
// TypingTweaks
// ---------------------------------------------------------------------------

/// Who types in `ch` (same filter and order as `AppState::typing_users`).
fn typists(state: &AppState, ch: Id) -> Vec<Id> {
    let now = Instant::now();
    let Some(t) = state.chat.typing.get(&ch) else { return Vec::new() };
    let mut live: Vec<(Instant, Id)> = t.iter().filter(|(_, t)| t.until > now).map(|(id, t)| (t.until, *id)).collect();
    live.sort();
    live.into_iter().map(|(_, id)| id).collect()
}

/// Avatars of the typists left-centred at `at` (composer typing line); returns their width.
pub fn typing_avatars(ui: &mut egui::Ui, cx: &mut UiCx<'_>, ch: Id, at: Pos2) -> f32 {
    if !enabled(Plugin::TypingTweaks) || !opt_bool(Plugin::TypingTweaks, "showAvatars") {
        return 0.0;
    }
    let ids = typists(cx.state, ch);
    let guild_id = cx.state.channels.get(&ch).and_then(|c| c.guild_id);
    let mut x = at.x;
    for id in ids.iter().take(3) {
        let Some(u) = cx.state.users.get(id).cloned() else { continue };
        let member = guild_id.and_then(|g| cx.state.find_member(g, *id)).cloned();
        let url = crate::ui::images::user_avatar_url(guild_id, &u, member.as_ref(), 32, false);
        crate::ui::avatar_at(ui, cx.images, &url, Rect::from_min_size(pos2(x, at.y - 8.0), Vec2::splat(16.0)), false, 1.0);
        x += 12.0;
    }
    if x > at.x { x - at.x + 8.0 } else { 0.0 }
}

/// Role colours on the typing names (sections of the typing line's job equal to a name).
pub fn tint_typing_names(state: &AppState, ch: Id, job: &mut egui::text::LayoutJob) {
    if !enabled(Plugin::TypingTweaks) || !opt_bool(Plugin::TypingTweaks, "showRoleColors") {
        return;
    }
    let Some(g) = state.channels.get(&ch).and_then(|c| c.guild_id) else { return };
    let names: Vec<(String, Color32)> = typists(state, ch)
        .into_iter()
        .filter_map(|id| {
            let u = state.users.get(&id)?;
            let m = state.find_member(g, id);
            let color = state.role_color(g, m.map_or(&[][..], |m| &m.roles))?;
            Some((state.member_display_name(Some(g), u, m), crate::ui::rgb(color)))
        })
        .collect();
    for s in &mut job.sections {
        if let Some((_, c)) = names.iter().find(|(n, _)| job.text.get(s.byte_range.start.0..s.byte_range.end.0) == Some(n.as_str())) {
            s.format.color = *c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_off_change_nothing() {
        // Test default: every plugin off.
        assert_eq!(outgoing("https://a.com/?utm_source=1".into()), "https://a.com/?utm_source=1");
        on_send("1");
        assert_eq!(send_flags("1"), 0);
        assert!(!silent_typing());
        assert!(reply_ping(&AppState::default(), Id(1), Id(2)));
        assert_eq!(id_list("1, 22 ,x,333"), [Id(1), Id(22), Id(333)].into_iter().collect());
    }

    #[test]
    fn silent_nonces_survive_a_retry() {
        SILENT_NONCES.lock().unwrap().push_back("n1".into());
        assert_eq!(send_flags("n1"), SUPPRESS_NOTIFICATIONS);
        // Retry: same nonce, still silent.
        assert_eq!(send_flags("n1"), SUPPRESS_NOTIFICATIONS);
        assert_eq!(send_flags("n2"), 0);
    }
}
