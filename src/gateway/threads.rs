//! Thread dispatches -> `GatewayEvent`s (FEATURES_NOTES §2.1). READY / GUILD_CREATE
//! `threads[]` are appended to `Guild::channels` in `normalize::GuildWire::into_guild`.
//!
//! OWNERSHIP: threads-forums engineer (docs/SOCIAL_DESIGN.md).

use super::normalize::data;
use crate::model::*;
use serde::Deserialize;

/// The thread dispatches `dispatch` handles.
pub(super) const EVENTS: [&str; 6] =
    ["THREAD_CREATE", "THREAD_UPDATE", "THREAD_DELETE", "THREAD_LIST_SYNC", "THREAD_MEMBER_UPDATE", "THREAD_MEMBERS_UPDATE"];

/// THREAD_DELETE `{id, guild_id, parent_id, type}`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct DeleteWire {
    id: Id,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListSyncWire {
    guild_id: Id,
    /// Absent = the guild's whole set.
    channel_ids: Option<Vec<Id>>,
    #[serde(deserialize_with = "null_default")]
    threads: Vec<Channel>,
    /// Mine, one per joined thread (`id` = thread).
    #[serde(deserialize_with = "null_default")]
    members: Vec<ThreadMember>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MembersUpdateWire {
    id: Id,
    #[serde(deserialize_with = "null_default")]
    member_count: u32,
    #[serde(deserialize_with = "null_default")]
    added_members: Vec<ThreadMember>,
    #[serde(deserialize_with = "null_default")]
    removed_member_ids: Vec<Id>,
}

/// One thread dispatch (`t` in `EVENTS`); `me` = my user id (0 before READY).
pub(super) fn dispatch(t: &str, b: &[u8], me: Id) -> serde_json::Result<Option<GatewayEvent>> {
    use GatewayEvent as E;
    Ok(Some(match t {
        // THREAD_CREATE also fires when I am added to a thread (with `member`); THREAD_UPDATE
        // carries no `member` (the state keeps mine).
        "THREAD_CREATE" | "THREAD_UPDATE" => E::ThreadUpsert(data(b)?),
        "THREAD_DELETE" => E::ThreadDelete { id: data::<DeleteWire>(b)?.id },
        "THREAD_LIST_SYNC" => {
            let w: ListSyncWire = data(b)?;
            let mut members = w.members;
            let threads = (w.threads.into_iter())
                .map(|mut t| {
                    t.guild_id = Some(w.guild_id);
                    if let Some(i) = members.iter().position(|m| m.id == Some(t.id)) {
                        t.member = Some(members.swap_remove(i));
                    }
                    t
                })
                .collect();
            E::ThreadListSync { guild_id: w.guild_id, channel_ids: w.channel_ids, threads }
        }
        // Always mine ("you are a member"); without a thread id there is nothing to update.
        "THREAD_MEMBER_UPDATE" => {
            let member: ThreadMember = data(b)?;
            let Some(thread_id) = member.id else { return Ok(None) };
            E::ThreadMemberUpdate { thread_id, member }
        }
        "THREAD_MEMBERS_UPDATE" => {
            let w: MembersUpdateWire = data(b)?;
            let mine = |id: Option<Id>| !me.is_zero() && id == Some(me);
            let joined = w.added_members.into_iter().find(|m| mine(m.user_id));
            let left = w.removed_member_ids.into_iter().any(|id| mine(Some(id)));
            E::ThreadMembersUpdate { thread_id: w.id, member_count: w.member_count, joined, left }
        }
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str, d: &str) -> Option<GatewayEvent> {
        assert!(EVENTS.contains(&t));
        dispatch(t, format!(r#"{{"op":0,"t":"{t}","d":{d}}}"#).as_bytes(), Id(1)).unwrap()
    }

    #[test]
    fn create_update_delete() {
        let create = r#"{"id":"300","guild_id":"10","parent_id":"100","owner_id":"1","type":11,"name":"plans",
            "member_count":1,"message_count":0,"newly_created":true,"rate_limit_per_user":0,"flags":0,
            "thread_metadata":{"archived":false,"auto_archive_duration":4320,"archive_timestamp":"2026-11-12T08:00:00+00:00","locked":false,"create_timestamp":"2026-11-12T08:00:00+00:00"},
            "member":{"id":"300","user_id":"1","join_timestamp":"2026-11-12T08:00:00+00:00","flags":1,"muted":false,"mute_config":null}}"#;
        let Some(GatewayEvent::ThreadUpsert(t)) = ev("THREAD_CREATE", create) else { panic!() };
        assert_eq!((t.id, t.guild_id, t.parent_id, t.kind), (Id(300), Some(Id(10)), Some(Id(100)), channel_type::PUBLIC_THREAD));
        assert_eq!(t.member.as_ref().and_then(|m| m.user_id), Some(Id(1)));
        assert_eq!(t.thread_metadata.as_ref().map(|m| m.auto_archive_duration), Some(4320));

        let update = r#"{"id":"300","guild_id":"10","parent_id":"100","type":11,"name":"plans",
            "thread_metadata":{"archived":true,"auto_archive_duration":60,"archive_timestamp":"2026-11-12T09:00:00+00:00","locked":true}}"#;
        let Some(GatewayEvent::ThreadUpsert(t)) = ev("THREAD_UPDATE", update) else { panic!() };
        assert!(t.is_archived() && t.member.is_none());

        let r = ev("THREAD_DELETE", r#"{"id":"300","guild_id":"10","parent_id":"100","type":11}"#);
        assert!(matches!(r, Some(GatewayEvent::ThreadDelete { id: Id(300) })));
    }

    #[test]
    fn list_sync_attaches_my_members() {
        let d = r#"{"guild_id":"10","channel_ids":["100"],
            "threads":[{"id":"300","parent_id":"100","type":11,"name":"a","thread_metadata":{"archived":false}},
                       {"id":"301","parent_id":"100","type":11,"name":"b","thread_metadata":{"archived":false}}],
            "members":[{"id":"301","user_id":"1","join_timestamp":"2026-11-12T08:00:00+00:00","flags":2}],
            "most_recent_messages":[]}"#;
        let Some(GatewayEvent::ThreadListSync { guild_id, channel_ids, threads }) = ev("THREAD_LIST_SYNC", d) else { panic!() };
        assert_eq!((guild_id, channel_ids), (Id(10), Some(vec![Id(100)])));
        assert!(threads.iter().all(|t| t.guild_id == Some(Id(10))));
        assert!(threads[0].member.is_none());
        assert_eq!(threads[1].member.as_ref().map(|m| m.flags), Some(2));
        // Whole-guild sync: no channel_ids, null lists.
        let r = ev("THREAD_LIST_SYNC", r#"{"guild_id":"10","threads":null,"members":null}"#);
        assert!(matches!(r, Some(GatewayEvent::ThreadListSync { channel_ids: None, threads, .. }) if threads.is_empty()));
    }

    #[test]
    fn member_updates_only_track_me() {
        let r = ev("THREAD_MEMBER_UPDATE", r#"{"id":"300","user_id":"1","guild_id":"10","join_timestamp":"2026-11-12T08:00:00+00:00","flags":4,"muted":true,"mute_config":null}"#);
        assert!(matches!(r, Some(GatewayEvent::ThreadMemberUpdate { thread_id: Id(300), member }) if member.flags == 4 && member.muted));
        assert!(ev("THREAD_MEMBER_UPDATE", r#"{"user_id":"1","flags":4}"#).is_none(), "no thread id");

        let joined = r#"{"id":"300","guild_id":"10","member_count":3,
            "added_members":[{"id":"300","user_id":"2","join_timestamp":"x","flags":0,"member":{"user":{"id":"2"},"roles":[]},"presence":null},
                             {"id":"300","user_id":"1","join_timestamp":"y","flags":1,"member":{"user":{"id":"1"},"roles":[]},"presence":null}]}"#;
        let Some(GatewayEvent::ThreadMembersUpdate { thread_id, member_count, joined, left }) = ev("THREAD_MEMBERS_UPDATE", joined) else { panic!() };
        assert_eq!((thread_id, member_count, left), (Id(300), 3, false));
        assert_eq!(joined.and_then(|m| m.join_timestamp).as_deref(), Some("y"));

        let r = ev("THREAD_MEMBERS_UPDATE", r#"{"id":"300","guild_id":"10","member_count":2,"removed_member_ids":["1"]}"#);
        assert!(matches!(r, Some(GatewayEvent::ThreadMembersUpdate { joined: None, left: true, member_count: 2, .. })));
        let r = ev("THREAD_MEMBERS_UPDATE", r#"{"id":"300","guild_id":"10","member_count":1,"removed_member_ids":["2"]}"#);
        assert!(matches!(r, Some(GatewayEvent::ThreadMembersUpdate { joined: None, left: false, .. })), "someone else left");
        // Before READY my id is unknown: never "me".
        let r = dispatch("THREAD_MEMBERS_UPDATE", br#"{"op":0,"d":{"id":"300","member_count":0,"removed_member_ids":["0"]}}"#, Id(0)).unwrap();
        assert!(matches!(r, Some(GatewayEvent::ThreadMembersUpdate { left: false, .. })));
    }

    #[test]
    fn ready_and_guild_create_threads_join_the_channels() {
        let ready = br#"{"op":0,"t":"READY","d":{"user":{"id":"1","username":"me"},"session_id":"s",
            "guilds":[{"id":"10","properties":{"id":"10","name":"G","owner_id":"2"},
              "channels":[{"id":"100","type":0,"name":"general"}],
              "threads":[{"id":"300","parent_id":"100","type":11,"name":"t","owner_id":"1",
                          "thread_metadata":{"archived":false,"auto_archive_duration":1440},
                          "member":{"join_timestamp":"2026-11-12T08:00:00+00:00","flags":1}}]}],
            "merged_members":[[{"user_id":"1","roles":[]}]]}}"#;
        let r = super::super::normalize::ready(ready).unwrap();
        let ch = &r.data.guilds[0].channels;
        assert_eq!(ch.iter().map(|c| c.id).collect::<Vec<_>>(), [Id(100), Id(300)]);
        assert!(ch.iter().all(|c| c.guild_id == Some(Id(10))));
        assert!(ch[1].member.is_some(), "READY threads are joined");

        let gc = br#"{"op":0,"t":"GUILD_CREATE","d":{"id":"10","name":"G","channels":[],"threads":[{"id":"301","parent_id":"100","type":12}]}}"#;
        let r = super::super::normalize::dispatch("GUILD_CREATE", gc, Id(1)).unwrap();
        assert!(matches!(&r[..], [GatewayEvent::GuildCreate(g)] if g.channels.len() == 1 && g.channels[0].guild_id == Some(Id(10))));
        // The thread events go through `normalize::dispatch` too.
        let r = super::super::normalize::dispatch("THREAD_DELETE", br#"{"op":0,"d":{"id":"301"}}"#, Id(1)).unwrap();
        assert!(matches!(&r[..], [GatewayEvent::ThreadDelete { id: Id(301) }]));
    }
}
