//! Memory diagnostics for measurements (no effect unless the env hooks are set):
//! - `AppState::mem_estimate`: capacity-based heap estimates of the big state structures
//!   (`RUSTCORD_MEMSTATS=<file>` writes them, see `ui::images::mem_hooks`).
//! - `AppState::apply_memtest`: `RUSTCORD_MEMTEST=presences:<n>,members:<n>` fills presences and
//!   the open guild's member list, which the offline snapshot cannot carry.

use super::{AppState, ChannelMessages, MemberList};
use crate::model::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::size_of;

/// Heap bytes a value owns (beyond its inline size).
trait Heap {
    fn heap(&self) -> usize;
}

macro_rules! leaf {
    ($($t:ty),*) => { $(impl Heap for $t { fn heap(&self) -> usize { 0 } })* };
}
leaf!(Id, u8, u32, u64, bool, Perms, Presence, Overwrite, ReadState, MessageReference);

/// Structs: the sum of the listed fields' heap bytes.
macro_rules! fields {
    ($($t:ty { $($f:ident),* })*) => { $(impl Heap for $t { fn heap(&self) -> usize { 0 $(+ self.$f.heap())* } })* };
}
fields! {
    User { username, global_name, discriminator, avatar, banner }
    Member { user, nick, avatar, roles, communication_disabled_until }
    Role { name }
    CustomEmoji { name }
    Emoji { name }
    MuteConfig { end_time }
    ThreadMetadata { archive_timestamp, create_timestamp }
    ThreadMember { join_timestamp, mute_config }
    ForumTag { name, emoji_name }
    DefaultReaction { emoji_name }
    Channel { name, topic, permission_overwrites, recipient_ids, recipients, icon, last_pin_timestamp,
              thread_metadata, member, applied_tags, available_tags, default_reaction_emoji }
    VoiceState { member, session_id }
    Guild { name, icon, roles, emojis, channels, voice_states }
    Attachment { filename, url, proxy_url, content_type }
    EmbedMedia { url, proxy_url }
    EmbedAuthor { name, url, icon_url, proxy_icon_url }
    EmbedFooter { text, icon_url, proxy_icon_url }
    EmbedField { name, value }
    EmbedProvider { name, url }
    Embed { kind, title, description, url, timestamp, author, footer, fields, thumbnail, image, video, provider }
    Reaction { emoji }
    StickerItem { name }
    Message { author, member, content, timestamp, edited_timestamp, mentions, mention_roles, attachments,
              embeds, reactions, referenced_message, message_reference, sticker_items, nonce }
    ChannelOverride { mute_config }
    GuildSettings { mute_config, channel_overrides }
    CustomStatus { text, emoji }
    Activity { name, rich }
    RichPresence { details, state, large_image, large_text, small_image }
    UserPresence { custom_status, activity }
    Relationship { user, nickname, since }
    ChannelMessages { msgs, error }
    MemberList { list_id, items }
}

impl Heap for String {
    fn heap(&self) -> usize {
        self.capacity()
    }
}

impl Heap for serde_json::Value {
    fn heap(&self) -> usize {
        self.as_str().map_or(0, str::len)
    }
}

impl Heap for MemberListItem {
    fn heap(&self) -> usize {
        match self {
            MemberListItem::Group { id, .. } => id.heap(),
            MemberListItem::Member { member, .. } => member.heap(),
        }
    }
}

impl<T: Heap> Heap for Option<T> {
    fn heap(&self) -> usize {
        self.as_ref().map_or(0, Heap::heap)
    }
}

impl<T: Heap> Heap for Box<T> {
    fn heap(&self) -> usize {
        size_of::<T>() + (**self).heap()
    }
}

impl<T: Heap> Heap for Vec<T> {
    fn heap(&self) -> usize {
        self.capacity() * size_of::<T>() + self.iter().map(Heap::heap).sum::<usize>()
    }
}

impl<T: Heap> Heap for VecDeque<T> {
    fn heap(&self) -> usize {
        self.capacity() * size_of::<T>() + self.iter().map(Heap::heap).sum::<usize>()
    }
}

impl<K: Heap, V: Heap> Heap for HashMap<K, V> {
    fn heap(&self) -> usize {
        // hashbrown: one control byte per bucket.
        self.capacity() * (size_of::<(K, V)>() + 1) + self.iter().map(|(k, v)| k.heap() + v.heap()).sum::<usize>()
    }
}

impl<K: Heap> Heap for HashSet<K> {
    fn heap(&self) -> usize {
        self.capacity() * (size_of::<K>() + 1)
    }
}

impl AppState {
    /// (name, entries, approximate heap bytes) of the big structures.
    pub fn mem_estimate(&self) -> Vec<(&'static str, usize, usize)> {
        let msgs: usize = self.messages.values().map(|m| m.msgs.len()).sum();
        let list_items: usize = self.member_lists.values().map(|l| l.items.len()).sum();
        let voice: usize = self.voice_states.values().map(HashMap::len).sum();
        vec![
            ("users", self.users.len(), self.users.heap()),
            ("guilds (roles, emojis)", self.guilds.len(), self.guilds.heap()),
            ("channels", self.channels.len(), self.channels.heap()),
            ("read_states", self.read_states.len(), self.read_states.heap()),
            ("guild_settings", self.guild_settings.len(), self.guild_settings.heap()),
            ("my_members", self.my_members.len(), self.my_members.heap()),
            ("messages", msgs, self.messages.heap()),
            ("member_lists", list_items, self.member_lists.heap()),
            ("presences", self.chat.presences.len(), self.chat.presences.heap()),
            ("relationships", self.social.relationships.len(), self.social.relationships.heap()),
            ("voice_states", voice, self.voice_states.heap()),
            ("misc id sets", self.dm_order.len() + self.fresh_channels.len() + self.hidden_authors.len(),
             self.dm_order.heap() + self.fresh_channels.heap() + self.hidden_authors.heap() + self.stale_upto.heap()),
        ]
    }

    /// `RUSTCORD_MEMTEST=presences:<n>,members:<n>` (offline measurements): `n` cached users get a
    /// presence (every 4th idle/dnd, every 5th a custom status, every 7th a game), and the open
    /// guild a member list of `n` members in an online and an offline group, as op 37 would fill it.
    pub fn apply_memtest(&mut self, spec: &str) {
        let mut users: Vec<User> = self.users.values().cloned().collect();
        users.sort_by_key(|u| u.id);
        for part in spec.split(',') {
            let Some((what, n)) = part.trim().split_once(':') else { continue };
            let n = n.parse::<usize>().unwrap_or(0).min(users.len());
            match what {
                "presences" => {
                    let list = users[..n]
                        .iter()
                        .enumerate()
                        .map(|(i, u)| {
                            let status = [Presence::Online, Presence::Online, Presence::Idle, Presence::Dnd][i % 4];
                            let custom_status = (i % 5 == 0).then(|| CustomStatus { text: Some(format!("status text {i}")), emoji: None });
                            let activity = (i % 7 == 0).then(|| Activity { kind: 0, name: format!("Game {i}"), rich: None });
                            (u.id, UserPresence { status, mobile: i % 9 == 0, custom_status, activity, ..Default::default() })
                        })
                        .collect();
                    self.on_presences(list);
                }
                "members" if let Some(g) = self.selected_guild => {
                    let online = n * 2 / 3;
                    let mut items = Vec::with_capacity(n + 2);
                    for (i, u) in users[..n].iter().enumerate() {
                        if i == 0 || i == online {
                            let (id, count) = if i == 0 { ("online", online) } else { ("offline", n - online) };
                            items.push(MemberListItem::Group { id: id.into(), count: count as u32 });
                        }
                        let status = if i < online { Presence::Online } else { Presence::Offline };
                        let member = Member { user: Some(u.clone()), roles: vec![g], ..Default::default() };
                        items.push(MemberListItem::Member { member, status });
                    }
                    let list = MemberList {
                        list_id: "everyone".into(),
                        channel_id: self.selected_channel,
                        member_count: n as u32,
                        online_count: online as u32,
                        items,
                    };
                    self.member_lists.insert(g, list);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_count_capacity_and_nested_heap() {
        let mut s = String::with_capacity(10);
        s.push('x');
        assert_eq!(s.heap(), 10);
        let v: Vec<String> = vec![String::with_capacity(5), String::with_capacity(7)];
        assert_eq!(v.heap(), 2 * size_of::<String>() + 12);
        let m = Message { content: "hello".into(), referenced_message: Some(Box::new(Message::default())), ..Default::default() };
        assert_eq!(m.heap(), 5 + size_of::<Message>());
    }

    #[test]
    fn memtest_fills_presences_and_member_list() {
        let mut s = AppState::default();
        for i in 1..=10 {
            s.users.insert(Id(i), User { id: Id(i), ..Default::default() });
        }
        s.selected_guild = Some(Id(99));
        s.apply_memtest("presences:6,members:9");
        assert_eq!(s.chat.presences.len(), 6);
        let list = &s.member_lists[&Id(99)];
        assert_eq!(list.items.len(), 9 + 2);
        assert!(matches!(&list.items[0], MemberListItem::Group { id, count: 6 } if id == "online"));
        assert!(s.mem_estimate().iter().any(|(name, n, bytes)| *name == "member_lists" && *n == 11 && *bytes > 0));
    }
}
