# Account & server design (settings proto, profile, server admin, invites, notifications)

Protocol facts: `docs/FEATURES_NOTES.md` Part 3 and §0 (authoritative). Same pattern as
`docs/SOCIAL_DESIGN.md`: one nested command/event pair, one state submodule. This page fixes
the split between the engineers. Status: **backend complete** (contract, `handle_account`,
`state/account.rs` + `admin.rs`, REST calls, proto codec, gateway decoding; unit-tested, never
sent to live Discord). Only the UI is missing: see the hook-up guide at the end.

## Data flow
```
UI ──Command::Account(AccountCommand)──► backend::run ──► handle_account (backend.rs, done)
     SetPersistentStatus ──► the settings PATCH (the UI sends ChatCommand::SetStatus, op 3, just before)
     settings proto  api/account.rs: GET settings-proto/1 ─► settings_proto::with_* ─► PATCH (only field 11 or 14)
     profile         api/account.rs: PATCH /users/@me, /users/@me/profile (images: image_data_uri)
     server admin, invites, notification settings: api/guild_admin.rs
        ──► AccountEvent::{Done, InviteCreated, InviteResolved, BansLoaded, InvitesLoaded}
gateway: READY user_settings_proto, USER_SETTINGS_PROTO_UPDATE ─► settings_proto::decode ─► GatewayEvent::UserSettingsProto
         GUILD_BAN_ADD / _REMOVE ─► GatewayEvent::{GuildBanAdd, GuildBanRemove}
         already handled: USER_UPDATE (refreshes `me`), GUILD_*, CHANNEL_*, GUILD_ROLE_*, GUILD_EMOJIS_UPDATE,
         USER_GUILD_SETTINGS_UPDATE, USER_SETTINGS_UPDATE (+ the op 3 follow in conn.rs)
         ──► AppState::apply_gateway (hooks: on_account_gateway, on_account_created)
UI drain_events: BackendEvent::Account(ev) ──► AppState::apply_account (state/account.rs) ──► toast?
```
Logged out, `rest` answers at once with "Not connected" (writes as a `Done` error, reads with
their typed event).

## Contract (additive only; ask the contract owner before changing)
- `backend.rs`: `Command::Account`, `AccountCommand` (+ `op()`), `AccountEvent`,
  `BackendEvent::Account`, `AccountOp`, `OpKind`, `GuildPatch`, `ChannelPatch`,
  `ChannelPosition`, `RolePatch`, `NotificationPatch`; `handle_account`, `done`, `created` (done).
- `model.rs`: `GuildFolder`, `StatusSettings`, `UserSettingsProto`, `Invite`, `Ban`, the new
  `perm` bits (CREATE_INSTANT_INVITE, KICK/BAN_MEMBERS, MANAGE_GUILD, VIEW_AUDIT_LOG,
  CHANGE/MANAGE_NICKNAMES, MANAGE_ROLES, MANAGE/CREATE_GUILD_EXPRESSIONS, MODERATE_MEMBERS),
  `GatewayEvent::{UserSettingsProto, GuildBanAdd, GuildBanRemove}`.
- `api/settings_proto.rs`: `decode`, `with_status`, `with_custom_status`, `with_guild_folders`
  (base64 in, base64 out).
- `api/account.rs`: `ApiClient::{save_status, save_custom_status, save_guild_folders,
  set_global_name, set_avatar, set_banner, set_bio, set_pronouns, set_accent_color,
  set_username}`, `image_data_uri`, `ImageKind`, `MAX_IMAGE_FILE`.
- `api/guild_admin.rs`: `ApiClient::{create_guild, leave_guild, update_guild, create_channel,
  update_channel, delete_channel, reorder_channels, set_overwrite, delete_overwrite,
  create_role, update_role, delete_role, reorder_roles, member_role, set_nick, kick, ban, unban,
  bans, upload_emoji, delete_emoji, create_invite, resolve_invite, accept_invite, delete_invite,
  guild_invites, update_notifications}`, `audit_reason`, `invite_code` (link -> code).
- `state/account.rs`: `AccountState` (+ `begin`), `SettingsView`, `OpState`, `BansView`,
  `InvitesView`, `CreatedInvite`, `InvitePreview`, `ServerBarItem`, `GuildDrop`, `INVITE_CACHE`,
  `INVITE_TTL`, `BAN_PAGE`, `AppState::{apply_account, request_invite, invite_preview,
  server_bar, move_guild, mark_guild_read, guild_permissions, can_manage_role, outranks}`;
  `state/account/admin.rs`: `AppState::account_check(&AccountCommand) -> Result<(), &str>`.

Routes, bodies, headers and error codes are in each call's doc comment.

## File ownership
| Owner | Files | Status |
|---|---|---|
| contract owner | `backend.rs` (enums, `handle_account`), `model.rs`, `state/account.rs` + its hooks in `state.rs` (`account` field, `on_account_gateway` arm, `on_account_created` in the GUILD_CREATE / CHANNEL_CREATE arms), the `BackendEvent::Account` arm in `ui/mod.rs`, the placeholder files below, this page | done, 9 unit tests |
| settings engineer | `api/settings_proto.rs` (the codec), `api/account.rs` (settings-proto calls with the one `out_of_date` retry, profile calls, `image_data_uri`), new `gateway/account.rs` (USER_SETTINGS_PROTO_UPDATE type 1 -> `UserSettingsProto { partial }`; GUILD_BAN_ADD / _REMOVE `{guild_id, user}` -> `GuildBanAdd` / `GuildBanRemove`; an `EVENTS` list + one guard arm in `normalize::dispatch`, like `gateway/social.rs`), READY `user_settings_proto` (`ReadyWire` field, decoded into `normalize::Ready`, and `conn.rs` sends `UserSettingsProto { partial: false }` right after `Ready`; a bad blob is logged and skipped, never fatal) | done, 12 unit tests (`with_status` also drops 11.4 `status_expires_at_ms`, so an old status duration cannot end the new status) |
| guild-admin engineer | `api/guild_admin.rs` (all calls, `audit_reason`, `invite_code`, §0.4 codes -> text), `state/account/admin.rs` (`account_check`) | done, 12 unit tests |

Each engineer removes the `#![allow(unused_variables, …)]` / `dead_code` markers of their file
once the bodies are in. Private helpers and `impl ApiClient` blocks inside one's own file are
fine. `guild_admin.rs` calls `account::image_data_uri` (guild icon, emoji) through its
signature only. Tests the codec must have: the three §3.1.3 vectors both ways; an edit keeps
unknown 11.x / 14.x subfields byte for byte and drops every other top-level field; packed and
unpacked `repeated fixed64`; groups and truncated input -> `Err`.

## Per-area rules
- **Every request is one user action.** No bulk, no queue, no timer; no automatic retry of a
  4xx; 429 goes through the limiter and long waits (channel rename, username) are reported,
  never queued. Captcha -> "do it in the official app" (already in `ApiClient`'s error mapping;
  `accept_invite` adds the invite link). 401/60003 -> `ApiError::MfaRequired`: text, no logout
  (the re-prompt flow is a later block).
- **Settings proto** (§3.1, §3.9): GET, edit one top-level field with the codec (unknown
  subfields kept), PATCH only that field; `out_of_date: true` -> one re-GET + re-PATCH, then
  an error. This block uses the proto store although §3.1.1 calls legacy JSON the safe default:
  the codec keeping unknown fields is what makes it safe. Status: op 3 first (the picker sends
  `ChatCommand::SetStatus`), then `SetPersistentStatus` PATCHes. Custom status: PATCH only; the
  USER_SETTINGS_UPDATE echo makes `conn.rs` re-send op 3 with it (`on_status` follow). Folders:
  the whole list.
- **Profile** (§3.2): name, avatar, username via `/users/@me`; bio, pronouns, accent colour,
  banner via `/users/@me/profile`. Images only from paths the user picked; refuse files over
  `MAX_IMAGE_FILE` (10 MiB) before reading; decode, crop and resize on a blocking thread; PNG
  data URI; emoji ≤ 256 KiB. The password stays an `auth::Secret`, exposed only into the body.
- **Server admin** (§3.3-§3.7): destructive commands only after a UI confirmation; `reason` ->
  `x-audit-log-reason` through `send_with`'s extra headers (`audit_reason`: percent-encoded,
  ≤ 512 chars). Results arrive as gateway dispatches. Gate every button locally to avoid 403s
  (they count toward the Cloudflare budget): `account_check(&cmd)` before showing or sending
  any admin command (built on `guild_permissions`, `channel_permissions`, `can_manage_role`,
  `outranks`; `Err` is the text for the disabled control).
- **Invites** (§3.8): resolve only invites on screen, cached (`request_invite`); join only on
  click + confirm. INVITE_* dispatches never reach users: refetch lists after a change.
  `route_key` already maps codes to `{code}`.
- **Notifications** (§3.10): partial bodies; channel overrides as a map keyed by channel id.
  The UI coalesces rapid toggles (~500 ms).
- **Mark server read** (§3.11): `mark_guild_read` + the existing `SocialCommand::MarkAllRead`
  (ack-bulk, chunks of 100). No new command.

## Memory bounds (state/account.rs)
Action progress: finished entries are dropped once 32 are kept (pending ones stay until their
answer; every command is answered). Invite previews: LRU of 64, TTL 10 min. Bans view ≤ 10,000.
One view each (bans, invites, created invite). Settings: one decoded view.

## Notes for the UI block
- **Progress of a write:** `let cmd = AccountCommand::X { .. }; let op = cmd.op();
  state.account.begin(op); tx.send(Command::Account(cmd));`, then show
  `state.account.ops.get(&op)` (`Pending` / `Done(id)` / `Failed(text)`) and remove the entry
  when the form closes. Without `begin`, a failure becomes a toast (right for context-menu
  actions).
- **Created things open themselves:** `CreateGuild` and `AcceptInvite` select the guild once
  its GUILD_CREATE arrives (at once if already known), `CreateChannel` the channel (text-like
  kinds only).
- **Server bar:** render `server_bar()` instead of the `guilds` order; a folder's `name` None =
  the guild names joined with ", ".
- **Status:** `chat.my_status` stays the effective status (dot); `account.settings.status` is
  what is saved.
- Not covered yet: the 2FA re-prompt (`POST /mfa/finish`), guild `mfa_level` gating (not in the
  model), member timeout, deleting a guild, templates, password/email change, username
  availability (`pomelo-attempt`), recent avatars, per-server profiles, category collapse sync
  (`channel_overrides.collapsed`), status durations (`status_expires_at_ms`),
  USER_REQUIRED_ACTION_UPDATE, animated avatars/emoji (flattened to PNG), the gateway
  `session_id` in `accept_invite`. An admin member list over REST is not usable by users: use
  the op 37 list and `ChatCommand::SearchMembers`.
- UNCONFIRMED: that a settings-proto PATCH also fires the legacy USER_SETTINGS_UPDATE (DPS says
  both fire). If it does not, the custom status reaches others only after the next op 3; then
  add a gateway command that sets the custom status before its op 3.

## UI hook-up guide
Commands are `Command::Account(AccountCommand::X)`; answers arrive as `BackendEvent::Account`
and are already applied in `ui/mod.rs` `drain_events` (`apply_account`, toast on `Some`).
Gateway events are applied by `apply_gateway`. The UI only sends commands and reads state; the
one `api` item it calls is the pure `api::guild_admin::invite_code(link)`.

Every admin control, one pattern (the "gate" notes in the table are what `account_check`
checks; it passes commands that need no permission):
```
let cmd = AccountCommand::KickMember { guild_id, user_id, reason };
let allowed = state.account_check(&cmd);          // local, no request
// Err(why): disabled control, `why` as hover text. Ok: on click (after the confirm dialog
// for leave / delete / kick / ban):
state.account.begin(cmd.op());                     // only where a form shows progress
tx.send(Command::Account(cmd));
```
Build the command with what the control would send: for `SetPermissionOverwrite`,
`CreateRole` and `UpdateRole` the permission bits are checked ("only bits I have").

Input checks run in the backend before anything is sent and come back as `Done(Err(text))`
like server errors, so a form only shows `account.ops[&op]`. Caps worth mirroring in the text
fields: display name 32, About Me 190, pronouns 40, custom status 128, username 2-32, server
name 2-100, channel name 1-100, nickname 32, server description 300, emoji name
`[A-Za-z0-9_]{2,32}`. Images: a path picked with `rfd`; the backend refuses files over 10 MiB
(`MAX_IMAGE_FILE`) and non-images, then crops to fill 512×512 (avatar, server icon), 1024×409
(banner) or 128×128 (emoji, ≤ 256 KiB) as PNG; animated files become their first frame.

| Feature | UI sends | Comes back as | UI reads |
|---|---|---|---|
| Status picker | `ChatCommand::SetStatus { status }` (op 3), then `SetPersistentStatus { status }` (PATCH only) | `MyStatus` after the op 3; `UserSettingsProto` | `chat.my_status` (dot), `account.settings.status.status` |
| Custom status dialog | `SetCustomStatus { text, emoji, expires_at_ms }` (presets 30 min / 1 h / 4 h / today / never; clear = both None) | `UserSettingsProto`, then `MyStatus` | `account.settings.status.custom_status`, `.custom_status_expires_at_ms` |
| Server bar | nothing | `UserSettingsProto` | `server_bar()` |
| Drag a guild / make a folder | `let f = state.move_guild(g, GuildDrop::Top(i) \| Folder { id, index } \| NewFolder { with });` `state.account.settings.folders = f.clone();` then, debounced 1-2 s, `SetGuildFolders { folders: f }` | `UserSettingsProto` | `server_bar()` |
| Rename / recolour / ungroup a folder | edit a clone of `account.settings.folders` (ungroup = one `id: None` entry per guild), then as above | same | same |
| Display name, avatar, banner, About Me, pronouns, banner colour | `UpdateGlobalName { name }`, `UpdateAvatar { path }` (rfd picker; None = remove), `UpdateBanner`, `UpdateBio`, `UpdatePronouns`, `UpdateAccentColor`, each with `begin(op)` | `Done`; USER_UPDATE refreshes `me` | `account.ops[&op]`, `state.me` |
| Username | `UpdateUsername { username, password: Secret }` | `Done` (wrong password, taken, 2FA text) | `account.ops[&op]` |
| Create server | `CreateGuild { name, icon }` | `Done(Some(g))`; GUILD_CREATE opens it | `account.ops[&op]` |
| Invite embed / join dialog | code from a link: `api::guild_admin::invite_code(url)`; while shown: `if state.request_invite(code, now) { ResolveInvite { code } }`; on confirm: `AcceptInvite { code, guild_id, channel_id, channel_type }` from the preview | `InviteResolved`; `Done(Some(g))` or the captcha text | `invite_preview(code)`: `loading`, `result` (guild, channel, inviter, counts, `expires_at`) |
| Leave server | confirm, then `LeaveGuild { guild_id }` (hidden for the owner) | GUILD_DELETE; `Done` | – |
| Server overview | `UpdateGuild { guild_id, patch }` (gate `MANAGE_GUILD`) | GUILD_UPDATE; `Done` | `guild(g)` |
| Channels | `CreateChannel`, `UpdateChannel`, `DeleteChannel` (confirm), `ReorderChannels` (whole bucket), `SetPermissionOverwrite` / `DeletePermissionOverwrite` (gate `MANAGE_CHANNELS` / `MANAGE_ROLES` via `channel_permissions`) | CHANNEL_*; `Done` (`Some(id)` on create) | `channels`, `sidebar_channels(g)` |
| Roles | `CreateRole`, `UpdateRole`, `DeleteRole` (confirm), `ReorderRoles` (gate `can_manage_role`) | GUILD_ROLE_*; `Done(Some(role))` on create | `guild(g).roles` |
| Member roles, nicknames | `AddMemberRole` / `RemoveMemberRole` (`can_manage_role(role)` + `outranks`), `SetNick` (`MANAGE_NICKNAMES` + `outranks`), `SetMyNick` (`CHANGE_NICKNAME`) | GUILD_MEMBER_UPDATE (mine, subscribed members); `Done` | `find_member` |
| Kick / ban | confirm dialog with a reason (ban: delete-messages preset 0 / 3600 / 21600 / 43200 / 86400 / 259200 / 604800), then `KickMember` / `BanMember` (gate `KICK_MEMBERS` / `BAN_MEMBERS` + `outranks`) | GUILD_BAN_ADD; `Done` | – |
| Bans page | `account.bans = Some(BansView::new(g))`, `FetchBans { after: None }`; more: `if let Some(a) = v.next_after() { v.loading = true; FetchBans { after: Some(a) } }`; `Unban` | `BansLoaded`; GUILD_BAN_* keep it current | `BansView.bans/has_more/loading/error` |
| Emoji page | `UploadEmoji { name, path }`, `DeleteEmoji` (confirm; gate `CREATE_` / `MANAGE_GUILD_EXPRESSIONS`) | GUILD_EMOJIS_UPDATE; `Done` | `guild(g).emojis` |
| Invite modal | `account.created_invite = Some(CreatedInvite { channel_id, result: None })`, `CreateInvite { channel_id, max_age: 604800, max_uses: 0, temporary: false }` (gate `CREATE_INSTANT_INVITE`) | `InviteCreated` | `created_invite.result`: `Ok(Some(i))` -> `https://discord.gg/{i.code}`, `Ok(None)` = invites paused |
| Invites page | `account.invites = Some(InvitesView::new(g))`, `FetchGuildInvites`; `DeleteInvite { code }` (confirm), then refetch | `InvitesLoaded`; `Done` | `InvitesView.invites/loading/error` |
| Notification menus | `UpdateGuildNotifications { guild_id (None = DMs), patch }`, `UpdateChannelOverride { guild_id, channel_id, patch }` | USER_GUILD_SETTINGS_UPDATE | `guild_settings`, `is_guild_muted`, `is_channel_muted` |
| Mark server read | `let acks = state.mark_guild_read(g); if settings.send_read_acks && !acks.is_empty() { Command::Social(SocialCommand::MarkAllRead { acks }) }` | MESSAGE_ACKs | – |

Also for the UI:
- **Custom status expiry:** `account.settings.status.custom_status` is what is stored; hide it
  (and treat it as none in the dialog) when `custom_status_expires_at_ms` is in the past.
- **Profile editor prefill:** `User` has no `bio` / `pronouns`; when the editor opens send
  `SocialCommand::FetchProfile { user_id: me, guild_id: None }` and prefill from the loaded
  profile. Display name and avatar come from `state.me`.
- **Joins:** a group DM invite answers `Done(Some(channel id))` and opens once its
  CHANNEL_CREATE arrives. Captcha / rules screening come back as `Done(Err(text))` sending the
  user to the official app (the captcha text carries the invite link: make it selectable).
- **Not gated** (no guild id in the command): `DeleteInvite`; show it only on the invites
  page, which `FetchGuildInvites` gates. `DeleteEmoji` needs Manage Expressions (the uploader is
  not cached, so "delete my own emoji" with Create Expressions only stays hidden).
- **Never resend on error.** A long 429 wait (channel rename: 2 per 10 min; username) comes back
  as "You're doing that too fast" text; 2FA-moderated servers answer the 2FA text (no logout).
- Debounce folder drags 1-2 s and notification toggles ~500 ms; resolve only invites on screen.

Removing the `#[allow(dead_code, reason = "…not built yet")]` markers on `Command::Account`,
`AccountCommand`, `Invite`, `Ban`, `invite_code` and `state/account.rs` is part of the UI block
once they are used.
