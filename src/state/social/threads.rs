//! Thread and forum rules on top of `state/social.rs` (FEATURES_NOTES §2.1, §2.2): permission
//! gates for the thread/forum UI (so it never sends a request that can only fail with 403),
//! the new-post check, and the local `message_count` of threads (the forum card reply count;
//! no THREAD_UPDATE carries it).
//!
//! OWNERSHIP: threads-forums engineer (docs/SOCIAL_DESIGN.md).

use crate::backend::{ArchivedThreads, ForumPost};
use crate::model::*;
use crate::state::{AppState, is_thread};

/// Forum / media flag: every post needs a tag.
const REQUIRE_TAG: u64 = 1 << 4;
/// THREAD_STARTER_MESSAGE: not part of `message_count`.
const MSG_THREAD_STARTER: u8 = 21;
const MAX_POST_TAGS: usize = 5;
const MAX_TITLE: usize = 100;

fn is_forum(kind: u8) -> bool {
    matches!(kind, channel_type::GUILD_FORUM | channel_type::GUILD_MEDIA)
}

impl AppState {
    /// "Create Thread" in a text / announcement channel, also from one of its messages:
    /// CREATE_PUBLIC_THREADS, or CREATE_PRIVATE_THREADS for a private one (text channels only).
    /// Forum posts: `can_create_post`.
    pub fn can_create_thread(&self, parent_id: Id, private: bool) -> bool {
        let bit = match (self.channels.get(&parent_id).map(|c| c.kind), private) {
            (Some(channel_type::GUILD_TEXT), true) => perm::CREATE_PRIVATE_THREADS,
            (Some(channel_type::GUILD_TEXT | channel_type::GUILD_ANNOUNCEMENT), false) => perm::CREATE_PUBLIC_THREADS,
            _ => return false,
        };
        self.has_perm(parent_id, bit)
    }

    /// "New Post" in a forum / media channel: SEND_MESSAGES there (CREATE_PUBLIC_THREADS is
    /// ignored for posts).
    pub fn can_create_post(&self, forum_id: Id) -> bool {
        self.channels.get(&forum_id).is_some_and(|c| is_forum(c.kind)) && self.has_perm(forum_id, perm::SEND_MESSAGES)
    }

    /// MANAGE_THREADS in a channel, or in a known thread's parent: lock, unarchive locked
    /// threads, archive others' threads, moderated tags, the private archive list.
    pub fn can_manage_threads(&self, channel_id: Id) -> bool {
        self.has_perm(channel_id, perm::MANAGE_THREADS)
    }

    /// "Join" / "Follow" for a thread from a list or its header (it may not be in `channels`):
    /// visible parent, not joined, not archived.
    pub fn can_join_thread(&self, thread: &Channel) -> bool {
        thread.member.is_none() && !thread.is_archived() && thread.parent_id.is_some_and(|p| self.can_view(p))
    }

    /// Archive: the creator or MANAGE_THREADS. Unarchive: anyone who may send in threads there,
    /// unless it is locked (then MANAGE_THREADS only).
    pub fn can_set_archived(&self, thread: &Channel, archived: bool) -> bool {
        let Some(parent) = thread.parent_id.filter(|p| self.can_view(*p)) else { return false };
        if self.can_manage_threads(parent) {
            return true;
        }
        if archived {
            thread.owner_id.is_some_and(|o| self.is_me(o))
        } else {
            !thread.thread_metadata.as_ref().is_some_and(|m| m.locked) && self.has_perm(parent, perm::SEND_MESSAGES_IN_THREADS)
        }
    }

    /// Archived thread lists need READ_MESSAGE_HISTORY; all private ones also MANAGE_THREADS.
    pub fn can_list_archived(&self, channel_id: Id, kind: ArchivedThreads) -> bool {
        self.has_perm(channel_id, perm::READ_MESSAGE_HISTORY)
            && (kind != ArchivedThreads::Private || self.can_manage_threads(channel_id))
    }

    /// Tags the New Post dialog offers: moderated ones only with MANAGE_THREADS.
    pub fn post_tags(&self, forum_id: Id) -> Vec<&ForumTag> {
        let manage = self.can_manage_threads(forum_id);
        let tags = self.channels.get(&forum_id).map_or(&[][..], |c| &c.available_tags);
        tags.iter().filter(|t| manage || !t.moderated).collect()
    }

    /// Checks a new post before `CreateForumPost`, so the dialog can say what is wrong instead
    /// of sending a request the server refuses: permission, title 1-100 chars, ≤ 5 offered tags,
    /// one when the forum requires it, text or files. Err = text for the dialog.
    pub fn check_forum_post(&self, forum_id: Id, post: &ForumPost) -> Result<(), String> {
        let Some(forum) = self.channels.get(&forum_id).filter(|_| self.can_create_post(forum_id)) else {
            return Err("You can't post in this channel.".into());
        };
        let title = post.name.trim().chars().count();
        if title == 0 || title > MAX_TITLE {
            return Err(format!("The title must be 1 to {MAX_TITLE} characters long."));
        }
        if post.applied_tags.len() > MAX_POST_TAGS {
            return Err(format!("A post can have at most {MAX_POST_TAGS} tags."));
        }
        let offered = self.post_tags(forum_id);
        if !post.applied_tags.iter().all(|id| offered.iter().any(|t| t.id == *id)) {
            return Err("One of the tags can't be used here. Pick the tags again.".into());
        }
        if post.applied_tags.is_empty() && forum.flags & REQUIRE_TAG != 0 {
            return Err("This channel needs at least one tag on every post.".into());
        }
        if post.content.trim().is_empty() && post.files.is_empty() {
            return Err("Write something or attach a file.".into());
        }
        Ok(())
    }

    /// Called by `apply_gateway` before it handles an event: keeps a known thread's
    /// `message_count` current on MESSAGE_CREATE / _DELETE / _DELETE_BULK. The starter message
    /// (type 21, or a forum post's first message: id = thread id) never counts.
    pub(in crate::state) fn count_thread_messages(&mut self, ev: &GatewayEvent) {
        let (thread_id, delta) = match ev {
            GatewayEvent::MessageCreate(m) if m.kind != MSG_THREAD_STARTER && m.id != m.channel_id => (m.channel_id, 1),
            GatewayEvent::MessageDelete { id, channel_id } if id != channel_id => (*channel_id, -1),
            GatewayEvent::MessageDeleteBulk { ids, channel_id } => (*channel_id, -(ids.iter().filter(|i| *i != channel_id).count() as i64)),
            _ => return,
        };
        let count = self.channels.get_mut(&thread_id).filter(|c| is_thread(c.kind)).and_then(|c| c.message_count.as_mut());
        if let Some(n) = count {
            *n = (i64::from(*n) + delta).max(0) as u32;
        }
    }

    fn has_perm(&self, channel_id: Id, bit: u64) -> bool {
        let p = self.channel_permissions(channel_id);
        p & perm::VIEW_CHANNEL != 0 && p & bit != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{UploadFile, UploadSource};

    const ME: Id = Id(1);
    const G: Id = Id(10);
    const TEXT: Id = Id(100);
    const NEWS: Id = Id(102);
    const FORUM: Id = Id(101);
    const MOD_ROLE: Id = Id(11);
    /// What @everyone may do in these tests.
    const MEMBER: u64 = perm::VIEW_CHANNEL | perm::SEND_MESSAGES | perm::READ_MESSAGE_HISTORY | perm::CREATE_PUBLIC_THREADS;

    fn state(mod_: bool) -> AppState {
        let mut s = AppState { me: Some(User { id: ME, ..Default::default() }), ..Default::default() };
        let roles = vec![
            Role { id: G, permissions: Perms(MEMBER), ..Default::default() },
            Role { id: MOD_ROLE, permissions: Perms(perm::MANAGE_THREADS | perm::CREATE_PRIVATE_THREADS), ..Default::default() },
        ];
        s.guilds.push(Guild { id: G, owner_id: Id(99), roles, ..Default::default() });
        s.my_members.insert(G, Member { roles: if mod_ { vec![MOD_ROLE] } else { vec![] }, ..Default::default() });
        let tags = vec![
            ForumTag { id: Id(7), name: "bug".into(), ..Default::default() },
            ForumTag { id: Id(8), name: "staff".into(), moderated: true, ..Default::default() },
        ];
        for c in [
            Channel { id: TEXT, guild_id: Some(G), ..Default::default() },
            Channel { id: NEWS, kind: channel_type::GUILD_ANNOUNCEMENT, guild_id: Some(G), ..Default::default() },
            Channel { id: FORUM, kind: channel_type::GUILD_FORUM, guild_id: Some(G), available_tags: tags, flags: REQUIRE_TAG, ..Default::default() },
        ] {
            s.channels.insert(c.id, c);
        }
        s
    }

    fn thread(id: u64, owner: Id, locked: bool) -> Channel {
        Channel {
            id: Id(id),
            kind: channel_type::PUBLIC_THREAD,
            guild_id: Some(G),
            parent_id: Some(TEXT),
            owner_id: Some(owner),
            message_count: Some(2),
            thread_metadata: Some(ThreadMetadata { locked, ..Default::default() }),
            ..Default::default()
        }
    }

    fn post(name: &str, tags: &[u64], content: &str) -> ForumPost {
        ForumPost { name: name.into(), content: content.into(), applied_tags: tags.iter().map(|&t| Id(t)).collect(), ..Default::default() }
    }

    #[test]
    fn create_and_manage_gates() {
        let (member, moderator) = (state(false), state(true));
        assert!(member.can_create_thread(TEXT, false) && member.can_create_thread(NEWS, false));
        assert!(!member.can_create_thread(TEXT, true), "needs CREATE_PRIVATE_THREADS");
        assert!(moderator.can_create_thread(TEXT, true));
        assert!(!moderator.can_create_thread(NEWS, true), "private threads only under text channels");
        assert!(!member.can_create_thread(FORUM, false), "forums take posts");
        assert!(!member.can_create_thread(Id(404), false));
        assert!(member.can_create_post(FORUM) && !member.can_create_post(TEXT));
        assert!(!member.can_manage_threads(TEXT) && moderator.can_manage_threads(TEXT));
        assert!(member.can_list_archived(TEXT, ArchivedThreads::Public) && member.can_list_archived(TEXT, ArchivedThreads::Joined));
        assert!(!member.can_list_archived(TEXT, ArchivedThreads::Private) && moderator.can_list_archived(TEXT, ArchivedThreads::Private));

        // A channel I can't see allows nothing, whatever the role says.
        let mut hidden = state(true);
        hidden.channels.get_mut(&TEXT).unwrap().permission_overwrites =
            vec![Overwrite { id: G, kind: 0, allow: Perms(0), deny: Perms(perm::VIEW_CHANNEL) }];
        assert!(!hidden.can_create_thread(TEXT, false) && !hidden.can_manage_threads(TEXT));
        assert!(!hidden.can_join_thread(&thread(300, Id(2), false)));
    }

    #[test]
    fn join_and_archive_gates() {
        let mut s = state(false);
        let theirs = thread(300, Id(2), false);
        assert!(s.can_join_thread(&theirs));
        assert!(!s.can_join_thread(&Channel { member: Some(ThreadMember::default()), ..theirs.clone() }), "already joined");
        let mut archived = theirs.clone();
        archived.thread_metadata.as_mut().unwrap().archived = true;
        assert!(!s.can_join_thread(&archived));

        // Unarchiving needs SEND_MESSAGES_IN_THREADS (not in MEMBER) and no lock.
        assert!(!s.can_set_archived(&theirs, false));
        s.guilds[0].roles[0].permissions = Perms(MEMBER | perm::SEND_MESSAGES_IN_THREADS);
        assert!(s.can_set_archived(&theirs, false));
        assert!(!s.can_set_archived(&thread(301, Id(2), true), false), "locked");
        assert!(!s.can_set_archived(&theirs, true), "only the creator archives");
        assert!(s.can_set_archived(&thread(302, ME, false), true));
        let m = state(true);
        assert!(m.can_set_archived(&thread(301, Id(2), true), false) && m.can_set_archived(&theirs, true));
    }

    #[test]
    fn forum_post_checks() {
        let (member, moderator) = (state(false), state(true));
        assert_eq!(member.post_tags(FORUM).iter().map(|t| t.id).collect::<Vec<_>>(), [Id(7)]);
        assert_eq!(moderator.post_tags(FORUM).len(), 2);
        assert_eq!(member.check_forum_post(FORUM, &post(" Crash on start ", &[7], "steps…")), Ok(()));
        assert!(member.check_forum_post(FORUM, &post("x", &[], "y")).unwrap_err().contains("at least one tag"));
        assert!(member.check_forum_post(FORUM, &post("x", &[8], "y")).unwrap_err().contains("can't be used"));
        assert_eq!(moderator.check_forum_post(FORUM, &post("x", &[8], "y")), Ok(()));
        assert!(member.check_forum_post(FORUM, &post("x", &[404], "y")).is_err(), "unknown tag");
        assert!(moderator.check_forum_post(FORUM, &post("x", &[7, 7, 7, 7, 7, 8], "y")).unwrap_err().contains("at most 5"));
        assert!(member.check_forum_post(FORUM, &post("  ", &[7], "y")).unwrap_err().contains("title"));
        assert!(member.check_forum_post(FORUM, &post(&"a".repeat(101), &[7], "y")).is_err());
        assert!(member.check_forum_post(FORUM, &post("x", &[7], " ")).unwrap_err().contains("Write something"));
        let file = UploadFile { source: UploadSource::Bytes(std::sync::Arc::from(&b"a"[..])), filename: "a.png".into(), spoiler: false, description: None };
        assert_eq!(member.check_forum_post(FORUM, &ForumPost { files: vec![file], ..post("x", &[7], "") }), Ok(()));
        assert!(member.check_forum_post(TEXT, &post("x", &[], "y")).unwrap_err().contains("can't post"));
    }

    #[test]
    fn thread_message_counts() {
        let mut s = state(true);
        s.channels.insert(Id(300), thread(300, ME, false));
        let msg = |id: u64, channel: u64, kind: u8| Message { id: Id(id), channel_id: Id(channel), kind, ..Default::default() };
        for ev in [
            GatewayEvent::MessageCreate(Box::new(msg(301, 300, 0))),
            GatewayEvent::MessageCreate(Box::new(msg(302, 300, 19))),
            GatewayEvent::MessageCreate(Box::new(msg(300, 300, 0))), // forum starter
            GatewayEvent::MessageCreate(Box::new(msg(303, 300, MSG_THREAD_STARTER))),
            GatewayEvent::MessageCreate(Box::new(msg(304, 100, 0))), // the parent channel
        ] {
            s.apply_gateway(ev, None);
        }
        assert_eq!(s.channels[&Id(300)].message_count, Some(4));
        assert_eq!(s.channels[&TEXT].message_count, None);
        s.apply_gateway(GatewayEvent::MessageDelete { id: Id(301), channel_id: Id(300) }, None);
        s.apply_gateway(GatewayEvent::MessageDelete { id: Id(300), channel_id: Id(300) }, None);
        assert_eq!(s.channels[&Id(300)].message_count, Some(3));
        s.apply_gateway(GatewayEvent::MessageDeleteBulk { ids: vec![Id(300), Id(302), Id(5), Id(6), Id(7)], channel_id: Id(300) }, None);
        assert_eq!(s.channels[&Id(300)].message_count, Some(0), "never below zero");
    }

    #[test]
    fn joined_threads_light_up_the_guild_unless_muted() {
        let mut s = state(true);
        let mut joined = Channel { last_message_id: Some(Id(500)), member: Some(ThreadMember::default()), ..thread(300, ME, false) };
        let unjoined = Channel { last_message_id: Some(Id(600)), ..thread(301, ME, false) };
        s.channels.insert(Id(301), unjoined);
        s.read_states.insert(Id(301), ReadState { id: Id(301), last_message_id: Some(Id(1)), mention_count: 2, ..Default::default() });
        assert_eq!(s.guild_unread(G), (false, 0), "unjoined threads have no badge");
        s.channels.insert(Id(300), joined.clone());
        s.read_states.insert(Id(300), ReadState { id: Id(300), last_message_id: Some(Id(1)), mention_count: 1, ..Default::default() });
        assert_eq!(s.guild_unread(G), (true, 1));
        // Muted on its thread member: mentions still count, plain unread does not.
        joined.member.as_mut().unwrap().muted = true;
        s.channels.insert(Id(300), joined);
        assert!(s.is_channel_muted(Id(300)));
        assert_eq!(s.guild_unread(G), (false, 1), "mentions still count");
        s.read_states.get_mut(&Id(300)).unwrap().mention_count = 0;
        assert_eq!(s.guild_unread(G), (false, 0));
    }
}
