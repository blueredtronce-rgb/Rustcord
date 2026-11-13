//! Local permission gate for server-admin commands (FEATURES_NOTES §3.3-§3.8): the UI disables
//! or refuses an action this rejects instead of earning a 403 (they count toward the
//! Cloudflare budget). Mirrors Discord's rules from the cached guild, roles, overwrites and
//! member objects; the server stays the authority.
//!
//! OWNERSHIP: guild-admin engineer (docs/ACCOUNT_SERVER_DESIGN.md).

use super::super::AppState;
use crate::backend::AccountCommand;
use crate::model::{Id, Overwrite, perm};

const MANAGE_GUILD: &str = "You need the Manage Server permission.";
const MANAGE_CHANNELS: &str = "You need the Manage Channels permission.";
const MANAGE_ROLE: &str = "You need the Manage Roles permission, and the role must be below your highest role.";
const GRANT: &str = "You can only grant permissions you have yourself.";
const EVERYONE: &str = "The @everyone role can't be deleted, moved or assigned.";
const KICK: &str = "You need the Kick Members permission and a higher role than this member.";
const BAN: &str = "You need the Ban Members permission and a higher role than this member.";
const BANS: &str = "You need the Ban Members permission.";
const NICK: &str = "You need the Manage Nicknames permission and a higher role than this member.";
const MY_NICK: &str = "You need the Change Nickname permission.";
const EXPRESSIONS: &str = "You need the Create Expressions or Manage Expressions permission.";
const MANAGE_EXPRESSIONS: &str = "You need the Manage Expressions permission.";
const INVITE: &str = "You need the Create Invite permission in this channel.";
const OWNER_LEAVE: &str = "You own this server. Transfer it or delete it in the official app before leaving.";

impl AppState {
    /// May I send this account/admin command? `Err` = why not (user-facing; show it on the
    /// disabled control or as a toast). Commands that need no permission (my settings and
    /// profile, notifications, creating a server, resolving / accepting / deleting invites)
    /// pass. Guild `mfa_level` 1 without 2FA drops the elevated bits (`guild_permissions`).
    /// Not modelled: managed (bot) roles, emoji uploaders.
    pub fn account_check(&self, cmd: &AccountCommand) -> Result<(), &'static str> {
        use AccountCommand as C;
        let guild = |g: Id| self.guild_permissions(g);
        let guild_of = |c: Id| self.channels.get(&c).and_then(|c| c.guild_id).and_then(|g| self.guild(g));
        let channel = |c: Id| self.channel_permissions(c) & guild_of(c).map_or(perm::ALL, |g| self.mfa_mask(g));
        let roles = |g: Id, u: Id| self.find_member(g, u).map_or(&[][..], |m| m.roles.as_slice());
        // Moderating another member: the permission bit and a higher top role.
        let moderate = |g: Id, u: Id, bit: u64, why| check(guild(g) & bit != 0 && self.outranks(g, u, roles(g, u)), why);
        match cmd {
            C::LeaveGuild { guild_id } => {
                check(self.guild(*guild_id).is_none_or(|g| self.me.as_ref().is_none_or(|me| me.id != g.owner_id)), OWNER_LEAVE)
            }
            C::UpdateGuild { guild_id, .. } => need(guild(*guild_id), perm::MANAGE_GUILD, MANAGE_GUILD),
            // In a category its overwrites count.
            C::CreateChannel { guild_id, parent_id, .. } => {
                need(parent_id.map_or_else(|| guild(*guild_id), channel), perm::MANAGE_CHANNELS, MANAGE_CHANNELS)
            }
            C::UpdateChannel { channel_id, .. } | C::DeleteChannel { channel_id } => {
                need(channel(*channel_id), perm::MANAGE_CHANNELS, MANAGE_CHANNELS)
            }
            C::ReorderChannels { guild_id, .. } => need(guild(*guild_id), perm::MANAGE_CHANNELS, MANAGE_CHANNELS),
            C::SetPermissionOverwrite { channel_id, overwrite } => {
                need(channel(*channel_id), perm::MANAGE_ROLES, MANAGE_ROLE)?;
                let Some(ch) = self.channels.get(channel_id) else { return Err(MANAGE_ROLE) };
                let g = ch.guild_id.unwrap_or_default();
                let mine = |o: &Overwrite| self.is_me(o.id) || self.my_roles(g).contains(&o.id);
                // A MANAGE_ROLES overwrite for me here lifts the "only bits you have" rule.
                if ch.permission_overwrites.iter().any(|o| mine(o) && o.allow.0 & perm::MANAGE_ROLES != 0) {
                    return Ok(());
                }
                // Bits I have in the parent (or guild), not here; PUT re-sends unchanged bits.
                let have = ch.parent_id.map_or_else(|| guild(g), channel);
                let old = ch.permission_overwrites.iter().find(|o| o.id == overwrite.id);
                let (allow, deny) = old.map_or((0, 0), |o| (o.allow.0, o.deny.0));
                check(((allow ^ overwrite.allow.0) | (deny ^ overwrite.deny.0)) & !have == 0, GRANT)
            }
            C::DeletePermissionOverwrite { channel_id, .. } => need(channel(*channel_id), perm::MANAGE_ROLES, MANAGE_ROLE),
            C::CreateRole { guild_id, role } => {
                let have = guild(*guild_id);
                need(have, perm::MANAGE_ROLES, MANAGE_ROLE)?;
                check(role.permissions.unwrap_or(0) & !have == 0, GRANT)
            }
            C::UpdateRole { guild_id, role_id, patch } => {
                check(self.can_manage_role(*guild_id, *role_id), MANAGE_ROLE)?;
                // PATCH carries the whole bitfield: only bits that change must be mine.
                let role = self.guild(*guild_id).and_then(|g| g.roles.iter().find(|r| r.id == *role_id));
                let old = role.map_or(0, |r| r.permissions.0);
                check(patch.permissions.map_or(0, |p| p ^ old) & !guild(*guild_id) == 0, GRANT)
            }
            C::DeleteRole { guild_id, role_id }
            | C::AddMemberRole { guild_id, role_id, .. }
            | C::RemoveMemberRole { guild_id, role_id, .. } => {
                check(role_id != guild_id, EVERYONE)?;
                check(self.can_manage_role(*guild_id, *role_id), MANAGE_ROLE)
            }
            // Each role must be mine to manage and land below my top role (@everyone stays 0).
            C::ReorderRoles { guild_id, positions } => {
                let owner = self.guild(*guild_id).is_some_and(|g| self.is_me(g.owner_id));
                let top = self.my_top_position(*guild_id);
                positions.iter().try_for_each(|(role_id, position)| {
                    check(role_id != guild_id, EVERYONE)?;
                    check(self.can_manage_role(*guild_id, *role_id), MANAGE_ROLE)?;
                    check(*position >= 1 && (owner || *position < top), MANAGE_ROLE)
                })
            }
            C::SetMyNick { guild_id, .. } => need(guild(*guild_id), perm::CHANGE_NICKNAME | perm::MANAGE_NICKNAMES, MY_NICK),
            C::SetNick { guild_id, user_id, .. } => moderate(*guild_id, *user_id, perm::MANAGE_NICKNAMES, NICK),
            C::KickMember { guild_id, user_id, .. } => moderate(*guild_id, *user_id, perm::KICK_MEMBERS, KICK),
            C::BanMember { guild_id, user_id, .. } => moderate(*guild_id, *user_id, perm::BAN_MEMBERS, BAN),
            C::Unban { guild_id, .. } | C::FetchBans { guild_id, .. } => need(guild(*guild_id), perm::BAN_MEMBERS, BANS),
            C::UploadEmoji { guild_id, .. } => {
                need(guild(*guild_id), perm::CREATE_GUILD_EXPRESSIONS | perm::MANAGE_GUILD_EXPRESSIONS, EXPRESSIONS)
            }
            // shortcut: CREATE_GUILD_EXPRESSIONS may delete one's own emoji, but the uploader is
            // not cached; that case stays hidden until it is.
            C::DeleteEmoji { guild_id, .. } => need(guild(*guild_id), perm::MANAGE_GUILD_EXPRESSIONS, MANAGE_EXPRESSIONS),
            C::CreateInvite { channel_id, .. } => need(channel(*channel_id), perm::CREATE_INSTANT_INVITE, INVITE),
            C::FetchGuildInvites { guild_id } => need(guild(*guild_id), perm::MANAGE_GUILD | perm::VIEW_AUDIT_LOG, MANAGE_GUILD),
            _ => Ok(()),
        }
    }
}

/// Any of `bits`.
fn need(have: u64, bits: u64, why: &'static str) -> Result<(), &'static str> {
    check(have & bits != 0, why)
}

fn check(ok: bool, why: &'static str) -> Result<(), &'static str> {
    if ok { Ok(()) } else { Err(why) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RolePatch;
    use crate::model::*;

    const G: Id = Id(10);
    const ME: Id = Id(1);
    const OWNER: Id = Id(99);
    const MOD_ROLE: Id = Id(100);
    const LOW_ROLE: Id = Id(101);
    const HIGH_ROLE: Id = Id(102);
    const TEXT: Id = Id(20);

    fn state(my_perms: u64) -> AppState {
        let role = |id: Id, position: i32, perms: u64| Role { id, position, permissions: Perms(perms), ..Default::default() };
        let mut s = AppState { me: Some(User { id: ME, ..Default::default() }), ..Default::default() };
        s.guilds = vec![Guild {
            id: G,
            owner_id: OWNER,
            roles: vec![role(G, 0, perm::VIEW_CHANNEL), role(MOD_ROLE, 2, my_perms), role(LOW_ROLE, 1, 0), role(HIGH_ROLE, 5, 0)],
            ..Default::default()
        }];
        s.my_members.insert(G, Member { roles: vec![MOD_ROLE], ..Default::default() });
        s.channels.insert(TEXT, Channel { id: TEXT, kind: channel_type::GUILD_TEXT, guild_id: Some(G), ..Default::default() });
        s
    }

    #[test]
    fn moderation_needs_the_bit_and_a_higher_role() {
        let s = state(perm::KICK_MEMBERS | perm::MANAGE_NICKNAMES);
        let kick = |user_id| AccountCommand::KickMember { guild_id: G, user_id, reason: None };
        assert_eq!(s.account_check(&kick(Id(7))), Ok(()), "no cached member: @everyone only");
        assert_eq!(s.account_check(&kick(OWNER)), Err(KICK));
        assert_eq!(s.account_check(&kick(ME)), Err(KICK));
        let ban = AccountCommand::BanMember { guild_id: G, user_id: Id(7), reason: None, delete_message_seconds: 0 };
        assert_eq!(s.account_check(&ban), Err(BAN));
        assert_eq!(s.account_check(&AccountCommand::FetchBans { guild_id: G, after: None }), Err(BANS));
        assert_eq!(s.account_check(&AccountCommand::SetNick { guild_id: G, user_id: Id(7), nick: None }), Ok(()));
        assert_eq!(s.account_check(&AccountCommand::SetMyNick { guild_id: G, nick: None }), Ok(()), "MANAGE_NICKNAMES covers mine");
        assert_eq!(s.account_check(&AccountCommand::UpdateGuild { guild_id: G, patch: Default::default() }), Err(MANAGE_GUILD));
        assert_eq!(s.account_check(&AccountCommand::LeaveGuild { guild_id: G }), Ok(()));
        assert_eq!(s.account_check(&AccountCommand::CreateGuild { name: "x".into(), icon: None }), Ok(()), "needs nothing");
    }

    #[test]
    fn roles_follow_the_hierarchy_and_my_permissions() {
        let mut s = state(perm::MANAGE_ROLES | perm::KICK_MEMBERS);
        let create = |permissions| AccountCommand::CreateRole { guild_id: G, role: RolePatch { permissions, ..Default::default() } };
        assert_eq!(s.account_check(&create(Some(perm::KICK_MEMBERS))), Ok(()));
        assert_eq!(s.account_check(&create(Some(perm::BAN_MEMBERS))), Err(GRANT));
        let delete = |role_id| AccountCommand::DeleteRole { guild_id: G, role_id };
        assert_eq!(s.account_check(&delete(LOW_ROLE)), Ok(()));
        assert_eq!(s.account_check(&delete(HIGH_ROLE)), Err(MANAGE_ROLE));
        assert_eq!(s.account_check(&delete(G)), Err(EVERYONE));
        let assign = AccountCommand::AddMemberRole { guild_id: G, user_id: Id(7), role_id: MOD_ROLE };
        assert_eq!(s.account_check(&assign), Err(MANAGE_ROLE), "not my own top role");
        let reorder = |positions: Vec<(Id, i32)>| AccountCommand::ReorderRoles { guild_id: G, positions };
        assert_eq!(s.account_check(&reorder(vec![(LOW_ROLE, 1)])), Ok(()));
        assert_eq!(s.account_check(&reorder(vec![(LOW_ROLE, 1), (HIGH_ROLE, 2)])), Err(MANAGE_ROLE));
        assert_eq!(s.account_check(&reorder(vec![(LOW_ROLE, 2)])), Err(MANAGE_ROLE), "onto my top role");
        assert_eq!(s.account_check(&reorder(vec![(LOW_ROLE, 0)])), Err(MANAGE_ROLE), "@everyone's place");
        let edit_everyone = AccountCommand::UpdateRole { guild_id: G, role_id: G, patch: RolePatch::default() };
        assert_eq!(s.account_check(&edit_everyone), Ok(()), "@everyone can be edited");
        // A role holding a bit I lack stays editable while that bit is left as it is.
        s.guilds[0].roles[2].permissions = Perms(perm::MENTION_EVERYONE);
        let edit = |p| AccountCommand::UpdateRole { guild_id: G, role_id: LOW_ROLE, patch: RolePatch { permissions: Some(p), ..Default::default() } };
        assert_eq!(s.account_check(&edit(perm::MENTION_EVERYONE | perm::KICK_MEMBERS)), Ok(()));
        assert_eq!(s.account_check(&edit(perm::MENTION_EVERYONE | perm::BAN_MEMBERS)), Err(GRANT));
        assert_eq!(s.account_check(&edit(0)), Err(GRANT), "nor removed");
    }

    #[test]
    fn overwrites_check_changed_bits_against_the_parent() {
        const CAT: Id = Id(30);
        let mut s = state(perm::MANAGE_ROLES | perm::ATTACH_FILES);
        s.channels.insert(CAT, Channel { id: CAT, kind: channel_type::GUILD_CATEGORY, guild_id: Some(G), ..Default::default() });
        let ow = |id, allow: u64| Overwrite { id, kind: 0, allow: Perms(allow), deny: Perms(0) };
        let set = |overwrite| AccountCommand::SetPermissionOverwrite { channel_id: TEXT, overwrite };
        fn text(s: &mut AppState) -> &mut Channel {
            s.channels.get_mut(&TEXT).unwrap()
        }
        // Denied to me here but mine in the guild: may still be allowed for others.
        text(&mut s).permission_overwrites = vec![Overwrite { id: MOD_ROLE, kind: 0, allow: Perms(0), deny: Perms(perm::ATTACH_FILES) }];
        assert_eq!(s.account_check(&set(ow(LOW_ROLE, perm::ATTACH_FILES))), Ok(()));
        // Granted to me only here: not mine to grant.
        text(&mut s).permission_overwrites = vec![ow(MOD_ROLE, perm::SEND_MESSAGES)];
        assert_eq!(s.account_check(&set(ow(LOW_ROLE, perm::SEND_MESSAGES))), Err(GRANT));
        // Bits the overwrite already holds are re-sent unchanged.
        text(&mut s).permission_overwrites = vec![ow(LOW_ROLE, perm::MENTION_EVERYONE)];
        assert_eq!(s.account_check(&set(ow(LOW_ROLE, perm::MENTION_EVERYONE | perm::ATTACH_FILES))), Ok(()));
        assert_eq!(s.account_check(&set(ow(HIGH_ROLE, perm::MENTION_EVERYONE))), Err(GRANT));
        // In a category, my permissions there count.
        text(&mut s).parent_id = Some(CAT);
        s.channels.get_mut(&CAT).unwrap().permission_overwrites = vec![ow(MOD_ROLE, perm::SEND_MESSAGES)];
        assert_eq!(s.account_check(&set(ow(HIGH_ROLE, perm::SEND_MESSAGES))), Ok(()));
        // A MANAGE_ROLES overwrite for me here lifts the rule.
        text(&mut s).permission_overwrites = vec![ow(MOD_ROLE, perm::MANAGE_ROLES)];
        assert_eq!(s.account_check(&set(ow(HIGH_ROLE, perm::BAN_MEMBERS))), Ok(()));
    }

    #[test]
    fn mfa_moderation_needs_my_2fa() {
        let mut s = state(perm::KICK_MEMBERS | perm::MANAGE_ROLES | perm::CHANGE_NICKNAME);
        s.guilds[0].mfa_level = 1;
        let kick = AccountCommand::KickMember { guild_id: G, user_id: Id(7), reason: None };
        assert_eq!(s.account_check(&kick), Err(KICK));
        assert_eq!(s.account_check(&AccountCommand::DeleteRole { guild_id: G, role_id: LOW_ROLE }), Err(MANAGE_ROLE));
        assert_eq!(s.account_check(&AccountCommand::SetMyNick { guild_id: G, nick: None }), Ok(()), "not an elevated bit");
        s.me.as_mut().unwrap().mfa_enabled = true;
        assert_eq!(s.account_check(&kick), Ok(()));
        s.me.as_mut().unwrap().mfa_enabled = false;
        s.guilds[0].owner_id = ME;
        assert_eq!(s.account_check(&kick), Ok(()), "the owner");
        // ADMINISTRATOR without 2FA keeps the plain bits, in the guild and in channels.
        let mut admin = state(perm::ADMINISTRATOR);
        admin.guilds[0].mfa_level = 1;
        assert_eq!(admin.guild_permissions(G) & (perm::KICK_MEMBERS | perm::CREATE_INSTANT_INVITE), perm::CREATE_INSTANT_INVITE);
        assert_eq!(admin.account_check(&AccountCommand::DeleteChannel { channel_id: TEXT }), Err(MANAGE_CHANNELS));
    }

    #[test]
    fn channel_actions_use_channel_permissions() {
        let mut s = state(perm::MANAGE_CHANNELS | perm::MANAGE_ROLES | perm::VIEW_CHANNEL);
        let ow = |allow: u64| AccountCommand::SetPermissionOverwrite {
            channel_id: TEXT,
            overwrite: Overwrite { id: G, kind: 0, allow: Perms(allow), deny: Perms(0) },
        };
        assert_eq!(s.account_check(&ow(perm::VIEW_CHANNEL)), Ok(()));
        assert_eq!(s.account_check(&ow(perm::SEND_MESSAGES)), Err(GRANT));
        assert_eq!(s.account_check(&AccountCommand::DeleteChannel { channel_id: TEXT }), Ok(()));
        assert_eq!(s.account_check(&AccountCommand::CreateInvite { channel_id: TEXT, max_age: 0, max_uses: 0, temporary: false }), Err(INVITE));
        // A channel overwrite taking MANAGE_CHANNELS from my role wins there.
        let deny = Overwrite { id: MOD_ROLE, kind: 0, allow: Perms(0), deny: Perms(perm::MANAGE_CHANNELS) };
        s.channels.get_mut(&TEXT).unwrap().permission_overwrites = vec![deny];
        assert_eq!(s.account_check(&AccountCommand::DeleteChannel { channel_id: TEXT }), Err(MANAGE_CHANNELS));
        let create = AccountCommand::CreateChannel { guild_id: G, name: "c".into(), kind: 0, parent_id: Some(TEXT) };
        assert_eq!(s.account_check(&create), Err(MANAGE_CHANNELS));
        let create_top = AccountCommand::CreateChannel { guild_id: G, name: "c".into(), kind: 0, parent_id: None };
        assert_eq!(s.account_check(&create_top), Ok(()));
    }

    #[test]
    fn the_owner_may_do_everything_but_leave() {
        let mut s = state(0);
        s.guilds[0].owner_id = ME;
        assert_eq!(s.account_check(&AccountCommand::LeaveGuild { guild_id: G }), Err(OWNER_LEAVE));
        assert_eq!(s.account_check(&AccountCommand::DeleteRole { guild_id: G, role_id: HIGH_ROLE }), Ok(()));
        let reorder = AccountCommand::ReorderRoles { guild_id: G, positions: vec![(LOW_ROLE, 9)] };
        assert_eq!(s.account_check(&reorder), Ok(()), "anywhere above @everyone");
        let ban = AccountCommand::BanMember { guild_id: G, user_id: Id(7), reason: None, delete_message_seconds: 0 };
        assert_eq!(s.account_check(&ban), Ok(()));
        assert_eq!(s.account_check(&AccountCommand::DeleteEmoji { guild_id: G, emoji_id: Id(3) }), Ok(()));
    }
}
