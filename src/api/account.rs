//! Account REST calls (FEATURES_NOTES §3.1, §3.2): the settings proto (status, custom status,
//! server bar folders), my profile, password and 2FA (authenticator app, SMS, security keys,
//! backup codes, the `/mfa/finish` re-check), and picked images as data URIs. Called
//! from `backend::handle_account`, which turns results into `AccountEvent`s. Every call goes
//! through `ApiClient::send` / `send_with` / `send_coded` (token only to the API host, rate
//! limiter, 401 handling, 401/60003 -> `ApiError::MfaRequired` without logout, captcha -> "do it
//! in the official app"); none is retried on 4xx. Server administration, invites and
//! notification settings are in `guild_admin.rs`.
//!
//! OWNERSHIP: settings engineer (docs/ACCOUNT_SERVER_DESIGN.md).

use super::{ApiClient, ApiError, settings_proto};
use crate::auth::{Secret, Token};
use crate::model::{AuthSession, AuthorizedApp, Consents, CustomStatus, Emoji, GuildFolder, Id, UserStatus};
use base64::Engine as _;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::PathBuf;
use zeroize::Zeroizing;

/// Largest picked image file read at all (it is downscaled before upload).
pub const MAX_IMAGE_FILE: u64 = 10 * 1024 * 1024;
/// Discord's guild emoji limit (§3.7).
const MAX_EMOJI_BYTES: usize = 256 * 1024;
/// `PATCH /users/@me` / `/users/@me/profile` image error codes: file too large, invalid file.
const FILE_TOO_LARGE: u64 = 50045;
const INVALID_FILE: u64 = 50046;

/// A check that failed before anything was sent (user-facing text).
fn local(message: &str) -> ApiError {
    ApiError::Http { status: 0, message: message.into() }
}

/// What a picked image becomes (§3.2, §3.7): the size it is downscaled to and its byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// Square crop, 512×512 PNG.
    Avatar,
    /// 1024×409 PNG (profile banner).
    Banner,
    /// Square crop, 512×512 PNG (guild icon).
    GuildIcon,
    /// 128×128 PNG, ≤ 256 KiB.
    Emoji,
}

impl ImageKind {
    fn size(self) -> (u32, u32) {
        match self {
            ImageKind::Avatar | ImageKind::GuildIcon => (512, 512),
            ImageKind::Banner => (1024, 409),
            ImageKind::Emoji => (128, 128),
        }
    }
}

/// Reads a file the user picked (never any other path), refuses it over `MAX_IMAGE_FILE` or
/// when it is no image, crops/downscales it for `kind` with the `image` crate on a blocking
/// thread, encodes PNG and returns `data:image/png;base64,…`. Errors are user-facing text.
// shortcut: animated avatars/emoji are flattened to their first frame; keep GIF bytes untouched
// for Nitro accounts once the UI offers it.
pub async fn image_data_uri(path: PathBuf, kind: ImageKind) -> Result<String, ApiError> {
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ApiError> {
        let bytes = read_picked(&path)?;
        to_png(&bytes, kind)
    })
    .await
    .map_err(|_| local("Reading the image failed."))??;
    Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

/// The picked file, at most `MAX_IMAGE_FILE` bytes (checked before and while reading).
fn read_picked(path: &std::path::Path) -> Result<Vec<u8>, ApiError> {
    let too_big = || local("That image is too large (over 10 MB).");
    let file = std::fs::File::open(path).map_err(|_| local("Could not open that file."))?;
    let meta = file.metadata().map_err(|_| local("Could not open that file."))?;
    if !meta.is_file() {
        return Err(local("That is not a file."));
    }
    if meta.len() > MAX_IMAGE_FILE {
        return Err(too_big());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_IMAGE_FILE + 1).read_to_end(&mut bytes).map_err(|_| local("Could not read that file."))?;
    if bytes.len() as u64 > MAX_IMAGE_FILE {
        return Err(too_big());
    }
    Ok(bytes)
}

/// Decodes (bounded), center-crops to `kind`'s aspect, resizes to its size and encodes PNG.
fn to_png(bytes: &[u8], kind: ImageKind) -> Result<Vec<u8>, ApiError> {
    let not_image = || local("That file is not an image Rustcord can read (PNG, JPEG, GIF, WebP).");
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| not_image())?;
    let mut limits = image::Limits::default(); // non_exhaustive: start from default
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 << 20);
    reader.limits(limits);
    let img = reader.decode().map_err(|_| not_image())?;
    let (w, h) = kind.size();
    let img = img.resize_to_fill(w, h, image::imageops::FilterType::Lanczos3);
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| local("Could not convert the image."))?;
    if kind == ImageKind::Emoji && png.len() > MAX_EMOJI_BYTES {
        return Err(local("That emoji is too large (over 256 KB)."));
    }
    Ok(png)
}

/// Readable text for Discord's image errors.
fn image_error((e, code): (ApiError, Option<u64>)) -> ApiError {
    match code {
        Some(FILE_TOO_LARGE) => local("Discord says the image is too large."),
        Some(INVALID_FILE) => local("Discord did not accept that image."),
        _ => e,
    }
}

/// `{"settings":"<b64>","out_of_date"?}` of the settings-proto routes. `settings` is required
/// (null / "" = empty): a GET answer without it must not become an edit of empty settings,
/// which would reset the other subfields of the PATCHed field.
#[derive(Deserialize)]
struct ProtoAnswer {
    #[serde(deserialize_with = "crate::model::null_default")]
    settings: String,
    #[serde(default, deserialize_with = "crate::model::null_default")]
    out_of_date: bool,
}

/// Trimmed text, refused over `max` characters; empty = None.
fn checked<'a>(text: &'a str, max: usize, what: &str) -> Result<Option<&'a str>, ApiError> {
    let t = text.trim();
    if t.chars().count() > max {
        return Err(ApiError::Http { status: 0, message: format!("{what} can be at most {max} characters.") });
    }
    Ok((!t.is_empty()).then_some(t))
}

impl ApiClient {
    // ----- settings proto (§3.1). GET `/users/@me/settings-proto/1` -> `{"settings":"<b64>"}`,
    // edit with `api::settings_proto`, `PATCH /users/@me/settings-proto/1 {"settings":"<b64>"}`
    // -> `{"settings","out_of_date"?}`; `out_of_date: true` = discarded: re-GET, re-edit,
    // re-PATCH once, then give up with an error. USER_SETTINGS_PROTO_UPDATE carries the result.

    /// GET, `edit`, PATCH (only the edited top-level field); one retry on `out_of_date`.
    async fn edit_settings(&self, edit: impl Fn(&str) -> Result<String, String>) -> Result<(), ApiError> {
        let _one_edit_at_a_time = self.settings_edit.lock().await;
        let url = || Self::url(&["users", "@me", "settings-proto", "1"]);
        for _ in 0..2 {
            let current: ProtoAnswer = self.send_json(Method::GET, url(), None).await?;
            let settings = edit(&current.settings).map_err(ApiError::Decode)?;
            let answer: ProtoAnswer = self.send_json(Method::PATCH, url(), Some(json!({ "settings": settings }))).await?;
            if !answer.out_of_date {
                return Ok(());
            }
        }
        Err(local("Your settings changed somewhere else at the same time. Try again."))
    }

    /// Saved status (field 11.1) via GET + `settings_proto::with_status` + PATCH.
    pub async fn save_status(&self, status: UserStatus) -> Result<(), ApiError> {
        self.edit_settings(|cur| settings_proto::with_status(cur, status)).await
    }

    /// Custom status (field 11.2; text ≤ 128 chars, both None clears) via
    /// `settings_proto::with_custom_status`. `expires_at_ms`: unix ms, None = never.
    pub async fn save_custom_status(&self, text: Option<&str>, emoji: Option<&Emoji>, expires_at_ms: Option<u64>) -> Result<(), ApiError> {
        let text = checked(text.unwrap_or_default(), 128, "A custom status")?.map(str::to_owned);
        let emoji = emoji.filter(|e| e.id.is_some() || e.name.as_deref().is_some_and(|n| !n.is_empty())).cloned();
        let custom = (text.is_some() || emoji.is_some()).then_some(CustomStatus { text, emoji });
        self.edit_settings(|cur| settings_proto::with_custom_status(cur, custom.as_ref(), expires_at_ms)).await
    }

    /// Server bar (field 14.1, the whole list) via `settings_proto::with_guild_folders`. An
    /// empty list is refused: it would delete every folder.
    pub async fn save_guild_folders(&self, folders: &[GuildFolder]) -> Result<(), ApiError> {
        if folders.is_empty() {
            return Err(local("Your server list is still loading."));
        }
        self.edit_settings(|cur| settings_proto::with_guild_folders(cur, folders)).await
    }

    /// Any other synced option: (path, value) pairs inside top-level field `top`
    /// (`settings_proto::with_values`). Only Settings pages' own fields, never 14 or 11.1/11.2 (their
    /// own calls above keep their rules).
    pub async fn save_settings(&self, top: u32, values: &[(Vec<u32>, settings_proto::SettingValue)]) -> Result<(), ApiError> {
        // Of field 11 (status) only 11.3 "Share detected activity"; status has its own calls.
        let status_field = top == 11 && !values.iter().all(|(path, _)| path.as_slice() == [3]);
        if top == 14 || status_field || values.is_empty() {
            return Err(local("This setting cannot be saved here."));
        }
        self.edit_settings(|cur| settings_proto::with_values(cur, top, values)).await
    }

    // ----- Settings lists (SETTINGS_NOTES §1)

    /// `GET /auth/sessions` -> `{"user_sessions":[…]}`.
    pub async fn auth_sessions(&self) -> Result<Vec<AuthSession>, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default, deserialize_with = "crate::model::null_default")]
            user_sessions: Vec<AuthSession>,
        }
        let a: Answer = self.send_json(Method::GET, Self::url(&["auth", "sessions"]), None).await?;
        Ok(a.user_sessions)
    }

    /// `POST /auth/sessions/logout {"session_id_hashes":[…]}` (1-64 hashes).
    pub async fn logout_sessions(&self, id_hashes: &[String]) -> Result<(), ApiError> {
        if id_hashes.is_empty() || id_hashes.len() > 64 {
            return Err(local("Pick 1 to 64 devices."));
        }
        let body = json!({ "session_id_hashes": id_hashes });
        self.send(Method::POST, Self::url(&["auth", "sessions", "logout"]), Some(body)).await.map(drop)
    }

    /// `GET /oauth2/tokens`.
    pub async fn authorized_apps(&self) -> Result<Vec<AuthorizedApp>, ApiError> {
        self.send_json(Method::GET, Self::url(&["oauth2", "tokens"]), None).await
    }

    /// `DELETE /oauth2/tokens/{id}`.
    pub async fn revoke_authorized_app(&self, token_id: Id) -> Result<(), ApiError> {
        self.send(Method::DELETE, Self::url(&["oauth2", "tokens", &token_id.to_string()]), None).await.map(drop)
    }

    /// `GET /safety-hub/@me` -> `account_standing.state` (SETTINGS_NOTES §1 Account).
    pub async fn account_standing(&self) -> Result<u16, ApiError> {
        #[derive(Deserialize)]
        struct Standing {
            state: u16,
        }
        #[derive(Deserialize)]
        struct Answer {
            account_standing: Standing,
        }
        let a: Answer = self.send_json(Method::GET, Self::url(&["safety-hub", "@me"]), None).await?;
        Ok(a.account_standing.state)
    }

    /// `GET /users/@me/consent`.
    pub async fn consents(&self) -> Result<Consents, ApiError> {
        self.send_json(Method::GET, Self::url(&["users", "@me", "consent"]), None).await
    }

    /// `POST /users/@me/consent {"grant":[…],"revoke":[…]}`.
    pub async fn set_consents(&self, grant: &[String], revoke: &[String]) -> Result<(), ApiError> {
        let body = json!({ "grant": grant, "revoke": revoke });
        self.send(Method::POST, Self::url(&["users", "@me", "consent"]), Some(body)).await.map(drop)
    }

    // ----- my profile (§3.2). USER_UPDATE carries the result.

    async fn patch_me(&self, profile: bool, body: Value) -> Result<(), (ApiError, Option<u64>)> {
        let path: &[&str] = if profile { &["users", "@me", "profile"] } else { &["users", "@me"] };
        self.send_coded(Method::PATCH, Self::url(path), Some(body), &[]).await.map(drop)
    }

    /// `PATCH /users/@me {"global_name"}` (1-32 chars, None/blank -> null).
    pub async fn set_global_name(&self, name: Option<&str>) -> Result<(), ApiError> {
        let name = checked(name.unwrap_or_default(), 32, "A display name")?;
        self.patch_me(false, json!({ "global_name": name })).await.map_err(|(e, _)| e)
    }

    /// `PATCH /users/@me {"avatar": image_data_uri(path, Avatar) | null}`. 50045/50046 ->
    /// readable text.
    pub async fn set_avatar(&self, path: Option<PathBuf>) -> Result<(), ApiError> {
        let avatar = match path {
            Some(p) => Some(image_data_uri(p, ImageKind::Avatar).await?),
            None => None,
        };
        self.patch_me(false, json!({ "avatar": avatar })).await.map_err(image_error)
    }

    /// `PATCH /users/@me/profile {"banner": image_data_uri(path, Banner) | null}` (Nitro).
    pub async fn set_banner(&self, path: Option<PathBuf>) -> Result<(), ApiError> {
        let banner = match path {
            Some(p) => Some(image_data_uri(p, ImageKind::Banner).await?),
            None => None,
        };
        self.patch_me(true, json!({ "banner": banner })).await.map_err(image_error)
    }

    /// `PATCH /users/@me/profile {"bio"}` (≤ 190 chars, checked before sending).
    pub async fn set_bio(&self, bio: &str) -> Result<(), ApiError> {
        let bio = checked(bio, 190, "About Me")?.unwrap_or_default();
        self.patch_me(true, json!({ "bio": bio })).await.map_err(|(e, _)| e)
    }

    /// `PATCH /users/@me/profile {"pronouns"}` (≤ 40 chars, checked before sending).
    pub async fn set_pronouns(&self, pronouns: &str) -> Result<(), ApiError> {
        let pronouns = checked(pronouns, 40, "Pronouns")?.unwrap_or_default();
        self.patch_me(true, json!({ "pronouns": pronouns })).await.map_err(|(e, _)| e)
    }

    /// `PATCH /users/@me/profile {"accent_color": int | null}`.
    pub async fn set_accent_color(&self, color: Option<u32>) -> Result<(), ApiError> {
        if color.is_some_and(|c| c > 0xFF_FFFF) {
            return Err(local("That is not a colour."));
        }
        self.patch_me(true, json!({ "accent_color": color })).await.map_err(|(e, _)| e)
    }

    /// `PATCH /users/@me {"username","password"}` (2-32 chars). The password is only exposed
    /// into the request body; 50035 field errors via `discord_message`, 60003 ->
    /// `MfaRequired`. The answer's `token` is ignored (only a password change rotates it).
    pub async fn set_username(&self, username: &str, password: &Secret) -> Result<(), ApiError> {
        let username = checked(username, 32, "A username")?.unwrap_or_default();
        if username.chars().count() < 2 {
            return Err(local("A username needs at least 2 characters."));
        }
        if password.expose().is_empty() {
            return Err(local("Enter your password to change your username."));
        }
        // shortcut: the JSON body (here and inside reqwest) is not zeroized, as in api/auth.rs.
        let body = json!({ "username": username, "password": password.expose() });
        self.patch_me(false, body).await.map_err(|(e, _)| e)
    }

    // ----- Password and Authentication (SETTINGS_NOTES §1 Account, FEATURES_NOTES §0.2, §3.2).
    // The token-returning calls only hand the new token back; `backend` puts it in place.

    /// `POST /mfa/finish {"ticket","mfa_type","data"}` -> `{"token"}`: the ~5 min JWT for
    /// `X-Discord-MFA-Authorization` (`api::with_mfa`). Codes lose spaces and dashes.
    pub async fn mfa_finish(&self, ticket: &Secret, kind: MfaKind, data: &Secret) -> Result<Secret, (ApiError, Option<u64>)> {
        let resp = self.send_coded(Method::POST, Self::url(&["mfa", "finish"]), Some(finish_body(ticket, kind, data)), &[]).await?;
        let a: TokenAnswer = read_json(resp).await.map_err(|e| (e, None))?;
        a.token.filter(|t| !t.is_empty()).map(Secret::new).ok_or((ApiError::Decode("no token".into()), None))
    }

    /// `POST /auth/mfa/sms/send {"ticket"}` -> `{"phone"}` (masked) for the re-check prompt.
    pub async fn mfa_send_sms(&self, ticket: &Secret) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            phone: String,
        }
        let body = json!({ "ticket": ticket.expose() });
        let a: Answer = self.send_json(Method::POST, Self::url(&["auth", "mfa", "sms", "send"]), Some(body)).await?;
        Ok(a.phone)
    }

    /// `PATCH /users/@me {"password","new_password"}` -> the user + its new `token` (every other
    /// login ends). New passwords are checked for 8-72 characters before sending.
    pub async fn change_password(&self, password: &Secret, new_password: &Secret) -> Result<Token, ApiError> {
        if password.expose().is_empty() {
            return Err(local("Enter your current password."));
        }
        if !(8..=72).contains(&new_password.expose().chars().count()) {
            return Err(local("A new password needs 8 to 72 characters."));
        }
        let a: TokenAnswer = self.send_json(Method::PATCH, Self::url(&["users", "@me"]), Some(password_body(password, new_password))).await?;
        a.into_token()
    }

    /// Authenticator step 1: `POST /users/@me/mfa/totp/enable {"password"}`; Discord answers
    /// 60005 ("send a secret and code too") when the password is right.
    pub async fn totp_check_password(&self, password: &Secret) -> Result<(), ApiError> {
        let body = json!({ "password": password.expose() });
        match self.send_coded(Method::POST, Self::url(&["users", "@me", "mfa", "totp", "enable"]), Some(body), &[]).await {
            Ok(_) | Err((_, Some(TOTP_NEEDS_CODE))) => Ok(()),
            Err((e, _)) => Err(e),
        }
    }

    /// Authenticator step 2: `POST /users/@me/mfa/totp/enable {"password","secret","code"}` ->
    /// `{"token","backup_codes"}`.
    pub async fn enable_totp(&self, password: &Secret, secret: &Secret, code: &Secret) -> Result<(Token, Vec<BackupCode>), ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            token: Option<String>,
            #[serde(default)]
            backup_codes: Vec<BackupCode>,
        }
        let url = Self::url(&["users", "@me", "mfa", "totp", "enable"]);
        let a: Answer = self.send_json(Method::POST, url, Some(totp_body(password, secret, code))).await?;
        Ok((TokenAnswer { token: a.token }.into_token()?, a.backup_codes))
    }

    /// `POST /users/@me/mfa/totp/disable` (2FA-gated) -> `{"token"}`.
    pub async fn disable_totp(&self) -> Result<Token, ApiError> {
        let a: TokenAnswer = self.send_json(Method::POST, Self::url(&["users", "@me", "mfa", "totp", "disable"]), None).await?;
        a.into_token()
    }

    /// `POST /users/@me/mfa/sms/enable|disable {"password"}` (needs the authenticator app and a
    /// verified phone).
    pub async fn set_sms_mfa(&self, enable: bool, password: &Secret) -> Result<(), ApiError> {
        let body = json!({ "password": password.expose() });
        let url = Self::url(&["users", "@me", "mfa", "sms", if enable { "enable" } else { "disable" }]);
        self.send(Method::POST, url, Some(body)).await.map(drop)
    }

    /// `GET /users/@me/mfa/webauthn/credentials`.
    pub async fn security_keys(&self) -> Result<Vec<SecurityKey>, ApiError> {
        self.send_json(Method::GET, Self::url(&["users", "@me", "mfa", "webauthn", "credentials"]), None).await
    }

    /// `PATCH /users/@me/mfa/webauthn/credentials/{id} {"name"}` (1-32 characters).
    pub async fn rename_security_key(&self, id: Id, name: &str) -> Result<(), ApiError> {
        let name = checked(name, 32, "A key name")?.ok_or_else(|| local("Give the key a name."))?;
        let url = Self::url(&["users", "@me", "mfa", "webauthn", "credentials", &id.to_string()]);
        self.send(Method::PATCH, url, Some(json!({ "name": name }))).await.map(drop)
    }

    /// `DELETE /users/@me/mfa/webauthn/credentials/{id}` (2FA-gated).
    pub async fn delete_security_key(&self, id: Id) -> Result<(), ApiError> {
        let url = Self::url(&["users", "@me", "mfa", "webauthn", "credentials", &id.to_string()]);
        self.send(Method::DELETE, url, None).await.map(drop)
    }

    /// View Backup Codes step 1: `POST /auth/verify/view-backup-codes-challenge {"password"}` ->
    /// `{"nonce","regenerate_nonce"}`; Discord emails a key.
    pub async fn backup_codes_challenge(&self, password: &Secret) -> Result<CodesNonce, ApiError> {
        let body = json!({ "password": password.expose() });
        self.send_json(Method::POST, Self::url(&["auth", "verify", "view-backup-codes-challenge"]), Some(body)).await
    }

    /// Step 2: `POST /users/@me/mfa/codes-verification {"key","nonce","regenerate"}` ->
    /// `{"backup_codes"}` (`nonce` = `regenerate_nonce` when regenerating).
    pub async fn backup_codes(&self, key: &Secret, nonce: &Secret, regenerate: bool) -> Result<Vec<BackupCode>, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            backup_codes: Vec<BackupCode>,
        }
        let url = Self::url(&["users", "@me", "mfa", "codes-verification"]);
        let a: Answer = self.send_json(Method::POST, url, Some(codes_body(key, nonce, regenerate))).await?;
        Ok(a.backup_codes)
    }
}

// ----- 2FA types and bodies

/// `/users/@me/mfa/totp/enable` with only the password: "now send the secret and code".
const TOTP_NEEDS_CODE: u64 = 60005;
/// `/mfa/finish` with a wrong code / wrong password: the ticket stays good.
pub const WRONG_MFA_CODE: [u64; 2] = [60008, 50018];

/// A way to pass the 2FA re-check of a 60003 ticket (`/mfa/finish` `mfa_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaKind {
    /// Authenticator app code.
    Totp,
    /// One of the account's backup codes.
    Backup,
    Sms,
    /// Accounts without 2FA confirm with their password.
    Password,
}

impl MfaKind {
    pub fn wire(self) -> &'static str {
        match self {
            MfaKind::Totp => "totp",
            MfaKind::Backup => "backup",
            MfaKind::Sms => "sms",
            MfaKind::Password => "password",
        }
    }
}

/// A 401/60003 answer: the ticket for `/mfa/finish` and the ways it can be passed (security
/// keys are skipped: Rustcord can't do WebAuthn). Debug never shows the ticket.
#[derive(Debug, Clone)]
pub struct MfaChallenge {
    pub ticket: Secret,
    pub methods: Vec<MfaKind>,
}

/// `{"mfa":{"ticket","methods":[{"type","backup_codes_allowed"?}]}}` of a 60003 body (FEATURES_NOTES §0.2).
pub(super) fn mfa_challenge(body: &Value) -> MfaChallenge {
    let mfa = body.get("mfa");
    let ticket = mfa.and_then(|m| m.get("ticket")?.as_str()).unwrap_or_default();
    let mut methods = Vec::new();
    for m in mfa.and_then(|m| m.get("methods")?.as_array()).into_iter().flatten() {
        let kind = match m.get("type").and_then(Value::as_str) {
            Some("totp") => MfaKind::Totp,
            Some("backup") => MfaKind::Backup,
            Some("sms") => MfaKind::Sms,
            Some("password") => MfaKind::Password,
            _ => continue,
        };
        let backup = kind == MfaKind::Totp && m.get("backup_codes_allowed").and_then(Value::as_bool) == Some(true);
        for k in std::iter::once(kind).chain(backup.then_some(MfaKind::Backup)) {
            if !methods.contains(&k) {
                methods.push(k);
            }
        }
    }
    if ticket.is_empty() {
        methods.clear();
    }
    MfaChallenge { ticket: Secret::new(ticket.to_owned()), methods }
}

/// One backup code (8 characters), shown once; Debug never shows it.
#[derive(Debug, Clone)]
pub struct BackupCode {
    pub code: Secret,
    pub consumed: bool,
}

impl<'de> Deserialize<'de> for BackupCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            code: String,
            #[serde(default)]
            consumed: bool,
        }
        let r = Raw::deserialize(d)?;
        // Moved, not copied, into the zeroizing Secret.
        Ok(BackupCode { code: Secret::new(r.code), consumed: r.consumed })
    }
}

/// View Backup Codes step 1 answer; Debug never shows the nonces.
#[derive(Debug, Clone)]
pub struct CodesNonce {
    pub nonce: Secret,
    pub regenerate_nonce: Secret,
}

impl<'de> Deserialize<'de> for CodesNonce {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            nonce: String,
            regenerate_nonce: String,
        }
        let r = Raw::deserialize(d)?;
        Ok(CodesNonce { nonce: Secret::new(r.nonce), regenerate_nonce: Secret::new(r.regenerate_nonce) })
    }
}

/// A registered security key / passkey (list, rename, delete; adding needs WebAuthn).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecurityKey {
    pub id: Id,
    #[serde(default, deserialize_with = "crate::model::null_default")]
    pub name: String,
}

/// `{"token"}` of a token-rotating answer (the rest of it is ignored).
#[derive(Deserialize)]
struct TokenAnswer {
    token: Option<String>,
}

impl TokenAnswer {
    fn into_token(self) -> Result<Token, ApiError> {
        self.token.as_deref().and_then(Token::parse).ok_or_else(|| ApiError::Decode("no new login token".into()))
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, ApiError> {
    let bytes = resp.bytes().await.map_err(super::net_err)?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
}

/// A typed code without spaces and dashes ("123 456", "abcd-efgh").
fn clean_code(code: &Secret) -> Zeroizing<String> {
    Zeroizing::new(code.expose().chars().filter(|c| !c.is_whitespace() && *c != '-').collect())
}

fn finish_body(ticket: &Secret, kind: MfaKind, data: &Secret) -> Value {
    let data = if kind == MfaKind::Password { Zeroizing::new(data.expose().to_owned()) } else { clean_code(data) };
    json!({ "ticket": ticket.expose(), "mfa_type": kind.wire(), "data": data.as_str() })
}

fn password_body(password: &Secret, new_password: &Secret) -> Value {
    json!({ "password": password.expose(), "new_password": new_password.expose() })
}

fn totp_body(password: &Secret, secret: &Secret, code: &Secret) -> Value {
    json!({ "password": password.expose(), "secret": secret.expose(), "code": clean_code(code).as_str() })
}

fn codes_body(key: &Secret, nonce: &Secret, regenerate: bool) -> Value {
    json!({ "key": clean_code(key).as_str(), "nonce": nonce.expose(), "regenerate": regenerate })
}

/// A new authenticator secret: 20 bytes from the OS RNG as 32 base32 characters (RFC 4648,
/// no padding). None if the OS RNG fails.
pub fn totp_secret() -> Option<Secret> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bytes = Zeroizing::new([0u8; 20]);
    getrandom::fill(&mut *bytes).ok()?;
    let mut out = String::with_capacity(32);
    // 20 bytes = 160 bits = 4 groups of 5 bytes -> 8 characters each.
    for group in bytes.chunks(5) {
        let n = group.iter().fold(0u64, |n, b| n << 8 | u64::from(*b));
        out.extend((0..8).rev().map(|i| char::from(ALPHABET[(n >> (i * 5)) as usize & 31])));
    }
    Some(Secret::new(out))
}

/// `otpauth://totp/Discord:<username>?secret=<secret>&issuer=Discord` for the QR code.
pub fn otpauth_uri(username: &str, secret: &Secret) -> Secret {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    const NAME: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'_').remove(b'-');
    Secret::new(format!("otpauth://totp/Discord:{}?secret={}&issuer=Discord", utf8_percent_encode(username, NAME), secret.expose()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(w, h, image::Rgba([200, 80, 20, 255])));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn dims(png: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(png).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn images_are_cropped_and_resized_per_kind() {
        let src = png(300, 100);
        assert_eq!(dims(&to_png(&src, ImageKind::Avatar).unwrap()), (512, 512));
        assert_eq!(dims(&to_png(&src, ImageKind::Banner).unwrap()), (1024, 409));
        let emoji = to_png(&src, ImageKind::Emoji).unwrap();
        assert_eq!(dims(&emoji), (128, 128));
        assert!(emoji.len() <= MAX_EMOJI_BYTES);
        assert!(to_png(b"definitely not an image", ImageKind::Avatar).is_err());
    }

    #[test]
    fn picked_files_are_size_checked() {
        let dir = std::env::temp_dir().join(format!("rustcord-account-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.png");
        std::fs::write(&small, png(4, 4)).unwrap();
        assert_eq!(read_picked(&small).unwrap().len() as u64, std::fs::metadata(&small).unwrap().len());
        let big = dir.join("big.bin");
        std::fs::File::create(&big).unwrap().set_len(MAX_IMAGE_FILE + 1).unwrap();
        assert!(read_picked(&big).unwrap_err().to_string().contains("too large"));
        assert!(read_picked(&dir).is_err(), "a folder is refused");
        assert!(read_picked(&dir.join("missing.png")).is_err());
        let uri = tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(image_data_uri(small, ImageKind::Emoji)).unwrap();
        assert!(uri.starts_with("data:image/png;base64,iVBORw0KGgo"), "{}", &uri[..40]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_limits_and_image_errors() {
        assert_eq!(checked("  hi  ", 5, "X").unwrap(), Some("hi"));
        assert_eq!(checked("   ", 5, "X").unwrap(), None);
        assert_eq!(checked("ééééé", 5, "X").unwrap(), Some("ééééé"), "characters, not bytes");
        assert_eq!(checked("123456", 5, "Pronouns").unwrap_err().to_string(), "Pronouns can be at most 5 characters.");
        let other = || ApiError::Http { status: 400, message: "x".into() };
        assert!(image_error((other(), Some(FILE_TOO_LARGE))).to_string().contains("too large"));
        assert!(image_error((other(), Some(INVALID_FILE))).to_string().contains("did not accept"));
        assert_eq!(image_error((other(), Some(50035))).to_string(), "x");
        let a: ProtoAnswer = serde_json::from_str(r#"{"settings":"WgcKBQoDZG5k","out_of_date":true}"#).unwrap();
        assert!(a.out_of_date && a.settings == "WgcKBQoDZG5k");
        let a: ProtoAnswer = serde_json::from_str(r#"{"settings":null}"#).unwrap();
        assert!(!a.out_of_date && a.settings.is_empty());
        assert!(serde_json::from_str::<ProtoAnswer>(r#"{"out_of_date":false}"#).is_err(), "no settings: nothing to edit");
    }

    fn s(v: &str) -> Secret {
        Secret::new(v.into())
    }

    #[test]
    fn totp_secrets_are_32_base32_characters_and_differ() {
        let a = totp_secret().unwrap();
        let b = totp_secret().unwrap();
        for k in [&a, &b] {
            assert_eq!(k.expose().len(), 32);
            assert!(k.expose().bytes().all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c)), "RFC 4648 alphabet");
        }
        assert_ne!(a.expose(), b.expose());
        assert_eq!(format!("{a:?}"), "Secret(<redacted>)");
    }

    #[test]
    fn otpauth_uri_names_discord_and_the_user() {
        let key = s("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP");
        assert_eq!(otpauth_uri("ferris_the.crab", &key).expose(), "otpauth://totp/Discord:ferris_the.crab?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Discord");
        assert!(otpauth_uri("old name#1234", &key).expose().starts_with("otpauth://totp/Discord:old%20name%231234?secret="));
    }

    #[test]
    fn security_request_bodies() {
        assert_eq!(password_body(&s("old pw"), &s("new password")), json!({"password":"old pw","new_password":"new password"}));
        assert_eq!(totp_body(&s("pw"), &s("KEY"), &s(" 123 456 ")), json!({"password":"pw","secret":"KEY","code":"123456"}));
        assert_eq!(finish_body(&s("T"), MfaKind::Totp, &s("123 456")), json!({"ticket":"T","mfa_type":"totp","data":"123456"}));
        assert_eq!(finish_body(&s("T"), MfaKind::Backup, &s("abcd-1234")), json!({"ticket":"T","mfa_type":"backup","data":"abcd1234"}));
        assert_eq!(finish_body(&s("T"), MfaKind::Password, &s(" my pass-word ")), json!({"ticket":"T","mfa_type":"password","data":" my pass-word "}));
        assert_eq!(codes_body(&s("ab12-cd34"), &s("N"), true), json!({"key":"ab12cd34","nonce":"N","regenerate":true}));
    }

    #[test]
    fn mfa_challenges_and_answers_parse_without_leaking() {
        let body = json!({"code":60003,"mfa":{"ticket":"TKT","methods":[{"type":"totp","backup_codes_allowed":true},{"type":"webauthn"},{"type":"sms"},{"type":"password"}]}});
        let c = mfa_challenge(&body);
        assert_eq!(c.ticket.expose(), "TKT");
        assert_eq!(c.methods, [MfaKind::Totp, MfaKind::Backup, MfaKind::Sms, MfaKind::Password]);
        assert!(!format!("{c:?}").contains("TKT"));
        assert!(mfa_challenge(&json!({"code":60003})).methods.is_empty(), "no ticket: nothing to pass");

        let codes: Vec<BackupCode> = serde_json::from_str(r#"[{"user_id":"1","code":"abcd1234","consumed":false},{"code":"used5678","consumed":true}]"#).unwrap();
        assert_eq!((codes[0].code.expose(), codes[0].consumed, codes[1].consumed), ("abcd1234", false, true));
        let n: CodesNonce = serde_json::from_str(r#"{"nonce":"N1","regenerate_nonce":"N2"}"#).unwrap();
        assert_eq!((n.nonce.expose(), n.regenerate_nonce.expose()), ("N1", "N2"));
        let debug = format!("{codes:?} {n:?}");
        assert!(!debug.contains("abcd1234") && !debug.contains("N1"), "{debug}");
        let k: Vec<SecurityKey> = serde_json::from_str(r#"[{"id":"42","name":"YubiKey","type":1}]"#).unwrap();
        assert_eq!(k, [SecurityKey { id: Id(42), name: "YubiKey".into() }]);
        assert!(TokenAnswer { token: None }.into_token().is_err());
        assert!(TokenAnswer { token: Some("x".repeat(40)) }.into_token().is_ok());
    }
}
