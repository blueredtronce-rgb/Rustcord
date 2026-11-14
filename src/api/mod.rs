//! Discord HTTP API client (user account).
//!
//! OWNERSHIP: api implementer (this file, ratelimit.rs, props.rs, cdn.rs, and the body of
//! `backend::run`). Pub signatures below are the contract used by backend.rs/ui.

pub mod account;
pub mod apps;
pub mod auth;
pub mod calls;
pub mod cdn;
pub mod chat;
pub mod collectibles;
pub mod cosmetics;
pub mod guild_admin;
pub mod props;
pub mod ratelimit;
pub mod settings_proto;
pub mod social;
pub mod threads;
pub mod upload;

use crate::auth::Token;
use crate::backend::ReplyTo;
use crate::model::{Emoji, Id, Message, User};
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

pub const API_BASE: &str = "https://discord.com/api/v9";
/// Discord error code of a 401 that asks for a 2FA re-check.
const MFA_REQUIRED: u64 = 60003;

#[derive(Debug, Clone)]
pub enum ApiError {
    /// 401: token invalid/expired (the only error that logs the user out).
    Unauthorized,
    /// 401 code 60003: the action needs a fresh 2FA check (`/mfa/finish` with this ticket,
    /// then `with_mfa`); the login itself is fine.
    MfaRequired(account::MfaChallenge),
    /// 403 with Discord's message.
    Forbidden(String),
    NotFound,
    /// Gave up after repeated 429s.
    RateLimited,
    Http { status: u16, message: String },
    Network(String),
    Decode(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => f.write_str("Your login is no longer valid. Please log in again."),
            ApiError::MfaRequired(_) => {
                f.write_str("This action needs a two-factor check. Do it in the official app for now.")
            }
            ApiError::Forbidden(m) if m.is_empty() => f.write_str("You don't have permission to do that."),
            ApiError::Forbidden(m) => write!(f, "Not allowed: {m}"),
            ApiError::NotFound => f.write_str("Not found (it may have been deleted)."),
            ApiError::RateLimited => f.write_str("You're doing that too fast. Wait a moment and try again."),
            ApiError::Http { status, message } if message.is_empty() => {
                write!(f, "Discord returned an error (HTTP {status}).")
            }
            ApiError::Http { message, .. } => f.write_str(message),
            ApiError::Network(m) => write!(f, "Network problem: {m}. Check your internet connection."),
            ApiError::Decode(m) => write!(f, "Unexpected response from Discord ({m})."),
        }
    }
}

impl std::error::Error for ApiError {}

/// reqwest errors without URL or headers, in plain words.
pub(crate) fn net_err(e: reqwest::Error) -> ApiError {
    let what = if e.is_timeout() {
        "the request timed out"
    } else if e.is_connect() {
        "could not connect to Discord"
    } else if e.is_body() || e.is_decode() {
        "the response was cut off"
    } else {
        "the connection failed"
    };
    ApiError::Network(what.into())
}

/// Map a non-success status + body to an ApiError, using Discord's `{message, code}` JSON.
fn error_from(status: u16, body: &[u8]) -> ApiError {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let message = json.as_ref().map(discord_message).unwrap_or_default();
    let code = json.as_ref().and_then(|v| v.get("code")?.as_u64());
    match status {
        401 if code == Some(MFA_REQUIRED) => ApiError::MfaRequired(account::mfa_challenge(json.as_ref().unwrap_or(&Value::Null))),
        // An invalid token is `{"message":"401: Unauthorized","code":0}`; 401s with another
        // code are about the action, not the login, and must not log the user out.
        401 if matches!(code, None | Some(0) | Some(40001)) => ApiError::Unauthorized,
        401 => ApiError::Http { status, message },
        403 => ApiError::Forbidden(message),
        404 => ApiError::NotFound,
        429 => ApiError::RateLimited,
        400 if json.as_ref().is_some_and(|v| v.get("captcha_key").is_some()) => ApiError::Http {
            status,
            message: "Discord wants a captcha for this action. Complete it in the official app, then try again."
                .into(),
        },
        _ => ApiError::Http { status, message },
    }
}

/// Validated extra request headers; never lets a caller replace the auth header.
fn extra_headers(extra: &[(&'static str, String)]) -> Result<Vec<(HeaderName, HeaderValue)>, ApiError> {
    let bad = || ApiError::Http { status: 0, message: "Internal error: invalid request header.".into() };
    extra
        .iter()
        .map(|(name, value)| {
            let name = HeaderName::from_static(name);
            if name == header::AUTHORIZATION {
                return Err(bad());
            }
            Ok((name, HeaderValue::from_str(value).map_err(|_| bad())?))
        })
        .collect()
}

fn discord_message(v: &Value) -> String {
    // "Invalid Form Body" hides the useful text in errors.<field>._errors[0].message.
    if let Some(detail) = v.get("errors").and_then(first_nested_error) {
        return detail;
    }
    v.get("message").and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn first_nested_error(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    if let Some(msg) = obj.get("_errors").and_then(|e| e.get(0)?.get("message")?.as_str()) {
        return Some(msg.to_owned());
    }
    obj.values().find_map(first_nested_error)
}

fn default_headers(props: &props::ClientProps) -> HeaderMap {
    let locale = props.system_locale.as_str();
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("accept", "*/*".into()),
        ("accept-language", props.accept_language()),
        ("origin", "https://discord.com".into()),
        ("referer", "https://discord.com/channels/@me".into()),
        ("sec-fetch-dest", "empty".into()),
        ("sec-fetch-mode", "cors".into()),
        ("sec-fetch-site", "same-origin".into()),
        ("x-debug-options", "bugReporterEnabled".into()),
        ("x-discord-locale", locale.to_owned()),
        ("x-super-properties", props.super_properties_header()),
    ];
    // IANA zone names are only cheaply available where TZ is set (mostly Unix).
    if let Ok(tz) = std::env::var("TZ")
        && tz.contains('/')
    {
        pairs.push(("x-discord-timezone", tz.trim_start_matches(':').to_owned()));
    }
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        // A locale from settings could contain characters headers can't carry: skip it then.
        if let Ok(v) = HeaderValue::from_str(&value) {
            map.insert(HeaderName::from_static(name), v);
        }
    }
    map
}

pub struct ApiClient {
    http: reqwest::Client,
    /// Pre-built, marked sensitive (never printed by Debug impls). Replaced by `set_token` when a
    /// password / 2FA change rotates the token.
    auth: std::sync::RwLock<Option<HeaderValue>>,
    limiter: ratelimit::RateLimiter,
    /// Set after the first 401: every later request fails fast instead of hitting the API
    /// again (invalid requests count towards Cloudflare's IP ban).
    unauthorized: AtomicBool,
    /// Token-rotating requests on their way (`rotation`): a 401 meanwhile is the old token
    /// ending, not a logout.
    rotating: AtomicUsize,
    /// Held across a settings-proto GET, edit and PATCH: concurrent edits would PATCH stale
    /// copies of the same top-level field (e.g. status flipping back after a custom status).
    settings_edit: tokio::sync::Mutex<()>,
}

/// The Authorization value for `token`, marked sensitive; None if it can't be a header.
fn auth_header(token: &Token) -> Option<HeaderValue> {
    HeaderValue::from_str(token.expose()).ok().map(|mut v| {
        v.set_sensitive(true);
        v
    })
}

tokio::task_local! {
    /// `X-Discord-MFA-Authorization` for the first request of an action retried after a 2FA
    /// re-check (`with_mfa`); taken by that request.
    static MFA_AUTH: std::cell::RefCell<Option<HeaderValue>>;
}

/// Runs `fut` (one user action) with the `/mfa/finish` JWT on its first request
/// (FEATURES_NOTES §0.2: `X-Discord-MFA-Authorization`).
pub async fn with_mfa<F: std::future::Future>(jwt: &crate::auth::Secret, fut: F) -> F::Output {
    let value = HeaderValue::from_str(jwt.expose()).ok().map(|mut v| {
        v.set_sensitive(true);
        v
    });
    MFA_AUTH.scope(std::cell::RefCell::new(value), fut).await
}

/// Held while a token-rotating request is on its way (`ApiClient::rotation`).
pub struct Rotation<'a>(&'a ApiClient);

impl Drop for Rotation<'_> {
    fn drop(&mut self) {
        self.0.rotating.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ApiClient {
    pub fn new(token: Token, props: Arc<props::ClientProps>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(props.user_agent())
            .default_headers(default_headers(&props))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .gzip(true)
            // The API never redirects; refusing keeps the token on discord.com.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client (TLS backend) failed to initialise");
        let auth = auth_header(&token);
        let unauthorized = AtomicBool::new(auth.is_none());
        let auth = std::sync::RwLock::new(auth);
        Self { http, auth, limiter: ratelimit::RateLimiter::new(), unauthorized, rotating: AtomicUsize::new(0), settings_edit: Default::default() }
    }

    fn current_auth(&self) -> Option<HeaderValue> {
        self.auth.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// The rotated login token (password / 2FA change): later requests use it, and a 401 of the
    /// old one no longer counts. Only `backend`'s token swap calls this.
    pub fn set_token(&self, token: &Token) {
        let auth = auth_header(token);
        self.unauthorized.store(auth.is_none(), Ordering::Relaxed);
        *self.auth.write().unwrap_or_else(std::sync::PoisonError::into_inner) = auth;
    }

    /// Tests: requests carry `token` and are not blocked by an earlier 401.
    #[cfg(test)]
    pub fn sends(&self, token: &Token) -> bool {
        self.current_auth() == auth_header(token) && !self.unauthorized.load(Ordering::Relaxed)
    }

    /// Hold while sending a request that rotates the token: Discord may end the old token before
    /// the answer arrives, and those 401s must not log the user out.
    pub fn rotation(&self) -> Rotation<'_> {
        self.rotating.fetch_add(1, Ordering::Relaxed);
        Rotation(self)
    }

    fn url(segments: &[&str]) -> Url {
        let mut url = Url::parse(API_BASE).expect("API_BASE is a valid URL");
        // Percent-encodes each segment (unicode emoji, spaces); ':' and '@' stay as-is.
        url.path_segments_mut().expect("API_BASE can be a base").extend(segments);
        url
    }

    /// Rate-limited request with auth; returns the response if the status is a success.
    async fn send(&self, method: Method, url: Url, body: Option<Value>) -> Result<reqwest::Response, ApiError> {
        self.send_with(method, url, body, &[]).await
    }

    /// `send` plus per-request headers such as `x-context-properties` or `x-audit-log-reason`
    /// (lowercase names; values already encoded as FEATURES_NOTES §0.3 describes).
    async fn send_with(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        extra: &[(&'static str, String)],
    ) -> Result<reqwest::Response, ApiError> {
        self.send_coded(method, url, body, extra).await.map_err(|(e, _)| e)
    }

    /// `send_with` whose error also carries Discord's JSON error `code` (e.g. 80013, "confirm
    /// the stranger request"), for the few calls that branch on it.
    async fn send_coded(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        extra: &[(&'static str, String)],
    ) -> Result<reqwest::Response, (ApiError, Option<u64>)> {
        let fail = |e: ApiError| (e, None);
        if self.unauthorized.load(Ordering::Relaxed) {
            return Err(fail(ApiError::Unauthorized));
        }
        let extra = extra_headers(extra).map_err(fail)?;
        // The token is only ever sent to the API host.
        let auth = match self.current_auth() {
            Some(a) if url.as_str().starts_with(API_BASE) && url.as_str()[API_BASE.len()..].starts_with('/') => a,
            _ => return Err(fail(ApiError::Http { status: 0, message: "Internal error: refused a non-API URL.".into() })),
        };
        let mfa = MFA_AUTH.try_with(|m| m.borrow_mut().take()).ok().flatten();
        let key = ratelimit::route_key(method.as_str(), url.path());
        let resp = self
            .limiter
            .execute(&key, || {
                let mut req = self.http.request(method.clone(), url.clone()).header(header::AUTHORIZATION, auth.clone());
                if let Some(m) = &mfa {
                    req = req.header("x-discord-mfa-authorization", m.clone());
                }
                for (name, value) in &extra {
                    req = req.header(name, value);
                }
                if let Some(b) = &body {
                    req = req.json(b);
                } else if matches!(method, Method::POST | Method::PUT | Method::PATCH) {
                    // Like browsers: hyper sends no length for an empty body, and bodyless
                    // POST/PUT (typing, pin, reactions) may otherwise get 411 Length Required.
                    req = req.header(header::CONTENT_LENGTH, "0");
                }
                req.send()
            })
            .await
            .map_err(fail)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let bytes = resp.bytes().await.unwrap_or_default();
        let mut err = error_from(status.as_u16(), &bytes);
        if matches!(err, ApiError::Unauthorized) {
            // The token was just rotated (or is being): this was the old one, not a logout.
            if self.rotating.load(Ordering::Relaxed) > 0 || self.current_auth().as_ref() != Some(&auth) {
                err = ApiError::Http { status: 401, message: "Your login changed while this was on its way. Try again.".into() };
            } else {
                self.unauthorized.store(true, Ordering::Relaxed);
            }
        }
        let code = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("code")?.as_u64());
        Err((err, code))
    }

    async fn send_json<T: DeserializeOwned>(&self, method: Method, url: Url, body: Option<Value>) -> Result<T, ApiError> {
        let resp = self.send(method, url, body).await?;
        let bytes = resp.bytes().await.map_err(net_err)?;
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
    }

    pub async fn me(&self) -> Result<User, ApiError> {
        self.send_json(Method::GET, Self::url(&["users", "@me"]), None).await
    }

    /// Newest-first from Discord; returned as received (state sorts).
    pub async fn messages(&self, channel_id: Id, before: Option<Id>, limit: u8) -> Result<Vec<Message>, ApiError> {
        let mut url = Self::url(&["channels", &channel_id.to_string(), "messages"]);
        {
            let mut q = url.query_pairs_mut();
            if let Some(b) = before {
                q.append_pair("before", &b.to_string());
            }
            q.append_pair("limit", &limit.clamp(1, 100).to_string());
        }
        self.send_json(Method::GET, url, None).await
    }

    pub async fn send_message(&self, channel_id: Id, content: &str, nonce: &str, reply: Option<&ReplyTo>) -> Result<Message, ApiError> {
        let body = send_body(channel_id, content, nonce, reply);
        let url = Self::url(&["channels", &channel_id.to_string(), "messages"]);
        self.send_json(Method::POST, url, Some(body)).await
    }

    pub async fn edit_message(&self, channel_id: Id, message_id: Id, content: &str) -> Result<Message, ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "messages", &message_id.to_string()]);
        self.send_json(Method::PATCH, url, Some(json!({ "content": content }))).await
    }

    pub async fn delete_message(&self, channel_id: Id, message_id: Id) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "messages", &message_id.to_string()]);
        self.send(Method::DELETE, url, None).await.map(drop)
    }

    pub async fn add_reaction(&self, channel_id: Id, message_id: Id, emoji: &Emoji) -> Result<(), ApiError> {
        let e = reaction_path_emoji(emoji)?;
        let mut url = Self::url(&["channels", &channel_id.to_string(), "messages", &message_id.to_string(), "reactions", &e, "@me"]);
        url.query_pairs_mut().append_pair("location", "Message").append_pair("type", "0");
        self.send(Method::PUT, url, None).await.map(drop)
    }

    pub async fn remove_reaction(&self, channel_id: Id, message_id: Id, emoji: &Emoji) -> Result<(), ApiError> {
        let e = reaction_path_emoji(emoji)?;
        // `0` = normal (non-burst) reaction type.
        let mut url =
            Self::url(&["channels", &channel_id.to_string(), "messages", &message_id.to_string(), "reactions", &e, "0", "@me"]);
        url.query_pairs_mut().append_pair("location", "Message").append_pair("burst", "false");
        self.send(Method::DELETE, url, None).await.map(drop)
    }

    /// `thread`: read-state `flags` 3 (IS_GUILD_CHANNEL | IS_THREAD) for types 10/11/12.
    pub async fn ack(&self, channel_id: Id, message_id: Id, thread: bool) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "messages", &message_id.to_string(), "ack"]);
        self.send(Method::POST, url, Some(ack_body(thread))).await.map(drop)
    }
}

/// Body of POST /channels/{id}/messages, shaped like the web client's.
fn send_body(channel_id: Id, content: &str, nonce: &str, reply: Option<&ReplyTo>) -> Value {
    let mut body = json!({
        "mobile_network_type": "unknown",
        "content": content,
        "nonce": nonce,
        "tts": false,
        // SilentMessageToggle plugin: SUPPRESS_NOTIFICATIONS for the messages it marked.
        "flags": crate::plugins::chat::send_flags(nonce),
    });
    if let Some(r) = reply {
        let mut reference = json!({ "channel_id": channel_id, "message_id": r.message_id, "type": 0 });
        if let Some(g) = r.guild_id {
            reference["guild_id"] = json!(g);
        }
        body["message_reference"] = reference;
        // Explicit on every reply: whether an omitted allowed_mentions pings is unconfirmed.
        body["allowed_mentions"] = json!({
            "parse": ["users", "roles", "everyone"],
            "replied_user": r.mention_author,
        });
    }
    body
}

/// Path segment for a reaction: the unicode emoji itself (the URL builder percent-encodes
/// it) or `name:id` for custom emoji.
fn reaction_path_emoji(e: &Emoji) -> Result<String, ApiError> {
    match (e.id, e.name.as_deref()) {
        (Some(id), name) => Ok(format!("{}:{id}", name.filter(|n| !n.is_empty()).unwrap_or("_"))),
        (None, Some(name)) if !name.is_empty() => Ok(name.to_owned()),
        _ => Err(ApiError::Http { status: 400, message: "Unknown emoji.".into() }),
    }
}

fn ack_body(thread: bool) -> Value {
    let mut body = json!({ "token": null, "last_viewed": days_since_discord_epoch() });
    if thread {
        body["flags"] = json!(3);
    }
    body
}

fn days_since_discord_epoch() -> u64 {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(Id::DISCORD_EPOCH_MS);
    now_ms.saturating_sub(Id::DISCORD_EPOCH_MS) / 86_400_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_urls_are_encoded() {
        let thumbs = Emoji { id: None, name: Some("👍".into()), animated: false };
        let e = reaction_path_emoji(&thumbs).unwrap();
        let url = ApiClient::url(&["channels", "1", "messages", "2", "reactions", &e, "@me"]);
        assert_eq!(url.as_str(), "https://discord.com/api/v9/channels/1/messages/2/reactions/%F0%9F%91%8D/@me");

        let heart = Emoji { id: None, name: Some("❤️".into()), animated: false };
        let url = ApiClient::url(&["reactions", &reaction_path_emoji(&heart).unwrap()]);
        assert!(url.as_str().ends_with("/reactions/%E2%9D%A4%EF%B8%8F"));

        let custom = Emoji { id: Some(Id(123)), name: Some("blobcat".into()), animated: true };
        let url = ApiClient::url(&["reactions", &reaction_path_emoji(&custom).unwrap(), "@me"]);
        assert!(url.as_str().ends_with("/reactions/blobcat:123/@me"));

        assert!(reaction_path_emoji(&Emoji::default()).is_err());
    }

    #[test]
    fn thread_acks_carry_flags() {
        assert!(ack_body(false).get("flags").is_none());
        assert_eq!(ack_body(true)["flags"], 3);
    }

    #[test]
    fn send_body_reply_shapes() {
        let plain = send_body(Id(5), "hi", "99", None);
        assert_eq!(plain["tts"], false);
        assert_eq!(plain["flags"], 0);
        assert_eq!(plain["nonce"], "99");
        assert!(plain.get("message_reference").is_none());
        assert!(plain.get("allowed_mentions").is_none());

        let reply = ReplyTo { message_id: Id(7), guild_id: Some(Id(3)), mention_author: false };
        let b = send_body(Id(5), "hi", "99", Some(&reply));
        assert_eq!(b["message_reference"], json!({"guild_id":"3","channel_id":"5","message_id":"7","type":0}));
        assert_eq!(b["allowed_mentions"], json!({"parse":["users","roles","everyone"],"replied_user":false}));

        let dm_reply = ReplyTo { message_id: Id(7), guild_id: None, mention_author: true };
        let b = send_body(Id(5), "hi", "99", Some(&dm_reply));
        assert!(b["message_reference"].get("guild_id").is_none());
        assert_eq!(b["allowed_mentions"]["replied_user"], true);
    }

    #[test]
    fn errors_map_and_read_well() {
        assert!(matches!(error_from(401, b""), ApiError::Unauthorized));
        assert!(matches!(error_from(404, b"{}"), ApiError::NotFound));
        match error_from(403, br#"{"message":"Missing Permissions","code":50013}"#) {
            ApiError::Forbidden(m) => assert_eq!(m, "Missing Permissions"),
            e => panic!("{e:?}"),
        }
        let form = br#"{"code":50035,"message":"Invalid Form Body","errors":{"content":{"_errors":[{"code":"BASE_TYPE_MAX_LENGTH","message":"Must be 2000 or fewer in length."}]}}}"#;
        assert_eq!(error_from(400, form).to_string(), "Must be 2000 or fewer in length.");
        let captcha = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x","captcha_service":"hcaptcha"}"#;
        assert!(error_from(400, captcha).to_string().contains("captcha"));
        assert_eq!(error_from(502, b"<html>bad gateway</html>").to_string(), "Discord returned an error (HTTP 502).");
    }

    #[test]
    fn only_invalid_token_401s_log_out() {
        assert!(matches!(error_from(401, br#"{"message":"401: Unauthorized","code":0}"#), ApiError::Unauthorized));
        assert!(matches!(error_from(401, b"<html>nope</html>"), ApiError::Unauthorized));
        let mfa = br#"{"message":"Two factor is required for this operation","code":60003,"mfa":{"ticket":"SECRET_TICKET","methods":[{"type":"totp"}]}}"#;
        let e = error_from(401, mfa);
        match &e {
            ApiError::MfaRequired(c) => assert_eq!((c.ticket.expose(), c.methods.as_slice()), ("SECRET_TICKET", &[account::MfaKind::Totp][..])),
            e => panic!("{e:?}"),
        }
        assert!(!format!("{e} {e:?}").contains("SECRET_TICKET"));
        match error_from(401, br#"{"message":"Password does not match","code":50018}"#) {
            ApiError::Http { status: 401, message } => assert_eq!(message, "Password does not match"),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn set_token_replaces_the_header_and_lifts_a_401() {
        let props = Arc::new(props::ClientProps::new(&crate::config::Settings::default()));
        let (old, new) = (Token::parse(&"o".repeat(30)).unwrap(), Token::parse(&"n".repeat(30)).unwrap());
        let api = ApiClient::new(old.clone(), props);
        assert!(api.sends(&old));
        api.unauthorized.store(true, Ordering::Relaxed);
        {
            let _r = api.rotation();
            assert_eq!(api.rotating.load(Ordering::Relaxed), 1);
        }
        assert_eq!(api.rotating.load(Ordering::Relaxed), 0, "the guard ends the rotation");
        api.set_token(&new);
        assert!(api.sends(&new) && !api.sends(&old));
    }

    #[tokio::test]
    async fn mfa_header_goes_to_the_first_request_only() {
        let jwt = crate::auth::Secret::new("JWT".into());
        let taken = with_mfa(&jwt, async {
            let first = MFA_AUTH.try_with(|m| m.borrow_mut().take()).ok().flatten();
            let second = MFA_AUTH.try_with(|m| m.borrow_mut().take()).ok().flatten();
            (first, second)
        })
        .await;
        assert!(matches!(taken, (Some(v), None) if v == "JWT" && v.is_sensitive()));
        assert!(MFA_AUTH.try_with(|_| ()).is_err(), "outside a retry there is none");
    }

    #[test]
    fn extra_headers_are_validated() {
        let ok = extra_headers(&[("x-context-properties", "e30=".into())]).unwrap();
        assert_eq!(ok[0].0.as_str(), "x-context-properties");
        assert_eq!(ok[0].1, "e30=");
        assert!(extra_headers(&[("x-audit-log-reason", "line\nbreak".into())]).is_err());
        assert!(extra_headers(&[("authorization", "x".into())]).is_err());
        assert!(extra_headers(&[]).unwrap().is_empty());
    }

    #[test]
    fn ack_day_count_is_plausible() {
        // 2026 is ~4000 days after the 2015 Discord epoch.
        let d = days_since_discord_epoch();
        assert!((3500..20_000).contains(&d), "{d}");
    }
}
