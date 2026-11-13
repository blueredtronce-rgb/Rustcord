//! Account & server state (FEATURES_NOTES Part 3): the settings-proto view (saved status,
//! custom status, server bar folders), progress of user-started account/admin actions, the
//! bans and invites admin views, the invite-preview cache, and pure helpers for the server bar,
//! admin gating and "Mark as read" on a guild. Pure data + logic like the rest of `state`;
//! every collection is bounded.
//!
//! Views: the UI opens one by setting it (e.g. `state.account.bans =
//! Some(BansView::new(guild_id))`) and sending the matching `AccountCommand`;
//! `AppState::apply_account` fills it and drops answers that no longer match.

/// `AppState::account_check`: the local permission gate for admin commands.
mod admin;

use super::{AppState, social};
use crate::api::settings_proto::{self, SettingValue};
use crate::backend::{AccountEvent, AccountOp, OpKind};
use crate::model::*;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Action progress entries kept; finished ones are dropped first.
const MAX_OPS: usize = 32;
/// Invite previews kept (LRU).
pub const INVITE_CACHE: usize = 64;
/// A resolved invite (or its error) is shown without refetching for this long.
pub const INVITE_TTL: Duration = Duration::from_secs(600);
/// `GET /guilds/{g}/bans` page size (the API maximum).
pub const BAN_PAGE: usize = 1000;
/// Bans one view holds; paging stops there.
const MAX_BANS: usize = 10_000;
/// Bits that count as absent in a guild with `mfa_level` 1 while my account has no 2FA (§3.3).
const ELEVATED: u64 = perm::KICK_MEMBERS
    | perm::BAN_MEMBERS
    | perm::ADMINISTRATOR
    | perm::MANAGE_CHANNELS
    | perm::MANAGE_GUILD
    | perm::MANAGE_MESSAGES
    | perm::MANAGE_ROLES
    | perm::MANAGE_WEBHOOKS
    | perm::MANAGE_GUILD_EXPRESSIONS
    | perm::MANAGE_THREADS;

#[derive(Debug, Default)]
pub struct AccountState {
    /// Settings proto (READY + USER_SETTINGS_PROTO_UPDATE). Not cached.
    pub settings: SettingsView,
    /// Progress of writes the UI waits on, keyed by `AccountCommand::op` (`begin`). Answers
    /// without an entry become toasts on error.
    pub ops: HashMap<AccountOp, OpState>,
    /// Server settings > Bans.
    pub bans: Option<BansView>,
    /// Server settings > Invites.
    pub invites: Option<InvitesView>,
    /// Invite modal: the invite just created.
    pub created_invite: Option<CreatedInvite>,
    /// Invite embeds / join dialog, least recently requested first (≤ `INVITE_CACHE`).
    pub invite_previews: VecDeque<InvitePreview>,
    /// Guild or channel to open once the gateway knows it (after CreateGuild, AcceptInvite,
    /// CreateChannel).
    select_on_create: Option<Id>,
    /// READY `auth_session_id_hash` (this login in Settings → Devices; never logged out from there).
    pub session_hash: String,
    /// Settings → Devices, Authorized Apps, Data & Privacy consents: None while loading. Set to
    /// None and send the fetch when the page opens.
    pub sessions: Option<Result<Vec<AuthSession>, String>>,
    pub authorized_apps: Option<Result<Vec<AuthorizedApp>, String>>,
    pub consents: Option<Result<Consents, String>>,
    /// My Account > Account Standing (`StandingLoaded`), like the lists above.
    pub standing: Option<Result<u16, String>>,
    /// My Account > Security Keys (`SecurityKeysLoaded`), like the lists above.
    pub security_keys: Option<Result<Vec<crate::api::account::SecurityKey>, String>>,
    /// The 2FA re-check an action waits for (`AccountEvent::Mfa`); the UI shows it as a dialog.
    pub mfa: Option<crate::backend::MfaPrompt>,
    /// Backup codes just received, until the UI takes them into its dialog (shown once, wiped
    /// with it).
    pub backup_codes: Option<Vec<crate::api::account::BackupCode>>,
    /// View Backup Codes step 1's nonces, until the UI takes them.
    pub codes_nonce: Option<crate::api::account::CodesNonce>,
}

/// What the settings proto says; defaults until READY's arrives.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SettingsView {
    /// READY's settings proto arrived. Before that `server_bar()` uses the cached guild order.
    pub loaded: bool,
    /// Saved status, custom status and its expiry (the effective status is `chat.my_status`).
    pub status: StatusSettings,
    /// Server bar entries as saved (may name guilds I left; `server_bar()` filters).
    pub folders: Vec<GuildFolder>,
    /// Every top-level field's bytes, for the Settings pages' other options (`proto_varint`).
    pub proto: BTreeMap<u32, Vec<u8>>,
    /// A top-level field as it was before a `SetSetting` still on its way (None = absent):
    /// put back if the write fails.
    stash: HashMap<u32, Option<Vec<u8>>>,
}

impl SettingsView {
    /// A synced option's varint (docs/SETTINGS_NOTES.md: `top`.`path`, wrapped = BoolValue /
    /// UInt32Value); None = not set (the caller's default applies).
    pub fn proto_varint(&self, top: u32, path: &[u32], wrapped: bool) -> Option<u64> {
        settings_proto::read_varint(self.proto.get(&top)?, path, wrapped)
    }

    pub fn proto_str(&self, top: u32, path: &[u32]) -> Option<String> {
        settings_proto::read_str(self.proto.get(&top)?, path)
    }

    /// A plain varint inside entry `key` of the map field `map`, e.g. 3.1[guild].11.
    pub fn proto_entry_varint(&self, top: u32, map: u32, key: u64, path: &[u32]) -> Option<u64> {
        settings_proto::read_varint(&settings_proto::read_entry(self.proto.get(&top)?, map, key)?, path, false)
    }

    /// A `repeated fixed64` id list (8.3 restricted guilds …); empty when unset.
    pub fn proto_ids(&self, top: u32, path: &[u32]) -> Vec<u64> {
        self.proto.get(&top).map_or_else(Vec::new, |f| settings_proto::read_ids(f, path))
    }

    /// Shows a `SetSetting` at once (the USER_SETTINGS_PROTO_UPDATE echo replaces it; a failed
    /// write puts the old field back).
    pub fn set_local(&mut self, top: u32, values: &[(Vec<u32>, SettingValue)]) {
        let old = self.proto.get(&top).cloned();
        self.stash.entry(top).or_insert_with(|| old.clone());
        let mut field = old.unwrap_or_default();
        for (path, v) in values {
            match settings_proto::set_in(&field, path, v) {
                Ok(f) => field = f,
                Err(e) => eprintln!("settings: local edit of {top} skipped: {e}"),
            }
        }
        self.proto.insert(top, field);
    }

    /// Settings > Advanced > Developer Mode (13.2, plain bool): "Copy … ID" menu items.
    pub fn developer_mode(&self) -> bool {
        self.proto_varint(13, &[2], false).is_some_and(|v| v != 0)
    }

    /// Appearance > Message display: Compact (6.17 BoolValue).
    pub fn compact_messages(&self) -> bool {
        self.proto_varint(6, &[17], true).is_some_and(|v| v != 0)
    }

    /// A Chat page BoolValue switch of field 6 (`sub`), on unless set off: 6.9 attachments,
    /// 6.10 linked media, 6.12 embeds, 6.13 reactions.
    pub fn chat_shows(&self, sub: u32) -> bool {
        self.proto_varint(6, &[sub], true).is_none_or(|v| v != 0)
    }

    /// Chat > Show spoiler content (6.4 StringValue): "ALWAYS", or "IF_MODERATOR" where
    /// `moderator()`; anything else (default "ON_CLICK") keeps spoilers covered.
    pub fn spoilers_shown(&self, moderator: impl FnOnce() -> bool) -> bool {
        match self.proto_str(6, &[4]).as_deref() {
            Some("ALWAYS") => true,
            Some("IF_MODERATOR") => moderator(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpState {
    Pending,
    /// The created guild / channel / role id, or the guild an invite joined.
    Done(Option<Id>),
    Failed(String),
}

#[derive(Debug)]
pub struct BansView {
    pub guild_id: Id,
    /// By user id ascending (API order), deduplicated, ≤ 10,000.
    pub bans: Vec<Ban>,
    pub has_more: bool,
    pub loading: bool,
    pub error: Option<String>,
}

impl BansView {
    /// A view waiting for its first page (send `FetchBans { after: None }`).
    pub fn new(guild_id: Id) -> Self {
        Self { guild_id, bans: Vec::new(), has_more: false, loading: true, error: None }
    }

    /// `after` for the next page (set `loading` and send `FetchBans`); None at the end.
    pub fn next_after(&self) -> Option<Id> {
        (self.has_more && !self.loading).then(|| self.bans.last().map(|b| b.user.id)).flatten()
    }
}

#[derive(Debug)]
pub struct InvitesView {
    pub guild_id: Id,
    pub invites: Vec<Invite>,
    pub loading: bool,
    pub error: Option<String>,
}

impl InvitesView {
    /// A view waiting for its list (send `FetchGuildInvites`).
    pub fn new(guild_id: Id) -> Self {
        Self { guild_id, invites: Vec::new(), loading: true, error: None }
    }
}

#[derive(Debug)]
pub struct CreatedInvite {
    pub channel_id: Id,
    /// None while creating; `Ok(None)` = invites are paused for this server.
    pub result: Option<Result<Option<Invite>, String>>,
}

#[derive(Debug)]
pub struct InvitePreview {
    pub code: String,
    /// When the last request went out.
    pub fetched: Instant,
    pub loading: bool,
    /// The last answer; kept (shown) while a refetch runs. None = never answered.
    pub result: Option<Result<Invite, String>>,
}

/// One server-bar entry, in display order (`AppState::server_bar`).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerBarItem {
    Guild(Id),
    /// `id` is Some; `guild_ids` only known guilds, never empty.
    Folder(GuildFolder),
}

/// Where `AppState::move_guild` puts a dragged guild. Indices are as shown before the move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuildDrop {
    /// Top level, before entry `index` of `server_bar()` (past the end = last).
    Top(usize),
    /// Into folder `id`, before its guild at `index` (past the end = last).
    Folder { id: i64, index: usize },
    /// Onto the top-level guild `with`: both become a new folder (`with` first).
    NewFolder { with: Id },
}

impl AccountState {
    /// Call right before sending a write the UI shows progress for: `if begin(cmd.op()) { send }`.
    /// False (nothing changed) while the same op is still pending: send nothing then, so a
    /// double click never creates two servers or joins twice (§3.12; at least for
    /// CreateGuild, AcceptInvite and UpdateUsername).
    #[must_use]
    pub fn begin(&mut self, op: AccountOp) -> bool {
        if self.ops.get(&op) == Some(&OpState::Pending) {
            return false;
        }
        if self.ops.len() >= MAX_OPS {
            self.ops.retain(|_, s| *s == OpState::Pending);
        }
        self.ops.insert(op, OpState::Pending);
        true
    }

    /// An answer's progress entry; the error text when nobody waits on it (worth a toast).
    fn finish(&mut self, op: AccountOp, result: Result<(), String>) -> Option<String> {
        match self.ops.get_mut(&op) {
            Some(s) => {
                *s = result.map_or_else(OpState::Failed, |()| OpState::Done(None));
                None
            }
            None => result.err(),
        }
    }
}

impl AppState {
    /// Spoilers in the open channel show uncovered (Settings > Chat; moderator = Manage Messages).
    pub fn spoilers_open(&self) -> bool {
        let moderator = || self.selected_channel.is_some_and(|c| self.channel_permissions(c) & perm::MANAGE_MESSAGES != 0);
        self.account.settings.spoilers_shown(moderator)
    }

    /// Applies an `AccountEvent`. Returns error text worth a toast (answers nobody waits on).
    pub fn apply_account(&mut self, ev: AccountEvent) -> Option<String> {
        use AccountEvent as E;
        match ev {
            // Settings pages show the switch at once: forget the old field, or put it back.
            E::Done { op, result } if op.kind == OpKind::SetSetting => {
                self.account.ops.remove(&op);
                let top = op.target.and_then(|t| u32::try_from(t.0).ok()).unwrap_or_default();
                let s = &mut self.account.settings;
                if let (Some(old), Err(_)) = (s.stash.remove(&top), &result) {
                    match old {
                        Some(f) => s.proto.insert(top, f),
                        None => s.proto.remove(&top),
                    };
                }
                return result.err();
            }
            E::SessionsLoaded(r) => self.account.sessions = Some(r),
            E::AuthorizedAppsLoaded(r) => self.account.authorized_apps = Some(r),
            E::ConsentsLoaded(r) => self.account.consents = Some(r),
            E::StandingLoaded(r) => self.account.standing = Some(r),
            E::SecurityKeysLoaded(r) => self.account.security_keys = Some(r),
            E::Mfa(p) => self.account.mfa = p,
            E::BackupCodes { op, result } => {
                let result = result.map(|codes| self.account.backup_codes = Some(codes));
                return self.account.finish(op, result);
            }
            E::CodesNonce { op, result } => {
                let result = result.map(|n| self.account.codes_nonce = Some(n));
                return self.account.finish(op, result);
            }
            E::Done { op, result } => {
                if let Ok(Some(id)) = result
                    && matches!(op.kind, OpKind::CreateGuild | OpKind::AcceptInvite | OpKind::CreateChannel)
                {
                    self.account.select_on_create = Some(id);
                    self.on_account_created(id);
                }
                // My profile's effect / frame / theme changed: reload it when next shown.
                if op.kind == OpKind::UpdateCosmetics && result.is_ok() {
                    let me = self.me.as_ref().map(|u| u.id);
                    self.social.profiles.retain(|e| Some(e.user_id) != me);
                }
                match self.account.ops.get_mut(&op) {
                    Some(s) => *s = result.map_or_else(OpState::Failed, OpState::Done),
                    None => return result.err(),
                }
            }
            E::InviteCreated { channel_id, result } => match self.account.created_invite.as_mut().filter(|v| v.channel_id == channel_id) {
                Some(v) => v.result = Some(result),
                None => return result.err(),
            },
            E::InviteResolved { code, result } => {
                if let Some(p) = self.account.invite_previews.iter_mut().find(|p| p.code == code) {
                    (p.loading, p.result) = (false, Some(result));
                }
            }
            E::BansLoaded { guild_id, after, result } => {
                let v = self.account.bans.as_mut().filter(|v| v.guild_id == guild_id && v.loading)?;
                v.loading = false;
                match result {
                    Ok(page) => {
                        if after.is_none() {
                            v.bans.clear();
                        }
                        let full = page.len() >= BAN_PAGE;
                        let known: HashSet<Id> = v.bans.iter().map(|b| b.user.id).collect();
                        let room = MAX_BANS.saturating_sub(v.bans.len());
                        v.bans.extend(page.into_iter().filter(|b| !known.contains(&b.user.id)).take(room));
                        (v.has_more, v.error) = (full && v.bans.len() < MAX_BANS, None);
                    }
                    Err(e) => v.error = Some(e),
                }
            }
            E::InvitesLoaded { guild_id, result } => {
                let v = self.account.invites.as_mut().filter(|v| v.guild_id == guild_id)?;
                v.loading = false;
                match result {
                    Ok(list) => (v.invites, v.error) = (list, None),
                    Err(e) => v.error = Some(e),
                }
            }
        }
        None
    }

    /// Call when an invite embed / join dialog shows `code`. Returns whether to send
    /// `ResolveInvite` now (not cached within `INVITE_TTL`, not loading); marks it most recently
    /// used.
    pub fn request_invite(&mut self, code: &str, now: Instant) -> bool {
        let p = &mut self.account.invite_previews;
        let mut e = match p.iter().position(|e| e.code == code) {
            Some(i) => p.remove(i).expect("index from position"),
            None => InvitePreview { code: code.to_owned(), fetched: now, loading: false, result: None },
        };
        let fetch = !e.loading && (e.result.is_none() || now.saturating_duration_since(e.fetched) >= INVITE_TTL);
        if fetch {
            (e.loading, e.fetched) = (true, now);
        }
        if p.len() >= INVITE_CACHE {
            p.pop_front();
        }
        p.push_back(e);
        fetch
    }

    pub fn invite_preview(&self, code: &str) -> Option<&InvitePreview> {
        self.account.invite_previews.iter().find(|p| p.code == code)
    }

    /// The server bar: guilds outside the saved list first (just joined; newest first), then the
    /// saved order. Unknown and repeated guild ids are skipped, empty folders dropped. Before the
    /// settings arrive: `guilds` order.
    pub fn server_bar(&self) -> Vec<ServerBarItem> {
        let known: HashSet<Id> = self.guilds.iter().map(|g| g.id).collect();
        let mut seen = HashSet::new();
        let mut placed = Vec::new();
        for f in &self.account.settings.folders {
            let ids: Vec<Id> = f.guild_ids.iter().copied().filter(|g| known.contains(g) && seen.insert(*g)).collect();
            match f.id {
                Some(_) if !ids.is_empty() => {
                    placed.push(ServerBarItem::Folder(GuildFolder { id: f.id, name: f.name.clone(), color: f.color, guild_ids: ids }))
                }
                _ => placed.extend(ids.into_iter().map(ServerBarItem::Guild)),
            }
        }
        let mut loose: Vec<Id> = self.guilds.iter().map(|g| g.id).filter(|g| !seen.contains(g)).collect();
        if !seen.is_empty() {
            loose.sort_unstable_by_key(|g| Reverse(*g));
        }
        loose.into_iter().map(ServerBarItem::Guild).chain(placed).collect()
    }

    /// The whole folder list after dragging `guild_id` to `to`, for `SetGuildFolders` (put it
    /// into `account.settings.folders` too, so the bar moves at once). Every known guild is in
    /// it; an impossible move returns the current list. None until the saved folders are known
    /// (`settings.loaded`): a list built from the fallback order would delete every folder.
    pub fn move_guild(&self, guild_id: Id, to: GuildDrop) -> Option<Vec<GuildFolder>> {
        if !self.account.settings.loaded {
            return None;
        }
        let mut list: Vec<GuildFolder> = (self.server_bar().into_iter())
            .map(|item| match item {
                ServerBarItem::Guild(id) => single(id),
                ServerBarItem::Folder(f) => f,
            })
            .collect();
        if self.guild(guild_id).is_none() {
            return Some(list);
        }
        let before = list.clone();
        match to {
            GuildDrop::Top(index) => {
                let anchor = list.iter().skip(index).find(|e| **e != single(guild_id)).map(entry_key);
                remove_guild(&mut list, guild_id);
                let at = anchor.and_then(|k| list.iter().position(|e| entry_key(e) == k)).unwrap_or(list.len());
                list.insert(at, single(guild_id));
            }
            GuildDrop::Folder { id, index } => {
                let folder = list.iter().find(|f| f.id == Some(id));
                let anchor = folder.and_then(|f| f.guild_ids.iter().skip(index).find(|g| **g != guild_id)).copied();
                remove_guild(&mut list, guild_id);
                // Gone when it only held this guild: dropping it back in is no move.
                let Some(f) = list.iter_mut().find(|f| f.id == Some(id)) else { return Some(before) };
                let at = anchor.and_then(|a| f.guild_ids.iter().position(|g| *g == a)).unwrap_or(f.guild_ids.len());
                f.guild_ids.insert(at, guild_id);
            }
            GuildDrop::NewFolder { with } => {
                remove_guild(&mut list, guild_id);
                let id = new_folder_id(&list);
                let Some(e) = list.iter_mut().find(|e| **e == single(with)).filter(|_| with != guild_id) else { return Some(before) };
                *e = GuildFolder { id: Some(id), name: None, color: None, guild_ids: vec![with, guild_id] };
            }
        }
        Some(list)
    }

    /// "Mark as read" on a guild (§3.11): marks its unread channels and joined threads locally
    /// and returns the acks; send them as `SocialCommand::MarkAllRead { acks }` when
    /// `settings.send_read_acks` is on (the local mark happens either way).
    pub fn mark_guild_read(&mut self, guild_id: Id) -> Vec<(Id, Id)> {
        let unread = |s: &Self, c: &Channel| {
            let rs = &s.read_states[&c.id];
            rs.mention_count > 0 || c.last_message_id.zip(rs.last_message_id).is_some_and(|(last, read)| !read.is_zero() && last > read)
        };
        let ids: Vec<Id> = (self.read_states.keys())
            .filter_map(|id| self.channels.get(id))
            .filter(|c| c.guild_id == Some(guild_id) && c.is_text_chat() && !social::inactive_thread(c))
            .filter(|c| unread(self, c) && self.can_view(c.id))
            .map(|c| c.id)
            .collect();
        self.mark_all_read(&ids)
    }

    /// My guild-level permissions (owner / ADMINISTRATOR = all bits; minus `ELEVATED` in a guild
    /// that requires 2FA for moderation while my account has none). Gate admin menus on these
    /// (`perm::MANAGE_GUILD`, `KICK_MEMBERS`, …) to avoid 403s.
    pub fn guild_permissions(&self, guild_id: Id) -> u64 {
        self.guild(guild_id).map_or(0, |g| self.base_permissions(g) & self.mfa_mask(g))
    }

    /// `!ELEVATED` when `g` has `mfa_level` 1 and I (not the owner) have no 2FA, else all bits.
    fn mfa_mask(&self, g: &Guild) -> u64 {
        let lacking = g.mfa_level == 1 && self.me.as_ref().is_some_and(|u| !u.mfa_enabled && u.id != g.owner_id);
        if lacking { !ELEVATED } else { perm::ALL }
    }

    /// Position of my highest role (0 = only @everyone). Unless I own the guild, roles I
    /// reorder must end up below it (§3.5).
    pub fn my_top_position(&self, guild_id: Id) -> i32 {
        self.guild(guild_id).map_or(0, |g| top_rank(g, self.my_roles(guild_id)).0)
    }

    /// May I edit, assign, reorder or delete this role: the owner, or MANAGE_ROLES and the role
    /// ranks below my highest role (ranks: position, then the lower id higher).
    pub fn can_manage_role(&self, guild_id: Id, role_id: Id) -> bool {
        let (Some(g), Some(me)) = (self.guild(guild_id), self.me.as_ref().map(|u| u.id)) else { return false };
        if g.owner_id == me {
            return true;
        }
        let Some(role) = g.roles.iter().find(|r| r.id == role_id) else { return false };
        self.guild_permissions(guild_id) & perm::MANAGE_ROLES != 0 && rank(role) < top_rank(g, self.my_roles(guild_id))
    }

    /// May I moderate this member (kick, ban, nick, roles; the permission bit is checked
    /// separately): never the owner or myself; the owner may; else my highest role must rank
    /// above theirs (`their_roles` from their member object).
    pub fn outranks(&self, guild_id: Id, user_id: Id, their_roles: &[Id]) -> bool {
        let (Some(g), Some(me)) = (self.guild(guild_id), self.me.as_ref().map(|u| u.id)) else { return false };
        if user_id == g.owner_id || user_id == me {
            return false;
        }
        g.owner_id == me || top_rank(g, self.my_roles(guild_id)) > top_rank(g, their_roles)
    }

    // ----- hooks (called from `apply_gateway`) ------------------------------------------

    /// USER_SETTINGS_PROTO, GUILD_BAN_ADD / _REMOVE.
    pub(super) fn on_account_gateway(&mut self, ev: GatewayEvent) {
        match ev {
            GatewayEvent::UserSettingsProto { settings, partial } => {
                let s = &mut self.account.settings;
                if partial {
                    s.proto.extend(settings.raw);
                } else {
                    s.proto = settings.raw;
                }
                if !partial || settings.status.is_some() {
                    s.status = settings.status.unwrap_or_default();
                }
                // `loaded` only once the folders are known: `move_guild` rewrites the whole list.
                if !partial || settings.guild_folders.is_some() {
                    s.folders = settings.guild_folders.unwrap_or_default();
                    s.loaded = true;
                }
            }
            GatewayEvent::GuildBanAdd { guild_id, user } => {
                if let Some(v) = self.account.bans.as_mut().filter(|v| v.guild_id == guild_id)
                    && v.bans.len() < MAX_BANS
                    && !v.bans.iter().any(|b| b.user.id == user.id)
                {
                    v.bans.push(Ban { reason: None, user });
                }
            }
            GatewayEvent::GuildBanRemove { guild_id, user_id } => {
                if let Some(v) = self.account.bans.as_mut().filter(|v| v.guild_id == guild_id) {
                    v.bans.retain(|b| b.user.id != user_id);
                }
            }
            _ => {}
        }
    }

    /// A guild or channel became known (GUILD_CREATE, CHANNEL_CREATE) or a create answer came:
    /// opens it if the user just created or joined it.
    pub(super) fn on_account_created(&mut self, id: Id) {
        if self.account.select_on_create != Some(id) {
            return;
        }
        if self.guild(id).is_some() {
            self.account.select_on_create = None;
            self.select_guild(Some(id));
        } else if let Some(text) = self.channels.get(&id).map(Channel::is_text_chat) {
            self.account.select_on_create = None;
            if text {
                self.select_channel(id);
            }
        }
    }
}

fn single(id: Id) -> GuildFolder {
    GuildFolder { guild_ids: vec![id], ..Default::default() }
}

/// Identity of a top-level entry: its folder id, or its single guild.
fn entry_key(e: &GuildFolder) -> (Option<i64>, Option<Id>) {
    if e.id.is_some() { (e.id, None) } else { (None, e.guild_ids.first().copied()) }
}

fn remove_guild(list: &mut Vec<GuildFolder>, guild_id: Id) {
    for e in list.iter_mut() {
        e.guild_ids.retain(|g| *g != guild_id);
    }
    list.retain(|e| !e.guild_ids.is_empty());
}

/// A random unused u32 (FEATURES_NOTES §3.9), else one above the largest.
fn new_folder_id(list: &[GuildFolder]) -> i64 {
    let used: HashSet<i64> = list.iter().filter_map(|f| f.id).collect();
    (0..8)
        .filter_map(|_| getrandom::u32().ok())
        .map(i64::from)
        .find(|id| *id != 0 && !used.contains(id))
        .unwrap_or_else(|| used.iter().max().map_or(1, |m| m + 1))
}

/// Role hierarchy key: higher position wins, equal positions: the lower id ranks higher.
fn rank(r: &Role) -> (i32, Reverse<Id>) {
    (r.position, Reverse(r.id))
}

/// Highest rank among `roles`; @everyone (position 0, id = guild id) when none.
fn top_rank(g: &Guild, roles: &[Id]) -> (i32, Reverse<Id>) {
    let everyone = (0, Reverse(g.id));
    g.roles.iter().filter(|r| roles.contains(&r.id)).map(rank).max().map_or(everyone, |r| r.max(everyone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AccountCommand;

    const G1: Id = Id(10);
    const G2: Id = Id(20);
    const G3: Id = Id(30);
    const G4: Id = Id(40);
    const ME: Id = Id(1);

    fn state(guilds: &[Id]) -> AppState {
        let mut s = AppState { me: Some(User { id: ME, ..Default::default() }), ..Default::default() };
        s.guilds = guilds.iter().map(|&id| Guild { id, owner_id: Id(99), ..Default::default() }).collect();
        s
    }

    fn folder(id: Option<i64>, ids: &[Id]) -> GuildFolder {
        GuildFolder { id, name: None, color: None, guild_ids: ids.to_vec() }
    }

    fn bar_ids(s: &AppState) -> Vec<String> {
        (s.server_bar().into_iter())
            .map(|i| match i {
                ServerBarItem::Guild(g) => g.to_string(),
                ServerBarItem::Folder(f) => format!("{:?}", f.guild_ids),
            })
            .collect()
    }

    fn proto(folders: Vec<GuildFolder>) -> GatewayEvent {
        GatewayEvent::UserSettingsProto { settings: UserSettingsProto { status: None, guild_folders: Some(folders), ..Default::default() }, partial: true }
    }

    #[test]
    fn settings_proto_merges_partial_and_full() {
        let mut s = state(&[]);
        let status = StatusSettings { status: Some(UserStatus::Dnd), custom_status: None, custom_status_expires_at_ms: Some(5) };
        let status_only = UserSettingsProto { status: Some(status.clone()), guild_folders: None, ..Default::default() };
        s.apply_gateway(GatewayEvent::UserSettingsProto { settings: status_only, partial: true }, None);
        assert!(!s.account.settings.loaded, "folders still unknown");
        let full = UserSettingsProto { status: Some(status.clone()), guild_folders: Some(vec![folder(None, &[G1])]), ..Default::default() };
        s.apply_gateway(GatewayEvent::UserSettingsProto { settings: full, partial: false }, None);
        assert!(s.account.settings.loaded);
        assert_eq!(s.account.settings.status, status);
        s.apply_gateway(proto(vec![folder(Some(7), &[G1, G2])]), None);
        assert_eq!(s.account.settings.status, status, "a partial update keeps absent fields");
        assert_eq!(s.account.settings.folders, vec![folder(Some(7), &[G1, G2])]);
        s.apply_gateway(GatewayEvent::UserSettingsProto { settings: UserSettingsProto::default(), partial: false }, None);
        assert_eq!(s.account.settings.status, StatusSettings::default(), "a full update resets absent fields");
        assert!(s.account.settings.folders.is_empty());
    }

    #[test]
    fn server_bar_order() {
        let mut s = state(&[G1, G2, G3, G4]);
        assert_eq!(bar_ids(&s), ["10", "20", "30", "40"], "no settings yet: guild order");
        s.account.settings.folders = vec![folder(None, &[G3]), folder(Some(5), &[Id(77), G1, G3]), folder(Some(6), &[Id(78)])];
        // G2/G4 are not in the list: first, newest first; unknown ids, repeats and empty folders go.
        assert_eq!(bar_ids(&s), ["40", "20", "30", "[10]"]);
    }

    #[test]
    fn move_guild_reorders_and_folds() {
        let mut s = state(&[G1, G2, G3, G4]);
        assert_eq!(s.move_guild(G1, GuildDrop::Top(9)), None, "folders not known yet");
        s.account.settings.loaded = true;
        let mv = |s: &AppState, g, to| s.move_guild(g, to).unwrap();
        s.account.settings.folders = vec![folder(None, &[G1]), folder(Some(5), &[G2, G3]), folder(None, &[G4])];
        // Top level: before the folder (index 1), to the end, onto its own place (no move).
        assert_eq!(mv(&s, G4, GuildDrop::Top(1)), vec![folder(None, &[G1]), folder(None, &[G4]), folder(Some(5), &[G2, G3])]);
        assert_eq!(mv(&s, G1, GuildDrop::Top(9)), vec![folder(Some(5), &[G2, G3]), folder(None, &[G4]), folder(None, &[G1])]);
        assert_eq!(mv(&s, G1, GuildDrop::Top(0)), vec![folder(None, &[G1]), folder(Some(5), &[G2, G3]), folder(None, &[G4])]);
        // Out of a folder to the top; into a folder; within a folder (down past itself).
        assert_eq!(mv(&s, G2, GuildDrop::Top(0)), vec![folder(None, &[G2]), folder(None, &[G1]), folder(Some(5), &[G3]), folder(None, &[G4])]);
        assert_eq!(mv(&s, G4, GuildDrop::Folder { id: 5, index: 1 }), vec![folder(None, &[G1]), folder(Some(5), &[G2, G4, G3])]);
        assert_eq!(mv(&s, G2, GuildDrop::Folder { id: 5, index: 2 }), vec![folder(None, &[G1]), folder(Some(5), &[G3, G2]), folder(None, &[G4])]);
        // Onto a guild: a new folder in its place.
        let made = mv(&s, G4, GuildDrop::NewFolder { with: G1 });
        assert_eq!(made.len(), 2);
        assert!(made[0].id.is_some_and(|id| id != 5));
        assert_eq!((made[0].guild_ids.as_slice(), &made[1]), (&[G1, G4][..], &folder(Some(5), &[G2, G3])));
        // Impossible moves change nothing.
        let current = mv(&s, Id(999), GuildDrop::Top(0));
        assert_eq!(mv(&s, G1, GuildDrop::NewFolder { with: G1 }), current);
        assert_eq!(mv(&s, G1, GuildDrop::NewFolder { with: G2 }), current, "G2 is inside a folder");
        assert_eq!(mv(&s, G1, GuildDrop::Folder { id: 404, index: 0 }), current);
        s.account.settings.folders = vec![folder(Some(8), &[G1]), folder(None, &[G2]), folder(None, &[G3]), folder(None, &[G4])];
        assert_eq!(mv(&s, G1, GuildDrop::Folder { id: 8, index: 0 })[0], folder(Some(8), &[G1]));
    }

    #[test]
    fn a_cosmetics_write_reloads_my_profile() {
        let mut s = state(&[]);
        let now = std::time::Instant::now();
        for user in [ME, Id(5)] {
            assert!(s.request_profile(user, None, now));
        }
        let op = AccountCommand::UpdateCosmetics { patch: crate::api::collectibles::CosmeticsPatch::ThemeColors(None) }.op();
        s.apply_account(AccountEvent::Done { op, result: Err("no".into()) });
        assert!(s.profile(ME, None).is_some(), "failed: nothing changed");
        s.apply_account(AccountEvent::Done { op, result: Ok(None) });
        assert!(s.profile(ME, None).is_none() && s.profile(Id(5), None).is_some());
    }

    #[test]
    fn ops_answers_and_selection_after_create() {
        let mut s = state(&[G1]);
        let leave = AccountCommand::LeaveGuild { guild_id: G1 }.op();
        assert_eq!(s.apply_account(AccountEvent::Done { op: leave, result: Err("nope".into()) }), Some("nope".into()), "nobody waits: toast");
        assert!(s.account.begin(leave));
        assert_eq!(s.account.ops[&leave], OpState::Pending);
        assert!(!s.account.begin(leave), "still pending: send nothing");
        assert_eq!(s.apply_account(AccountEvent::Done { op: leave, result: Err("nope".into()) }), None);
        assert_eq!(s.account.ops[&leave], OpState::Failed("nope".into()));

        // The REST answer comes first, GUILD_CREATE later: open it then.
        let create = AccountCommand::CreateGuild { name: "x".into(), icon: None }.op();
        assert!(s.account.begin(create));
        s.apply_account(AccountEvent::Done { op: create, result: Ok(Some(G2)) });
        assert_eq!(s.account.ops[&create], OpState::Done(Some(G2)));
        assert_eq!(s.selected_guild, None);
        let general = Channel { id: Id(21), kind: channel_type::GUILD_TEXT, ..Default::default() };
        let g2 = Guild { id: G2, owner_id: ME, channels: vec![general], ..Default::default() };
        s.apply_gateway(GatewayEvent::GuildCreate(Box::new(g2)), None);
        assert_eq!((s.selected_guild, s.selected_channel), (Some(G2), Some(Id(21))));

        // A channel known before its answer opens at once; a category is not opened.
        let cat = Channel { id: Id(22), kind: channel_type::GUILD_CATEGORY, guild_id: Some(G2), ..Default::default() };
        s.apply_gateway(GatewayEvent::ChannelCreate(Box::new(cat)), None);
        let op = AccountCommand::CreateChannel { guild_id: G2, name: "c".into(), kind: 4, parent_id: None }.op();
        s.apply_account(AccountEvent::Done { op, result: Ok(Some(Id(22))) });
        assert_eq!(s.selected_channel, Some(Id(21)));
        assert_eq!(s.account.select_on_create, None);

        for i in 0..MAX_OPS as u64 {
            assert!(s.account.begin(AccountCommand::DeleteRole { guild_id: G1, role_id: Id(i) }.op()));
        }
        assert!(!s.account.ops.contains_key(&leave) && !s.account.ops.contains_key(&create), "finished entries go first");
    }

    #[test]
    fn views_follow_answers_and_bans() {
        let mut s = state(&[G1]);
        let ban = |id: u64| Ban { reason: None, user: User { id: Id(id), ..Default::default() } };
        s.account.bans = Some(BansView::new(G1));
        let page: Vec<Ban> = (1..=BAN_PAGE as u64).map(ban).collect();
        s.apply_account(AccountEvent::BansLoaded { guild_id: G1, after: None, result: Ok(page) });
        let v = s.account.bans.as_mut().unwrap();
        assert_eq!((v.bans.len(), v.next_after()), (BAN_PAGE, Some(Id(BAN_PAGE as u64))));
        v.loading = true;
        s.apply_account(AccountEvent::BansLoaded { guild_id: G1, after: Some(Id(1000)), result: Ok(vec![ban(1000), ban(1001)]) });
        let v = s.account.bans.as_ref().unwrap();
        assert_eq!((v.bans.len(), v.has_more, v.next_after()), (BAN_PAGE + 1, false, None));
        s.apply_gateway(GatewayEvent::GuildBanRemove { guild_id: G1, user_id: Id(1) }, None);
        s.apply_gateway(GatewayEvent::GuildBanAdd { guild_id: G1, user: User { id: Id(5000), ..Default::default() } }, None);
        s.apply_gateway(GatewayEvent::GuildBanAdd { guild_id: G2, user: User { id: Id(6000), ..Default::default() } }, None);
        let ids: Vec<Id> = s.account.bans.as_ref().unwrap().bans.iter().map(|b| b.user.id).collect();
        assert_eq!((ids.len(), ids[0], ids[ids.len() - 1]), (BAN_PAGE + 1, Id(2), Id(5000)));

        // Stale answers are dropped; the invite modal keeps its result, others toast.
        assert_eq!(s.apply_account(AccountEvent::InvitesLoaded { guild_id: G1, result: Ok(Vec::new()) }), None);
        s.account.invites = Some(InvitesView::new(G1));
        s.apply_account(AccountEvent::InvitesLoaded { guild_id: G1, result: Err("x".into()) });
        assert_eq!(s.account.invites.as_ref().unwrap().error.as_deref(), Some("x"));
        s.account.created_invite = Some(CreatedInvite { channel_id: Id(5), result: None });
        assert_eq!(s.apply_account(AccountEvent::InviteCreated { channel_id: Id(6), result: Err("e".into()) }), Some("e".into()));
        s.apply_account(AccountEvent::InviteCreated { channel_id: Id(5), result: Ok(None) });
        assert!(matches!(s.account.created_invite, Some(CreatedInvite { result: Some(Ok(None)), .. })));
    }

    #[test]
    fn invite_previews_are_cached_and_bounded() {
        let mut s = state(&[]);
        let t0 = Instant::now();
        assert!(s.request_invite("abc", t0));
        assert!(!s.request_invite("abc", t0), "already loading");
        let invite = Invite { code: "abc".into(), ..Default::default() };
        s.apply_account(AccountEvent::InviteResolved { code: "abc".into(), result: Ok(invite) });
        assert!(!s.request_invite("abc", t0 + Duration::from_secs(60)));
        assert!(s.invite_preview("abc").is_some_and(|p| matches!(p.result, Some(Ok(_)))));
        assert!(s.request_invite("abc", t0 + INVITE_TTL));
        for i in 0..INVITE_CACHE * 2 {
            s.request_invite(&format!("c{i}"), t0);
        }
        assert_eq!(s.account.invite_previews.len(), INVITE_CACHE);
    }

    #[test]
    fn mark_guild_read_acks_only_unread_channels_of_the_guild() {
        let mut s = state(&[G1]);
        s.guilds[0].owner_id = ME;
        let ch = |id: u64, guild: Id, last: u64| Channel { id: Id(id), kind: 0, guild_id: Some(guild), last_message_id: Some(Id(last)), ..Default::default() };
        for c in [ch(1, G1, 50), ch(2, G1, 50), ch(3, G1, 50), ch(4, G2, 50)] {
            s.channels.insert(c.id, c);
        }
        let rs = |id: u64, read: u64, mentions: u32| ReadState { id: Id(id), last_message_id: Some(Id(read)), mention_count: mentions, read_state_type: 0 };
        for r in [rs(1, 40, 0), rs(2, 50, 0), rs(3, 50, 2), rs(4, 40, 0)] {
            s.read_states.insert(r.id, r);
        }
        let mut acks = s.mark_guild_read(G1);
        acks.sort();
        assert_eq!(acks, vec![(Id(1), Id(50)), (Id(3), Id(50))]);
        assert_eq!(s.mention_count(Id(3)), 0);
        assert!(s.mark_guild_read(G1).is_empty());
    }

    #[test]
    fn role_hierarchy_gates() {
        let mut s = state(&[G1]);
        let role = |id: u64, position: i32, perms: u64| Role { id: Id(id), position, permissions: Perms(perms), ..Default::default() };
        s.guilds[0].roles = vec![role(10, 0, 0), role(100, 3, perm::MANAGE_ROLES), role(101, 2, 0), role(102, 3, 0), role(103, 5, 0)];
        s.my_members.insert(G1, Member { roles: vec![Id(100)], ..Default::default() });
        assert_eq!(s.guild_permissions(G1), perm::MANAGE_ROLES);
        assert!(s.can_manage_role(G1, Id(101)));
        assert!(!s.can_manage_role(G1, Id(100)), "my own top role");
        assert!(s.can_manage_role(G1, Id(102)), "same position: the lower id ranks higher");
        assert!(!s.can_manage_role(G1, Id(103)));
        assert!(s.outranks(G1, Id(7), &[Id(101)]));
        assert!(s.outranks(G1, Id(7), &[]));
        assert!(!s.outranks(G1, Id(7), &[Id(103)]));
        assert!(!s.outranks(G1, Id(99), &[]), "never the owner");
        assert!(!s.outranks(G1, ME, &[]), "never myself");
        s.guilds[0].owner_id = ME;
        assert!(s.can_manage_role(G1, Id(103)) && s.outranks(G1, Id(7), &[Id(103)]));
    }
}
