//! Account REST calls (FEATURES_NOTES §3.1, §3.2): the settings proto (status, custom status,
//! server bar folders), my profile, and picked images as data URIs. Called
//! from `backend::handle_account`, which turns results into `AccountEvent`s. Every call goes
//! through `ApiClient::send` / `send_with` / `send_coded` (token only to the API host, rate
//! limiter, 401 handling, 401/60003 -> `ApiError::MfaRequired` without logout, captcha -> "do it
//! in the official app"); none is retried on 4xx. Server administration, invites and
//! notification settings are in `guild_admin.rs`.
//!
//! OWNERSHIP: settings engineer (docs/ACCOUNT_SERVER_DESIGN.md).

use super::{ApiClient, ApiError, settings_proto};
use crate::auth::Secret;
use crate::model::{AuthSession, AuthorizedApp, Consents, CustomStatus, Emoji, GuildFolder, Id, UserStatus};
use base64::Engine as _;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::PathBuf;

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
    /// `MfaRequired`. The answer's `token` is ignored (only a password change rotates it, which
    /// Rustcord does not offer).
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
}
