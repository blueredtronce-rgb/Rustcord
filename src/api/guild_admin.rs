//! Server administration REST calls (FEATURES_NOTES §3.3-§3.8, §3.10): guild create/leave/edit,
//! channels, permission overwrites, roles, member nick/roles, kick/ban, emoji, invites and my
//! notification settings. Called from `backend::handle_account` (answers `AccountEvent::Done`,
//! `BansLoaded`, `InviteCreated`, `InviteResolved`, `InvitesLoaded`). Every call goes
//! through `ApiClient::send_coded` (rate limiter, 401 handling, `MfaRequired` for guilds
//! with 2FA moderation, captcha -> "do it in the official app"); none is retried on 4xx, and
//! the error codes of §0.4 become plain text (`readable`). Results show through the gateway
//! (GUILD_*, CHANNEL_*, GUILD_ROLE_*, GUILD_BAN_*, GUILD_EMOJIS_UPDATE, GUILD_MEMBER_UPDATE,
//! USER_GUILD_SETTINGS_UPDATE), so writes only report success or the error. Destructive calls
//! are only reached after the UI's confirmation and its local gate
//! (`AppState::account_check`). `reason` -> the `x-audit-log-reason` header (`audit_reason`).
//!
//! OWNERSHIP: guild-admin engineer (docs/ACCOUNT_SERVER_DESIGN.md). The signatures are the
//! contract.

use super::account::{ImageKind, image_data_uri};
use super::{ApiClient, ApiError, net_err};
use crate::backend::{ChannelPatch, ChannelPosition, GuildPatch, NotificationPatch, RolePatch};
use crate::model::{Ban, GuildProfile, Id, Invite, MemberVerification, Overwrite};
use base64::Engine as _;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::path::PathBuf;

const MAX_REASON_CHARS: usize = 512;
const MAX_BAN_DELETE_SECS: u32 = 604_800;
const MAX_INVITE_AGE: u32 = 5_184_000;
const MAX_INVITE_USES: u32 = 100;
const MAX_SLOWMODE: u32 = 21_600;
/// Channel types `POST /guilds/{g}/channels` takes: text, voice, category, announcement, stage, forum.
const CHANNEL_KINDS: [u8; 6] = [0, 2, 4, 5, 13, 15];
/// "Blank Server", the template the web client creates new servers from (DPS).
const BLANK_TEMPLATE: &str = "2TffvPucqHkN";
const INVALID_INVITE: &str = "This invite is invalid or has expired.";

/// `x-audit-log-reason` header for a non-blank reason: trimmed, ≤ 512 chars, percent-encoded
/// UTF-8 (so newlines and non-ASCII survive `extra_headers`).
pub fn audit_reason(reason: Option<&str>) -> Vec<(&'static str, String)> {
    let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) else { return Vec::new() };
    let r: String = r.chars().take(MAX_REASON_CHARS).collect();
    vec![("x-audit-log-reason", utf8_percent_encode(&r, NON_ALPHANUMERIC).to_string())]
}

/// The code of an invite link (`discord.gg/CODE`, `discord.com/invite/CODE`,
/// `discordapp.com/invite/CODE`, with or without `https://` / `www.`), as FEATURES_NOTES §3.8's
/// regex matches it; None for anything else. For invite embeds and the "join a server" field.
pub fn invite_code(link: &str) -> Option<&str> {
    let link = link.trim();
    let rest = link.strip_prefix("https://").or_else(|| link.strip_prefix("http://")).unwrap_or(link);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = ["discord.gg/", "discord.com/invite/", "discordapp.com/invite/"].iter().find_map(|p| rest.strip_prefix(p))?;
    let code = &path[..path.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(path.len())];
    valid_code(code).then_some(code)
}

impl ApiClient {
    /// `send_coded` with the §0.4 error codes in plain words.
    async fn send_admin(&self, method: Method, url: Url, body: Option<Value>, extra: &[(&'static str, String)]) -> Result<reqwest::Response, ApiError> {
        self.send_coded(method, url, body, extra).await.map_err(readable)
    }

    // ----- guilds (§3.3)

    /// `POST /guilds {"name","icon": data URI | null,"channels":[],"system_channel_id":null,
    /// "guild_template_code":"2TffvPucqHkN"}` (name 2-100; icon via
    /// `account::image_data_uri(.., GuildIcon)`) -> the new guild's id. 30001 -> readable text.
    pub async fn create_guild(&self, name: &str, icon: Option<PathBuf>) -> Result<Id, ApiError> {
        let name = guild_name(name)?;
        let icon = match icon {
            Some(path) => Some(image_data_uri(path, ImageKind::GuildIcon).await?),
            None => None,
        };
        let body = json!({
            "name": name,
            "icon": icon,
            "channels": [],
            "system_channel_id": null,
            "guild_template_code": BLANK_TEMPLATE,
        });
        id_of(self.send_admin(Method::POST, Self::url(&["guilds"]), Some(body), &[]).await?).await
    }

    /// `DELETE /users/@me/guilds/{g} {"lurking":false}` -> 204.
    pub async fn leave_guild(&self, guild_id: Id) -> Result<(), ApiError> {
        let url = Self::url(&["users", "@me", "guilds", &guild_id.to_string()]);
        self.send_admin(Method::DELETE, url, Some(json!({ "lurking": false })), &[]).await.map(drop)
    }

    /// `PATCH /guilds/{g}` with the set fields of `patch` (`icon` file -> data URI, `Some(None)`
    /// -> null). Nothing set -> no request. 20031 -> readable text.
    pub async fn update_guild(&self, guild_id: Id, patch: GuildPatch) -> Result<(), ApiError> {
        let mut body = guild_patch_body(&patch)?;
        if let Some(icon) = patch.icon {
            let uri = match icon {
                Some(path) => Some(image_data_uri(path, ImageKind::GuildIcon).await?),
                None => None,
            };
            body.insert("icon".into(), json!(uri));
        }
        if body.is_empty() {
            return Ok(());
        }
        let url = Self::url(&["guilds", &guild_id.to_string()]);
        self.send_admin(Method::PATCH, url, Some(body.into()), &[]).await.map(drop)
    }

    // ----- channels (§3.4)

    /// `POST /guilds/{g}/channels {"name","type","parent_id"?}` -> the new channel's id.
    /// 30013 / 50024 -> readable text.
    pub async fn create_channel(&self, guild_id: Id, name: &str, kind: u8, parent_id: Option<Id>) -> Result<Id, ApiError> {
        let body = channel_body(name, kind, parent_id)?;
        let url = Self::url(&["guilds", &guild_id.to_string(), "channels"]);
        id_of(self.send_admin(Method::POST, url, Some(body), &[]).await?).await
    }

    /// `PATCH /channels/{c}` with the set fields (`slowmode` -> `rate_limit_per_user`). A 429
    /// on name/topic edits is reported, never queued (the limiter gives up on long waits).
    pub async fn update_channel(&self, channel_id: Id, patch: &ChannelPatch) -> Result<(), ApiError> {
        let body = channel_patch_body(patch)?;
        if body.is_empty() {
            return Ok(());
        }
        self.send_admin(Method::PATCH, channel_url(channel_id), Some(body.into()), &[]).await.map(drop)
    }

    /// `DELETE /channels/{c}`. 50074 -> readable text.
    pub async fn delete_channel(&self, channel_id: Id) -> Result<(), ApiError> {
        self.send_admin(Method::DELETE, channel_url(channel_id), None, &[]).await.map(drop)
    }

    /// `PATCH /guilds/{g}/channels [{"id","position","parent_id"?,"lock_permissions"?}]` -> 204.
    pub async fn reorder_channels(&self, guild_id: Id, positions: &[ChannelPosition]) -> Result<(), ApiError> {
        if positions.is_empty() {
            return Ok(());
        }
        let url = Self::url(&["guilds", &guild_id.to_string(), "channels"]);
        self.send_admin(Method::PATCH, url, Some(channel_positions_body(positions)), &[]).await.map(drop)
    }

    /// `PUT /channels/{c}/permissions/{overwrite.id} {"type","allow","deny"}` (bitfields as
    /// strings) -> 204.
    pub async fn set_overwrite(&self, channel_id: Id, overwrite: &Overwrite) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "permissions", &overwrite.id.to_string()]);
        let body = json!({ "type": overwrite.kind, "allow": overwrite.allow, "deny": overwrite.deny });
        self.send_admin(Method::PUT, url, Some(body), &[]).await.map(drop)
    }

    /// `DELETE /channels/{c}/permissions/{target}` -> 204.
    pub async fn delete_overwrite(&self, channel_id: Id, target_id: Id) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "permissions", &target_id.to_string()]);
        self.send_admin(Method::DELETE, url, None, &[]).await.map(drop)
    }

    // ----- roles (§3.5)

    /// `POST /guilds/{g}/roles` with the set fields (`permissions` as a string, `color` as
    /// `colors.primary_color`) -> the new role's id. 30005 -> readable text.
    pub async fn create_role(&self, guild_id: Id, role: &RolePatch) -> Result<Id, ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "roles"]);
        id_of(self.send_admin(Method::POST, url, Some(role_body(role).into()), &[]).await?).await
    }

    /// `PATCH /guilds/{g}/roles/{r}` with the set fields. Nothing set -> no request.
    pub async fn update_role(&self, guild_id: Id, role_id: Id, patch: &RolePatch) -> Result<(), ApiError> {
        let body = role_body(patch);
        if body.is_empty() {
            return Ok(());
        }
        self.send_admin(Method::PATCH, role_url(guild_id, role_id), Some(body.into()), &[]).await.map(drop)
    }

    /// `DELETE /guilds/{g}/roles/{r}` -> 204.
    pub async fn delete_role(&self, guild_id: Id, role_id: Id) -> Result<(), ApiError> {
        self.send_admin(Method::DELETE, role_url(guild_id, role_id), None, &[]).await.map(drop)
    }

    /// `PATCH /guilds/{g}/roles [{"id","position"}]`.
    pub async fn reorder_roles(&self, guild_id: Id, positions: &[(Id, i32)]) -> Result<(), ApiError> {
        if positions.is_empty() {
            return Ok(());
        }
        let body: Vec<Value> = positions.iter().map(|(id, position)| json!({ "id": id, "position": position })).collect();
        let url = Self::url(&["guilds", &guild_id.to_string(), "roles"]);
        self.send_admin(Method::PATCH, url, Some(body.into()), &[]).await.map(drop)
    }

    /// `PUT` (`add`) / `DELETE /guilds/{g}/members/{u}/roles/{r}` -> 204.
    pub async fn member_role(&self, guild_id: Id, user_id: Id, role_id: Id, add: bool) -> Result<(), ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "members", &user_id.to_string(), "roles", &role_id.to_string()]);
        let method = if add { Method::PUT } else { Method::DELETE };
        self.send_admin(method, url, None, &[]).await.map(drop)
    }

    // ----- members and moderation (§3.6)

    /// `PATCH /guilds/{g}/members/{u | @me} {"nick": nick | null}` (`user_id` None = me; blank
    /// -> null; ≤ 32 chars).
    pub async fn set_nick(&self, guild_id: Id, user_id: Option<Id>, nick: Option<&str>) -> Result<(), ApiError> {
        let nick = nick.map(str::trim).filter(|n| !n.is_empty());
        if nick.is_some_and(|n| n.chars().count() > 32) {
            return Err(local("Nicknames can be at most 32 characters long."));
        }
        let who = user_id.map_or_else(|| "@me".to_owned(), |u| u.to_string());
        let url = Self::url(&["guilds", &guild_id.to_string(), "members", &who]);
        self.send_admin(Method::PATCH, url, Some(json!({ "nick": nick })), &[]).await.map(drop)
    }

    /// `DELETE /guilds/{g}/members/{u}` + `audit_reason` -> 204.
    pub async fn kick(&self, guild_id: Id, user_id: Id, reason: Option<&str>) -> Result<(), ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "members", &user_id.to_string()]);
        self.send_admin(Method::DELETE, url, None, &audit_reason(reason)).await.map(drop)
    }

    /// `PUT /guilds/{g}/bans/{u} {"delete_message_seconds"}` (clamped to 0..=604800) +
    /// `audit_reason` -> 204. 30035 -> readable text.
    pub async fn ban(&self, guild_id: Id, user_id: Id, reason: Option<&str>, delete_message_seconds: u32) -> Result<(), ApiError> {
        let body = json!({ "delete_message_seconds": delete_message_seconds.min(MAX_BAN_DELETE_SECS) });
        self.send_admin(Method::PUT, ban_url(guild_id, user_id), Some(body), &audit_reason(reason)).await.map(drop)
    }

    /// `DELETE /guilds/{g}/bans/{u}` -> 204.
    pub async fn unban(&self, guild_id: Id, user_id: Id) -> Result<(), ApiError> {
        self.send_admin(Method::DELETE, ban_url(guild_id, user_id), None, &[]).await.map(drop)
    }

    /// `GET /guilds/{g}/bans?limit=1000[&after=<user id>]` (BAN_MEMBERS), as received.
    pub async fn bans(&self, guild_id: Id, after: Option<Id>) -> Result<Vec<Ban>, ApiError> {
        let mut url = Self::url(&["guilds", &guild_id.to_string(), "bans"]);
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("limit", "1000"); // = state::account::BAN_PAGE
            if let Some(a) = after {
                q.append_pair("after", &a.to_string());
            }
        }
        json_of(self.send_admin(Method::GET, url, None, &[]).await?).await
    }

    // ----- emoji (§3.7). Per-guild special limits: uploads are one user action each; the
    // limiter serializes them per guild bucket and a 429 waits there.

    /// `POST /guilds/{g}/emojis {"name","image": image_data_uri(path, Emoji),"roles":[]}`;
    /// `name` checked against `[A-Za-z0-9_]{2,32}` first. 30008 / 30018 / 50045 -> readable
    /// text.
    pub async fn upload_emoji(&self, guild_id: Id, name: &str, path: PathBuf) -> Result<(), ApiError> {
        if !valid_emoji_name(name) {
            return Err(local("Emoji names are 2-32 letters, digits or underscores."));
        }
        let image = image_data_uri(path, ImageKind::Emoji).await?;
        let url = Self::url(&["guilds", &guild_id.to_string(), "emojis"]);
        let body = json!({ "name": name, "image": image, "roles": [] });
        self.send_admin(Method::POST, url, Some(body), &[]).await.map(drop)
    }

    /// `DELETE /guilds/{g}/emojis/{e}` -> 204.
    pub async fn delete_emoji(&self, guild_id: Id, emoji_id: Id) -> Result<(), ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "emojis", &emoji_id.to_string()]);
        self.send_admin(Method::DELETE, url, None, &[]).await.map(drop)
    }

    // ----- invites (§3.8). Codes are free-form path segments: `route_key` maps them to `{code}`.

    /// `POST /channels/{c}/invites {"max_age","max_uses","temporary","flags":0,"target_type":null}`
    /// with `X-Context-Properties` `{"location":"Guild Header"}` (CREATE_INSTANT_INVITE).
    /// 204 -> `Ok(None)` (invites paused); 30016 -> readable text.
    pub async fn create_invite(&self, channel_id: Id, max_age: u32, max_uses: u32, temporary: bool) -> Result<Option<Invite>, ApiError> {
        let body = json!({
            "max_age": max_age.min(MAX_INVITE_AGE),
            "max_uses": max_uses.min(MAX_INVITE_USES),
            "temporary": temporary,
            "flags": 0,
            "target_type": null,
        });
        let url = Self::url(&["channels", &channel_id.to_string(), "invites"]);
        let ctx = [("x-context-properties", context(r#"{"location":"Guild Header"}"#))];
        let resp = self.send_admin(Method::POST, url, Some(body), &ctx).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        json_of(resp).await.map(Some)
    }

    /// `GET /invites/{code}?with_counts=true&with_expiration=true`; 404/10006 -> "This invite
    /// is invalid or has expired."
    pub async fn resolve_invite(&self, code: &str) -> Result<Invite, ApiError> {
        let mut url = invite_url(code)?;
        url.query_pairs_mut().append_pair("with_counts", "true").append_pair("with_expiration", "true");
        json_of(self.send_coded(Method::GET, url, None, &[]).await.map_err(invite_readable)?).await
    }

    /// `POST /invites/{code} {}` with `X-Context-Properties` `{"location":"Accept Invite Page",
    /// "location_guild_id","location_channel_id","location_channel_type"}` (ids left out when
    /// None) -> the joined guild's id (`guild.id` / `guild_id` of the answer; a group DM
    /// invite: the channel's id). A captcha answer -> "Join this server in the official app:
    /// https://discord.com/invite/{code}" (never solved or retried); 40007 banned, 30001 too
    /// many servers, `show_verification_form` (rules screening pending) -> "accept the rules in
    /// the official app".
    // shortcut: no gateway `session_id` in the body (the backend does not know it); add it if
    // joins fail without it.
    pub async fn accept_invite(
        &self,
        code: &str,
        guild_id: Option<Id>,
        channel_id: Option<Id>,
        channel_type: Option<u8>,
    ) -> Result<Id, ApiError> {
        let url = invite_url(code)?;
        let ctx = [("x-context-properties", context(&accept_location(guild_id, channel_id, channel_type)))];
        let resp = match self.send_coded(Method::POST, url, Some(json!({})), &ctx).await {
            Ok(r) => r,
            // Discord's own 400s carry a `code`; a captcha demand ("update your app", hCaptcha)
            // has none (`error_from`).
            Err((ApiError::Http { status: 400, .. }, None)) => {
                return Err(local(&format!("Join this server in the official app: https://discord.com/invite/{code}")));
            }
            Err(e) => return Err(invite_readable(e)),
        };
        joined_id(json_of(resp).await?)
    }

    /// `DELETE /invites/{code}` (MANAGE_CHANNELS / MANAGE_GUILD).
    pub async fn delete_invite(&self, code: &str) -> Result<(), ApiError> {
        self.send_coded(Method::DELETE, invite_url(code)?, None, &[]).await.map(drop).map_err(invite_readable)
    }

    /// `GET /guilds/{g}/invites` (MANAGE_GUILD), as received.
    pub async fn guild_invites(&self, guild_id: Id) -> Result<Vec<Invite>, ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "invites"]);
        json_of(self.send_admin(Method::GET, url, None, &[]).await?).await
    }

    // ----- server tag card (docs/GUILD_CARD_NOTES.md). Each call follows a click on the card.

    /// `GET /guilds/{g}/profile`; 403 / 404 (a private or unknown server) -> `Ok(None)`.
    pub async fn guild_profile(&self, guild_id: Id) -> Result<Option<GuildProfile>, ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "profile"]);
        match self.send_coded(Method::GET, url, None, &[]).await {
            Ok(resp) => json_of(resp).await.map(Some),
            Err(e) => private_profile(e),
        }
    }

    /// `PUT /guilds/{g}/members/@me?lurker=false {}` (discoverable servers): 200 with the guild,
    /// or 204 when I am in it already. A captcha answer -> "join it in the official app".
    // shortcut: no `X-Context-Properties` / discovery ids (like `accept_invite`); add them if
    // Discord asks for captchas more often without them.
    pub async fn join_guild(&self, guild_id: Id) -> Result<(), ApiError> {
        let mut url = Self::url(&["guilds", &guild_id.to_string(), "members", "@me"]);
        url.query_pairs_mut().append_pair("lurker", "false");
        self.send_coded(Method::PUT, url, Some(json!({})), &[]).await.map(drop).map_err(join_readable)
    }

    /// `GET /guilds/{g}/member-verification` (rules and application questions, `with_guild`
    /// left out = false).
    pub async fn member_verification(&self, guild_id: Id) -> Result<MemberVerification, ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "member-verification"]);
        json_of(self.send_coded(Method::GET, url, None, &[]).await.map_err(join_readable)?).await
    }

    /// `PUT /guilds/{g}/requests/@me {"version","form_fields"}` (every field as received plus its
    /// `response`) -> the join request's `application_status` (`APPROVED` at once for a
    /// rules-only form, else `SUBMITTED`).
    pub async fn submit_join_request(&self, guild_id: Id, version: Option<String>, form_fields: Vec<Value>) -> Result<String, ApiError> {
        let url = Self::url(&["guilds", &guild_id.to_string(), "requests", "@me"]);
        let body = json!({ "version": version, "form_fields": form_fields });
        let w: JoinRequestWire = json_of(self.send_coded(Method::PUT, url, Some(body), &[]).await.map_err(join_readable)?).await?;
        Ok(w.application_status)
    }

    // ----- notification settings (§3.10). USER_GUILD_SETTINGS_UPDATE carries the result.

    /// `PATCH /users/@me/guilds/{g|@me}/settings` with only the set fields: `muted`,
    /// `mute_config {"selected_time_window": secs | -1, "end_time": ISO-8601 | null}` (only when
    /// muting), `message_notifications`, `suppress_everyone`, `suppress_roles`. With
    /// `channel_id` the same fields (minus `suppress_*`) go into
    /// `{"channel_overrides":{"<c>":{…}}}` (a map, unlike the READ shape). Nothing set -> no
    /// request.
    pub async fn update_notifications(&self, guild_id: Option<Id>, channel_id: Option<Id>, patch: &NotificationPatch) -> Result<(), ApiError> {
        let Some(body) = notification_body(channel_id, patch, chrono::Utc::now()) else { return Ok(()) };
        let guild = guild_id.map_or_else(|| "@me".to_owned(), |g| g.to_string());
        let url = Self::url(&["users", "@me", "guilds", &guild, "settings"]);
        self.send_admin(Method::PATCH, url, Some(body), &[]).await.map(drop)
    }
}

fn local(message: &str) -> ApiError {
    ApiError::Http { status: 0, message: message.into() }
}

/// Plain text for the server-admin error codes (FEATURES_NOTES §0.4); anything else (a
/// captcha answer, 403 "Missing Permissions", 50035 field errors) stays as `error_from` made it.
fn readable((err, code): (ApiError, Option<u64>)) -> ApiError {
    let text = match code {
        Some(10006) => INVALID_INVITE,
        Some(20028 | 20029) => "You're doing that too often. Wait a few minutes and try again.",
        Some(20031) => "That contains words Discord doesn't allow. Pick something else.",
        Some(30001) => "You've reached the server limit (100, or 200 with Nitro). Leave a server first.",
        Some(30005) => "This server already has the maximum of 250 roles.",
        Some(30008) => "This server has no free emoji slots left.",
        Some(30013) => "This server has reached its channel limit (500, or 50 per category).",
        Some(30016) => "This server already has the maximum of 1,000 invites. Delete some first.",
        Some(30018) => "This server has no free animated emoji slots left.",
        Some(30035) => "This server has reached its limit of bans for people who aren't members.",
        Some(40007) => "You're banned from this server.",
        Some(50024) => "That can't be done with this type of channel.",
        Some(50045) => "The image is too large.",
        Some(50046) => "Discord couldn't read that image.",
        Some(50074) => "Community servers need this channel; it can't be deleted.",
        _ => return err,
    };
    local(text)
}

/// A guild profile Discord won't show (403: not discoverable, not public; 404 / 10004: gone) is
/// the card's private state, not an error.
fn private_profile(e: (ApiError, Option<u64>)) -> Result<Option<GuildProfile>, ApiError> {
    match e {
        (ApiError::Forbidden(_) | ApiError::NotFound, _) | (_, Some(10004)) => Ok(None),
        e => Err(readable(e)),
    }
}

/// Joins and join requests in plain words: a 400 without a code is a captcha (never solved),
/// 403 an invite-only server, 404 / 10004 a server that is gone; the rest as `readable`.
// UNCONFIRMED: the codes for a full server, an age gate and the join-request cooldown; they keep
// Discord's own message.
fn join_readable(e: (ApiError, Option<u64>)) -> ApiError {
    match e {
        (ApiError::Http { status: 400, .. }, None) => local("Discord wants a captcha before you join. Join this server in the official app."),
        (ApiError::Forbidden(_), _) => local("This server only takes new members by invite."),
        (ApiError::NotFound, _) | (_, Some(10004)) => local("This server isn't available."),
        e => readable(e),
    }
}

/// `PUT /guilds/{g}/requests/@me` answer (a partial join request).
#[derive(Deserialize, Default)]
#[serde(default)]
struct JoinRequestWire {
    application_status: String,
}

/// `readable`, and 404 means the invite is gone.
fn invite_readable(e: (ApiError, Option<u64>)) -> ApiError {
    match e {
        (ApiError::NotFound, _) => local(INVALID_INVITE),
        e => readable(e),
    }
}

async fn json_of<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, ApiError> {
    let bytes = resp.bytes().await.map_err(net_err)?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HasId {
    id: Id,
}

/// The `id` of a created guild / channel / role.
async fn id_of(resp: reqwest::Response) -> Result<Id, ApiError> {
    let w: HasId = json_of(resp).await?;
    if w.id.is_zero() { Err(ApiError::Decode("no id in the answer".into())) } else { Ok(w.id) }
}

/// `POST /invites/{code}` answer: the invite plus `new_member` / `show_verification_form`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct JoinedWire {
    guild_id: Option<Id>,
    guild: Option<HasId>,
    channel: Option<HasId>,
    show_verification_form: Option<bool>,
}

fn joined_id(w: JoinedWire) -> Result<Id, ApiError> {
    if w.show_verification_form == Some(true) {
        return Err(local("You joined, but this server wants you to accept its rules first. Do that in the official app."));
    }
    [w.guild_id, w.guild.map(|g| g.id), w.channel.map(|c| c.id)]
        .into_iter()
        .flatten()
        .find(|id| !id.is_zero())
        .ok_or_else(|| ApiError::Decode("no server in the invite answer".into()))
}

/// Invite codes are letters, digits and dashes (vanity codes included).
fn valid_code(code: &str) -> bool {
    (1..=64).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn invite_url(code: &str) -> Result<Url, ApiError> {
    if valid_code(code) { Ok(ApiClient::url(&["invites", code])) } else { Err(local(INVALID_INVITE)) }
}

/// `X-Context-Properties`: standard base64 of the compact location JSON (§0.3).
fn context(location_json: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(location_json)
}

/// Location JSON of a join, keys in the web client's order (serde_json would sort them).
fn accept_location(guild_id: Option<Id>, channel_id: Option<Id>, channel_type: Option<u8>) -> String {
    let mut s = String::from(r#"{"location":"Accept Invite Page""#);
    if let Some(g) = guild_id {
        s += &format!(r#","location_guild_id":"{g}""#);
    }
    if let Some(c) = channel_id {
        s += &format!(r#","location_channel_id":"{c}""#);
    }
    if let Some(t) = channel_type {
        s += &format!(r#","location_channel_type":{t}"#);
    }
    s + "}"
}

fn valid_emoji_name(name: &str) -> bool {
    (2..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Trimmed server name, 2-100 characters.
fn guild_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    if (2..=100).contains(&name.chars().count()) { Ok(name) } else { Err(local("Server names must be 2-100 characters long.")) }
}

/// Trimmed channel name, 1-100 characters.
fn channel_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    if (1..=100).contains(&name.chars().count()) { Ok(name) } else { Err(local("Channel names must be 1-100 characters long.")) }
}

/// `PATCH /guilds/{g}` body without `icon` (the caller adds the picked file).
fn guild_patch_body(p: &GuildPatch) -> Result<Map<String, Value>, ApiError> {
    let mut m = Map::new();
    if let Some(name) = &p.name {
        m.insert("name".into(), guild_name(name)?.into());
    }
    if let Some(d) = &p.description {
        let d = d.as_deref().map(str::trim).filter(|d| !d.is_empty());
        if d.is_some_and(|d| d.chars().count() > 300) {
            return Err(local("The server description can be at most 300 characters long."));
        }
        m.insert("description".into(), json!(d));
    }
    if let Some(c) = p.system_channel_id {
        m.insert("system_channel_id".into(), json!(c));
    }
    if let Some(c) = p.afk_channel_id {
        m.insert("afk_channel_id".into(), json!(c));
    }
    if let Some(t) = p.afk_timeout {
        m.insert("afk_timeout".into(), t.into());
    }
    if let Some(n) = p.default_message_notifications {
        m.insert("default_message_notifications".into(), n.into());
    }
    if let Some(v) = p.verification_level {
        m.insert("verification_level".into(), v.into());
    }
    if let Some(f) = p.explicit_content_filter {
        m.insert("explicit_content_filter".into(), f.into());
    }
    Ok(m)
}

fn channel_body(name: &str, kind: u8, parent_id: Option<Id>) -> Result<Value, ApiError> {
    if !CHANNEL_KINDS.contains(&kind) {
        return Err(local("That type of channel can't be created here."));
    }
    let mut body = json!({ "name": channel_name(name)?, "type": kind });
    if let Some(p) = parent_id {
        body["parent_id"] = json!(p);
    }
    Ok(body)
}

fn channel_patch_body(p: &ChannelPatch) -> Result<Map<String, Value>, ApiError> {
    let mut m = Map::new();
    if let Some(name) = &p.name {
        m.insert("name".into(), channel_name(name)?.into());
    }
    if let Some(topic) = &p.topic {
        m.insert("topic".into(), topic.trim().into());
    }
    if let Some(nsfw) = p.nsfw {
        m.insert("nsfw".into(), nsfw.into());
    }
    if let Some(s) = p.slowmode {
        m.insert("rate_limit_per_user".into(), s.min(MAX_SLOWMODE).into());
    }
    if let Some(parent) = p.parent_id {
        m.insert("parent_id".into(), json!(parent));
    }
    if let Some(pos) = p.position {
        m.insert("position".into(), pos.into());
    }
    Ok(m)
}

fn channel_positions_body(positions: &[ChannelPosition]) -> Value {
    let entries = positions.iter().map(|p| {
        let mut e = json!({ "id": p.id, "position": p.position });
        if let Some(parent) = p.parent_id {
            e["parent_id"] = json!(parent);
        }
        if p.lock_permissions {
            e["lock_permissions"] = true.into();
        }
        e
    });
    Value::Array(entries.collect())
}

fn role_body(r: &RolePatch) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(name) = &r.name {
        m.insert("name".into(), name.trim().into());
    }
    if let Some(p) = r.permissions {
        m.insert("permissions".into(), p.to_string().into());
    }
    if let Some(c) = r.color {
        m.insert("colors".into(), json!({ "primary_color": c, "secondary_color": null, "tertiary_color": null }));
    }
    if let Some(h) = r.hoist {
        m.insert("hoist".into(), h.into());
    }
    if let Some(mentionable) = r.mentionable {
        m.insert("mentionable".into(), mentionable.into());
    }
    m
}

/// Notification-settings PATCH body (None = nothing to change). A timed mute ends `now` +
/// `mute_secs`.
fn notification_body(channel_id: Option<Id>, p: &NotificationPatch, now: chrono::DateTime<chrono::Utc>) -> Option<Value> {
    let mut m = Map::new();
    if let Some(muted) = p.muted {
        m.insert("muted".into(), muted.into());
        if muted {
            let config = match p.mute_secs.filter(|s| *s > 0) {
                Some(s) => {
                    let end = now + chrono::TimeDelta::seconds(i64::from(s));
                    json!({ "selected_time_window": s, "end_time": end.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) })
                }
                None => json!({ "selected_time_window": -1, "end_time": null }),
            };
            m.insert("mute_config".into(), config);
        }
    }
    if let Some(level) = p.level {
        m.insert("message_notifications".into(), level.into());
    }
    match channel_id {
        Some(c) => (!m.is_empty()).then(|| json!({ "channel_overrides": { c.to_string(): m } })),
        None => {
            if let Some(s) = p.suppress_everyone {
                m.insert("suppress_everyone".into(), s.into());
            }
            if let Some(s) = p.suppress_roles {
                m.insert("suppress_roles".into(), s.into());
            }
            (!m.is_empty()).then_some(Value::Object(m))
        }
    }
}

fn channel_url(channel_id: Id) -> Url {
    ApiClient::url(&["channels", &channel_id.to_string()])
}

fn role_url(guild_id: Id, role_id: Id) -> Url {
    ApiClient::url(&["guilds", &guild_id.to_string(), "roles", &role_id.to_string()])
}

fn ban_url(guild_id: Id, user_id: Id) -> Url {
    ApiClient::url(&["guilds", &guild_id.to_string(), "bans", &user_id.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::extra_headers;
    use crate::model::Perms;
    use chrono::TimeZone as _;

    #[test]
    fn audit_reasons_are_encoded_and_bounded() {
        assert!(audit_reason(None).is_empty());
        assert!(audit_reason(Some("  \n ")).is_empty());
        let h = audit_reason(Some(" spam, again\nüber "));
        assert_eq!(h, [("x-audit-log-reason", "spam%2C%20again%0A%C3%BCber".to_owned())]);
        assert!(extra_headers(&h).is_ok(), "a valid header value");
        let long = audit_reason(Some(&"é".repeat(600)));
        assert_eq!(long[0].1.len(), 512 * 6, "512 chars, each two percent-encoded bytes");
    }

    #[test]
    fn invite_links_and_codes() {
        assert_eq!(invite_code("https://discord.gg/abc-DEF9"), Some("abc-DEF9"));
        assert_eq!(invite_code("discord.com/invite/rust?event=1"), Some("rust"));
        assert_eq!(invite_code(" http://www.discordapp.com/invite/xyz) "), Some("xyz"));
        assert_eq!(invite_code("https://discord.gg/"), None);
        assert_eq!(invite_code("https://example.com/invite/abc"), None);
        assert_eq!(invite_code("https://discord.com/channels/1/2"), None);
        assert!(invite_url("abc-1").is_ok());
        assert!(invite_url("../users/@me").is_err() && invite_url("").is_err());
        assert_eq!(invite_url("abc").unwrap().as_str(), "https://discord.com/api/v9/invites/abc");
    }

    #[test]
    fn context_headers_match_the_notes() {
        assert_eq!(context(r#"{"location":"Guild Header"}"#), "eyJsb2NhdGlvbiI6Ikd1aWxkIEhlYWRlciJ9");
        let full = accept_location(Some(Id(1)), Some(Id(2)), Some(0));
        assert_eq!(full, r#"{"location":"Accept Invite Page","location_guild_id":"1","location_channel_id":"2","location_channel_type":0}"#);
        assert_eq!(accept_location(None, None, None), r#"{"location":"Accept Invite Page"}"#);
        let decoded = base64::engine::general_purpose::STANDARD.decode(context(&full)).unwrap();
        assert!(serde_json::from_slice::<Value>(&decoded).is_ok());
    }

    #[test]
    fn join_answers() {
        let w = |s: &str| serde_json::from_str::<JoinedWire>(s).unwrap();
        assert_eq!(joined_id(w(r#"{"code":"x","guild":{"id":"5","name":"G"},"new_member":true}"#)).unwrap(), Id(5));
        assert_eq!(joined_id(w(r#"{"guild_id":"6","guild":null,"show_verification_form":null}"#)).unwrap(), Id(6));
        assert_eq!(joined_id(w(r#"{"type":1,"channel":{"id":"7","type":3}}"#)).unwrap(), Id(7), "group DM invite");
        let screening = joined_id(w(r#"{"guild":{"id":"5"},"show_verification_form":true}"#)).unwrap_err();
        assert!(screening.to_string().contains("official app"));
        assert!(joined_id(w("{}")).is_err());
    }

    #[test]
    fn error_codes_read_well() {
        let e = |code| readable((ApiError::Http { status: 400, message: "raw".into() }, Some(code))).to_string();
        assert!(e(30001).contains("server limit"));
        assert!(e(50074).contains("Community"));
        assert_eq!(e(50035), "raw", "field errors keep Discord's text");
        assert_eq!(invite_readable((ApiError::NotFound, None)).to_string(), INVALID_INVITE);
        assert!(matches!(readable((ApiError::NotFound, None)), ApiError::NotFound));
    }

    #[test]
    fn guild_profiles_and_join_answers() {
        // An invented profile with every key of the documented shape.
        let p: GuildProfile = serde_json::from_str(
            r##"{"id":"55","name":"Crab Club","icon_hash":"b47f","member_count":1284,"online_count":312,
            "description":"Small, fast tools.","brand_color_primary":"#3d6f8e","banner_hash":"x",
            "game_application_ids":["1"],"game_activity":{"1":{"activity_level":1,"activity_score":45}},
            "tag":"CRAB","badge":6,"badge_color_primary":"#32a070","badge_color_secondary":"#57b59e","badge_hash":"6082",
            "traits":[{"emoji_id":null,"emoji_name":"🦀","emoji_animated":false,"label":"Rust","position":0}],
            "features":["COMMUNITY","DISCOVERABLE"],"visibility":3,"custom_banner_hash":null,
            "premium_subscription_count":13,"premium_tier":2}"##,
        )
        .unwrap();
        assert_eq!((p.id, p.name.as_str(), p.member_count, p.online_count, p.visibility, p.premium_tier), (Id(55), "Crab Club", 1284, 312, 3, 2));
        assert_eq!((p.tag.as_deref(), p.badge_hash.as_deref(), p.traits[0].label.as_str(), p.features.len()), (Some("CRAB"), Some("6082"), "Rust", 2));
        let p: GuildProfile = serde_json::from_str(r#"{"id":"55","name":null,"traits":null,"features":null,"member_count":null}"#).unwrap();
        assert!(p.name.is_empty() && p.traits.is_empty() && p.member_count == 0);
        assert!(matches!(private_profile((ApiError::Forbidden("Missing Access".into()), Some(50001))), Ok(None)));
        assert!(matches!(private_profile((ApiError::NotFound, None)), Ok(None)));
        assert!(matches!(private_profile((ApiError::Network("x".into()), None)), Err(ApiError::Network(_))));
        let j = |e| join_readable(e).to_string();
        assert!(j((ApiError::Http { status: 400, message: "captcha".into() }, None)).contains("official app"));
        assert!(j((ApiError::Forbidden(String::new()), Some(50001))).contains("invite"));
        assert_eq!(j((ApiError::NotFound, None)), "This server isn't available.");
        assert!(j((ApiError::Http { status: 400, message: "raw".into() }, Some(30001))).contains("server limit"));
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_emoji_name("blob_cat2") && !valid_emoji_name("a") && !valid_emoji_name("bad-name"));
        assert!(!valid_emoji_name(&"a".repeat(33)));
        assert_eq!(guild_name("  My Server ").unwrap(), "My Server");
        assert!(guild_name(" x ").is_err() && channel_name("  ").is_err());
        assert!(channel_body("general", 0, Some(Id(4))).is_ok_and(|b| b == json!({"name":"general","type":0,"parent_id":"4"})));
        assert!(channel_body("x", 11, None).is_err(), "threads are not created here");
    }

    #[test]
    fn patch_bodies_hold_only_set_fields() {
        assert!(guild_patch_body(&GuildPatch::default()).unwrap().is_empty());
        let g = GuildPatch {
            name: Some("Café".into()),
            description: Some(Some(" ".into())),
            afk_channel_id: Some(Some(Id(9))),
            system_channel_id: Some(None),
            afk_timeout: Some(300),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(guild_patch_body(&g).unwrap()),
            json!({"name":"Café","description":null,"afk_channel_id":"9","system_channel_id":null,"afk_timeout":300})
        );
        assert!(guild_patch_body(&GuildPatch { description: Some(Some("x".repeat(301))), ..Default::default() }).is_err());

        let c = ChannelPatch { topic: Some(String::new()), slowmode: Some(99_999), parent_id: Some(None), ..Default::default() };
        assert_eq!(Value::Object(channel_patch_body(&c).unwrap()), json!({"topic":"","rate_limit_per_user":21600,"parent_id":null}));

        let r = RolePatch { name: Some("Mods".into()), permissions: Some(1 << 40), color: Some(0xFF0000), ..Default::default() };
        assert_eq!(
            Value::Object(role_body(&r)),
            json!({"name":"Mods","permissions":"1099511627776","colors":{"primary_color":16711680,"secondary_color":null,"tertiary_color":null}})
        );
        assert!(role_body(&RolePatch::default()).is_empty());

        let moves = [
            ChannelPosition { id: Id(1), position: 0, parent_id: None, lock_permissions: false },
            ChannelPosition { id: Id(2), position: 1, parent_id: Some(None), lock_permissions: false },
            ChannelPosition { id: Id(3), position: 2, parent_id: Some(Some(Id(8))), lock_permissions: true },
        ];
        assert_eq!(
            channel_positions_body(&moves),
            json!([{"id":"1","position":0},{"id":"2","position":1,"parent_id":null},{"id":"3","position":2,"parent_id":"8","lock_permissions":true}])
        );
        let o = Overwrite { id: Id(5), kind: 0, allow: Perms(0), deny: Perms(1024) };
        assert_eq!(json!({ "type": o.kind, "allow": o.allow, "deny": o.deny }), json!({"type":0,"allow":"0","deny":"1024"}));
    }

    #[test]
    fn notification_bodies() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 11, 11, 22, 0, 0).unwrap();
        let body = |c, p: NotificationPatch| notification_body(c, &p, now);
        let mute = |secs| NotificationPatch { muted: Some(true), mute_secs: secs, ..Default::default() };
        assert_eq!(
            body(None, mute(Some(3600))),
            Some(json!({"muted":true,"mute_config":{"selected_time_window":3600,"end_time":"2026-11-11T23:00:00.000Z"}}))
        );
        assert_eq!(body(None, mute(None)), Some(json!({"muted":true,"mute_config":{"selected_time_window":-1,"end_time":null}})));
        assert_eq!(body(None, NotificationPatch { muted: Some(false), ..Default::default() }), Some(json!({"muted":false})));
        let guild = NotificationPatch { level: Some(1), suppress_everyone: Some(true), suppress_roles: Some(false), ..Default::default() };
        assert_eq!(body(None, guild.clone()), Some(json!({"message_notifications":1,"suppress_everyone":true,"suppress_roles":false})));
        // Channel overrides: a map keyed by channel id, without the guild-only fields.
        assert_eq!(body(Some(Id(42)), guild), Some(json!({"channel_overrides":{"42":{"message_notifications":1}}})));
        assert_eq!(body(Some(Id(42)), mute(None)), Some(json!({"channel_overrides":{"42":{"muted":true,"mute_config":{"selected_time_window":-1,"end_time":null}}}})));
        assert_eq!(body(None, NotificationPatch::default()), None);
        assert_eq!(body(Some(Id(42)), NotificationPatch { suppress_roles: Some(true), ..Default::default() }), None);
    }
}
