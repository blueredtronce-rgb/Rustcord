//! Unauthenticated login calls: email/phone + password, 2FA, SMS code, QR ticket exchange
//! (docs/AUTH_NOTES.md). Separate from `ApiClient` on purpose: no `Authorization` header and no
//! `RateLimiter` (it re-sends on 429; Discord must never see an automatic second login attempt).
//! Request/response bodies are never logged: they carry the password, tickets and tokens.
//!
//! OWNERSHIP: email/QR login implementer.

use super::{API_BASE, default_headers, first_nested_error, net_err, props::ClientProps, ratelimit};
use crate::auth::{Secret, Token};
use crate::backend::MfaMethod;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, REFERER, SET_COOKIE};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const CAPTCHA_PASSWORD: &str = "Discord asks for a captcha after many sign-in attempts, and Rustcord cannot show one. Waiting a while helps. The Token tab also works.";
pub const CAPTCHA_QR: &str = CAPTCHA_PASSWORD;
pub const EXPIRED: &str = "The login expired. Enter your password again.";
pub const CAPTCHA_CANCELLED: &str = "The captcha wasn't finished, so Discord didn't sign you in. Try again when you're ready.";
pub const CAPTCHA_REJECTED: &str = "Discord didn't accept the captcha. Try again, or use QR code login or the Token tab.";

/// User-facing failure of a login call. Never contains the password, code, ticket or token.
#[derive(Debug, Clone, PartialEq)]
pub struct LoginError {
    pub message: String,
    /// The 2FA ticket is still usable (wrong code, network trouble, rate limit).
    pub keep_ticket: bool,
    /// Discord asked for an hCaptcha the user can solve; the same request goes out again with its
    /// answer (`CaptchaChallenge::answer`).
    pub captcha: Option<Box<CaptchaChallenge>>,
}

impl LoginError {
    fn new(message: impl Into<String>, keep_ticket: bool) -> Self {
        Self { message: message.into(), keep_ticket, captcha: None }
    }
}

/// The captcha of a 400 with a `captcha_key` field (Discord's "captcha handling" doc).
#[derive(Debug, Clone, PartialEq)]
pub struct CaptchaChallenge {
    pub sitekey: String,
    /// Enterprise data the widget must be given before the challenge starts.
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

/// The widget's token, to be sent as headers with the request Discord refused.
#[derive(Debug)]
pub struct CaptchaAnswer {
    pub key: Secret,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

impl CaptchaChallenge {
    pub fn answer(&self, key: Secret) -> CaptchaAnswer {
        CaptchaAnswer { key, rqtoken: self.rqtoken.clone(), session_id: self.session_id.clone() }
    }
}

/// hCaptcha only, with a sitekey that is safe to put into the captcha page (reCAPTCHA and
/// anything odd fall back to the plain text).
fn parse_captcha(v: &Value) -> Option<CaptchaChallenge> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let sitekey = text("captcha_sitekey").filter(|k| k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))?;
    (v.get("captcha_service").and_then(Value::as_str) == Some("hcaptcha")).then(|| CaptchaChallenge {
        sitekey,
        rqdata: text("captcha_rqdata"),
        rqtoken: text("captcha_rqtoken"),
        session_id: text("captcha_session_id"),
    })
}

/// `X-Captcha-Key`, plus `X-Captcha-Rqtoken` and `X-Captcha-Session-Id` when Discord sent those.
pub(crate) fn captcha_headers(a: &CaptchaAnswer) -> Vec<(&'static str, HeaderValue)> {
    let values = [("x-captcha-key", Some(a.key.expose())), ("x-captcha-rqtoken", a.rqtoken.as_deref()), ("x-captcha-session-id", a.session_id.as_deref())];
    values
        .into_iter()
        .filter_map(|(name, v)| {
            let mut v = HeaderValue::from_str(v?).ok()?;
            v.set_sensitive(true);
            Some((name, v))
        })
        .collect()
}

/// `RUSTCORD_CAPTCHA_DEMO`: the error of a login that needs a captcha, with hCaptcha's public
/// test sitekey (it passes without a question). Never comes from Discord.
pub(crate) fn demo_captcha() -> LoginError {
    let body = br#"{"captcha_key":["captcha-required"],"captcha_service":"hcaptcha","captcha_sitekey":"10000000-ffff-ffff-ffff-000000000001","captcha_rqtoken":"demo","captcha_session_id":"demo"}"#;
    classify(400, &HeaderMap::new(), body, CAPTCHA_PASSWORD)
}

#[derive(Debug)]
pub enum LoginOutcome {
    /// `update_password`: `required_actions` asks for a password change (AUTH_NOTES §1.7).
    Token { token: Token, update_password: bool },
    /// 2FA needed. `ticket` is valid for one login; `methods` is never empty.
    Mfa { ticket: Secret, login_instance_id: Option<String>, methods: Vec<MfaMethod> },
}

/// What Discord knows about this device before login.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Device {
    /// Discord's device cookies (`__dcfduid`, `__sdcfduid`, `_cfuvid`) as one `Cookie` value; a
    /// browser sends them on every login call and keeps them across logouts.
    cookies: String,
    fingerprint: Option<String>,
}

impl Device {
    /// Missing or unreadable file: a new device.
    fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, path: &Path) {
        // Never printed or logged: the values identify this device to Discord.
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = crate::config::write_private(path, &bytes);
        }
    }

    /// A login request is about to go out: fetch a fingerprint first unless one is stored.
    fn needs_fingerprint(&self) -> bool {
        self.fingerprint.is_none()
    }

    /// Takes the `Set-Cookie` lines of one response; true if the cookies changed.
    fn absorb(&mut self, headers: &HeaderMap) -> bool {
        let merged = merge_cookies(&self.cookies, headers.get_all(SET_COOKIE).iter().filter_map(|v| v.to_str().ok()));
        let changed = merged != self.cookies;
        self.cookies = merged;
        changed
    }
}

/// Tries per fingerprint fetch (the web client retries it twice).
const FINGERPRINT_TRIES: u32 = 3;

/// A login succeeded: the web client drops its fingerprint then. The cookies stay, like a
/// browser's across a logout.
pub fn forget_fingerprint(store: &Path) {
    let mut d = Device::load(store);
    if d.fingerprint.take().is_some() {
        d.save(store);
    }
}

pub struct AuthClient {
    http: reqwest::Client,
    device: Mutex<Device>,
    /// Where the device is kept between runs (None: memory only).
    store: Option<PathBuf>,
    fetching: tokio::sync::Mutex<()>,
}

impl AuthClient {
    pub fn new(props: &ClientProps, store: Option<PathBuf>) -> Self {
        let mut h = default_headers(props);
        h.insert(REFERER, HeaderValue::from_static("https://discord.com/login"));
        let http = reqwest::Client::builder()
            .user_agent(props.user_agent())
            .default_headers(h)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client (TLS backend) failed to initialise");
        let device = store.as_deref().map(Device::load).unwrap_or_default();
        Self { http, device: Mutex::new(device), store, fetching: tokio::sync::Mutex::new(()) }
    }

    /// GET /experiments for the `X-Fingerprint` header (3 valid ones per 2 minutes per IP), only
    /// while none is stored, up to 3 tries like the web client. Recommended, not required: if
    /// every try fails the request goes without it. Also keeps the device cookies it sets.
    async fn ensure_fingerprint(&self) {
        if !self.device.lock().unwrap().needs_fingerprint() {
            return;
        }
        // One fetch at a time: the QR flow and a password login may start together.
        let _one = self.fetching.lock().await;
        for attempt in 0..FINGERPRINT_TRIES {
            if !self.device.lock().unwrap().needs_fingerprint() {
                return;
            }
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let mut req = self.http.get(format!("{API_BASE}/experiments?with_guild_experiments=true"));
            if let Some(c) = self.cookie_header() {
                req = req.header(COOKIE, c);
            }
            let Ok(r) = req.send().await else { continue };
            let changed = self.device.lock().unwrap().absorb(r.headers());
            if changed {
                self.persist();
            }
            if !r.status().is_success() {
                continue;
            }
            let fp = r.json::<Value>().await.ok().and_then(|v| v.get("fingerprint")?.as_str().map(str::to_owned));
            self.device.lock().unwrap().fingerprint = fp.filter(|f| HeaderValue::from_str(f).is_ok());
            self.persist();
        }
    }

    fn persist(&self) {
        if let Some(path) = &self.store {
            let d = self.device.lock().unwrap().clone();
            d.save(path);
        }
    }

    fn cookie_header(&self) -> Option<HeaderValue> {
        let c = self.device.lock().unwrap().cookies.clone();
        HeaderValue::from_str(&c).ok().filter(|_| !c.is_empty()).map(|mut v| {
            v.set_sensitive(true);
            v
        })
    }

    /// The request with the stored fingerprint and cookies, and a captcha answer if there is one.
    fn request<B: Serialize + ?Sized>(&self, path: &str, body: &B, answer: Option<&CaptchaAnswer>) -> reqwest::RequestBuilder {
        let mut req = self.http.post(format!("{API_BASE}{path}")).json(body);
        let fp = self.device.lock().unwrap().fingerprint.clone().and_then(|f| HeaderValue::from_str(&f).ok());
        if let Some(fp) = fp {
            req = req.header("x-fingerprint", fp);
        }
        if let Some(c) = self.cookie_header() {
            req = req.header(COOKIE, c);
        }
        for (name, value) in answer.map(captcha_headers).unwrap_or_default() {
            req = req.header(name, value);
        }
        req
    }

    /// One POST, no retries. Non-2xx responses go through `classify`.
    async fn post<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        captcha: &str,
        answer: Option<&CaptchaAnswer>,
    ) -> Result<R, LoginError> {
        self.ensure_fingerprint().await;
        let req = self.request(path, body, answer);
        let network = |e| LoginError::new(net_err(e).to_string(), true);
        let resp = req.send().await.map_err(network)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        // Every response, errors included: the browser's jar takes them all.
        let changed = self.device.lock().unwrap().absorb(&headers);
        if changed {
            self.persist();
        }
        let bytes = resp.bytes().await.map_err(network)?;
        if !(200..300).contains(&status) {
            return Err(classify(status, &headers, &bytes, captcha));
        }
        serde_json::from_slice(&bytes).map_err(|_| LoginError::new("Unexpected response from Discord.", false))
    }

    /// POST /auth/login.
    pub async fn login(&self, login: &str, password: &Secret, answer: Option<&CaptchaAnswer>) -> Result<LoginOutcome, LoginError> {
        let login = normalize_login(login, password)?;
        // shortcut: the serialized request body (inside reqwest) is not zeroized; reqwest owns it.
        #[derive(Serialize)]
        struct Body<'a> {
            login: &'a str,
            password: &'a str,
            undelete: bool,
            login_source: Option<&'a str>,
            gift_code_sku_id: Option<&'a str>,
        }
        let body = Body { login: &login, password: password.expose(), undelete: false, login_source: None, gift_code_sku_id: None };
        let v: Value = self.post("/auth/login", &body, CAPTCHA_PASSWORD, answer).await?;
        parse_login(&v)
    }

    /// POST /auth/mfa/{totp|sms|backup}.
    pub async fn verify_mfa(
        &self,
        method: MfaMethod,
        code: &Secret,
        ticket: &Secret,
        login_instance_id: Option<&str>,
        answer: Option<&CaptchaAnswer>,
    ) -> Result<Token, LoginError> {
        let code = normalize_code(method, code)?;
        #[derive(Serialize)]
        struct Body<'a> {
            code: &'a str,
            ticket: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            login_instance_id: Option<&'a str>,
            login_source: Option<&'a str>,
            gift_code_sku_id: Option<&'a str>,
        }
        let path = match method {
            MfaMethod::Totp => "/auth/mfa/totp",
            MfaMethod::Sms => "/auth/mfa/sms",
            MfaMethod::Backup => "/auth/mfa/backup",
        };
        let body = Body { code: code.expose(), ticket: ticket.expose(), login_instance_id, login_source: None, gift_code_sku_id: None };
        let v: Value = self.post(path, &body, CAPTCHA_PASSWORD, answer).await?;
        token_field(&v, "token")
    }

    /// POST /auth/mfa/sms/send. Returns the masked phone number.
    pub async fn send_mfa_sms(&self, ticket: &Secret, answer: Option<&CaptchaAnswer>) -> Result<String, LoginError> {
        let v: Value = self.post("/auth/mfa/sms/send", &serde_json::json!({ "ticket": ticket.expose() }), CAPTCHA_PASSWORD, answer).await?;
        Ok(v.get("phone").and_then(Value::as_str).unwrap_or("your phone").to_owned())
    }

    /// POST /users/@me/remote-auth/login (QR flow). Returns `encrypted_token` (base64).
    pub async fn remote_auth_login(&self, ticket: &Secret, answer: Option<&CaptchaAnswer>) -> Result<String, LoginError> {
        let v: Value = self.post("/users/@me/remote-auth/login", &serde_json::json!({ "ticket": ticket.expose() }), CAPTCHA_QR, answer).await?;
        v.get("encrypted_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| LoginError::new("Unexpected response from Discord.", false))
    }
}

/// Merges `Set-Cookie` lines into a `Cookie` value ("a=1; b=2"): same name replaces, new appends.
// shortcut: ignores Expires/Max-Age=0 and Path; every cookie here comes from discord.com/api; upgrade if Discord starts expiring them mid-login.
fn merge_cookies<'a>(old: &str, set: impl Iterator<Item = &'a str>) -> String {
    let pair = |s: &str| s.split_once('=').map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned())).filter(|(n, _)| !n.is_empty());
    let mut jar: Vec<(String, String)> = old.split(';').filter_map(pair).collect();
    for line in set {
        let Some((n, v)) = line.split(';').next().and_then(pair) else { continue };
        match jar.iter_mut().find(|(k, _)| *k == n) {
            Some(slot) => slot.1 = v,
            None => jar.push((n, v)),
        }
    }
    jar.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ")
}

/// Trimmed login (phone numbers in E.164 without separators) after local checks, so a doomed
/// request is never sent.
fn normalize_login(login: &str, password: &Secret) -> Result<String, LoginError> {
    let login = login.trim();
    if login.is_empty() {
        return Err(LoginError::new("Enter your e-mail address or phone number.", false));
    }
    if !(8..=72).contains(&password.expose().chars().count()) {
        return Err(LoginError::new("The password must be 8 to 72 characters long.", false));
    }
    Ok(if login.starts_with('+') {
        login.chars().filter(|c| !matches!(c, ' ' | '-' | '(' | ')')).collect()
    } else {
        login.to_owned()
    })
}

/// Strips spaces and dashes; TOTP/SMS codes are 6 digits, backup codes 8 alphanumerics.
fn normalize_code(method: MfaMethod, code: &Secret) -> Result<Secret, LoginError> {
    // Pre-sized and lowercased in place: no unwiped intermediate copy of the code.
    let mut buf = String::with_capacity(code.expose().len());
    buf.extend(code.expose().chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_lowercase()));
    let c = Secret::new(buf);
    let s = c.expose();
    let ok = match method {
        MfaMethod::Totp | MfaMethod::Sms => s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit()),
        MfaMethod::Backup => s.len() == 8 && s.bytes().all(|b| b.is_ascii_alphanumeric()),
    };
    if ok {
        Ok(c)
    } else if method == MfaMethod::Backup {
        Err(LoginError::new("A backup code has 8 letters or digits.", true))
    } else {
        Err(LoginError::new("Enter the 6-digit code.", true))
    }
}

fn token_field(v: &Value, key: &str) -> Result<Token, LoginError> {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(Token::parse)
        .ok_or_else(|| LoginError::new("Unexpected response from Discord.", false))
}

/// Successful /auth/login body: a token, or a 2FA challenge.
fn parse_login(v: &Value) -> Result<LoginOutcome, LoginError> {
    if v.get("token").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
        let actions = v.get("required_actions").and_then(Value::as_array);
        let update_password = actions.is_some_and(|a| a.iter().any(|x| x.as_str() == Some("update_password")));
        return token_field(v, "token").map(|token| LoginOutcome::Token { token, update_password });
    }
    let ticket = v.get("ticket").and_then(Value::as_str);
    let (true, Some(ticket)) = (v.get("mfa").and_then(Value::as_bool) == Some(true), ticket) else {
        return Err(LoginError::new("Unexpected response from Discord.", false));
    };
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    let methods: Vec<MfaMethod> = [("totp", MfaMethod::Totp), ("sms", MfaMethod::Sms), ("backup", MfaMethod::Backup)]
        .into_iter()
        .filter(|(k, _)| flag(k))
        .map(|(_, m)| m)
        .collect();
    if methods.is_empty() {
        // Passkey/security-key only (webauthn): not possible in this native client.
        return Err(LoginError::new("This account only allows security-key 2FA. Use QR login instead.", false));
    }
    Ok(LoginOutcome::Mfa {
        ticket: Secret::new(ticket.to_owned()),
        login_instance_id: v.get("login_instance_id").and_then(Value::as_str).map(str::to_owned),
        methods,
    })
}

/// Any `errors.**._errors[].code == code` in a 50035 body.
fn has_form_error(v: &Value, code: &str) -> bool {
    match v {
        Value::Object(o) => {
            let here = o.get("_errors").and_then(Value::as_array).is_some_and(|errs| {
                errs.iter().any(|e| e.get("code").and_then(Value::as_str) == Some(code))
            });
            here || o.values().any(|x| has_form_error(x, code))
        }
        _ => false,
    }
}

/// Maps a non-2xx login response to user text (AUTH_NOTES §1.5, checked in that order).
fn classify(status: u16, headers: &HeaderMap, body: &[u8], captcha: &str) -> LoginError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if status == 429 {
        let hdr = |n: &str| headers.get(n).and_then(|h| h.to_str().ok());
        let retry = hdr("retry-after").and_then(|s| s.parse::<f64>().ok());
        let global = hdr("x-ratelimit-global").is_some_and(|s| s.eq_ignore_ascii_case("true"));
        return match ratelimit::parse_429(body, retry, global) {
            Some((secs, _)) => LoginError::new(format!("Too many attempts. Wait {} s, then try again.", secs.ceil() as u64), true),
            None => LoginError::new("Discord is blocking login attempts from this network for now. Try again later.", true),
        };
    }
    if status == 400 && v.get("captcha_key").is_some() {
        // Only the codes are logged ("captcha-required", "invalid-response"...), never a token.
        let codes: Vec<&str> = v["captcha_key"].as_array().map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
        eprintln!("rustcord: captcha asked ({})", codes.join(", "));
        // The ticket was not used up: the same request goes out again with the answer.
        return LoginError { message: captcha.into(), keep_ticket: true, captcha: parse_captcha(&v).map(Box::new) };
    }
    if status == 403 && v.get("suspended_user_token").is_some() {
        return LoginError::new("This account is suspended. See the official app for details.", false);
    }
    let discord_text = || {
        v.get("errors")
            .and_then(first_nested_error)
            .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_owned))
            .filter(|m| !m.is_empty())
    };
    let (msg, keep): (String, bool) = match v.get("code").and_then(Value::as_i64) {
        Some(20011 | 20013) => {
            ("This account is disabled or scheduled for deletion. Log in once in the official app to restore it.".into(), false)
        }
        Some(70007) => (
            "Discord wants to verify this new login location by phone. Log in once in the official app, or use QR login."
                .into(),
            false,
        ),
        Some(50035) if has_form_error(&v, "ACCOUNT_LOGIN_VERIFICATION_EMAIL") => (
            "New login location: Discord sent you an e-mail. Open the link in it, then log in again.".into(),
            false,
        ),
        Some(60008) => ("Wrong code. Try again.".into(), true),
        Some(60006 | 60009) => (EXPIRED.into(), false),
        Some(70003) => ("Discord could not send the SMS. Use another 2FA method.".into(), true),
        Some(70009) => (
            discord_text().unwrap_or_else(|| "Discord sent you an e-mail to reset your password.".into()),
            false,
        ),
        Some(40333) => ("Request blocked by Discord's firewall.".into(), false),
        Some(10008) if status == 403 => ("Discord blocked this login attempt.".into(), false),
        _ => (discord_text().unwrap_or_else(|| format!("Discord returned an error (HTTP {status}).")), false),
    };
    LoginError::new(msg, keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(status: u16, body: &str) -> LoginError {
        classify(status, &HeaderMap::new(), body.as_bytes(), CAPTCHA_PASSWORD)
    }

    #[test]
    fn classifies_errors_in_order() {
        let e = err(429, r#"{"message":"You are being rate limited.","retry_after":4.2,"global":false}"#);
        assert_eq!(e, LoginError::new("Too many attempts. Wait 5 s, then try again.", true));
        assert!(err(429, "<html>").message.contains("Try again later"));

        let captcha = r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x","captcha_service":"hcaptcha"}"#;
        assert_eq!(err(400, captcha).message, CAPTCHA_PASSWORD);
        assert_eq!(classify(400, &HeaderMap::new(), captcha.as_bytes(), CAPTCHA_QR).message, CAPTCHA_QR);
        let e = err(400, captcha);
        assert!(e.keep_ticket && e.captcha.is_some());
        assert!(err(400, r#"{"captcha_key":["captcha-required"],"captcha_service":"recaptcha"}"#).captcha.is_none());

        assert!(err(403, r#"{"user_id":"1","suspended_user_token":"x"}"#).message.contains("suspended"));
        assert!(err(400, r#"{"code":20013,"message":"This account is disabled"}"#).message.contains("disabled"));

        let invalid = r#"{"code":50035,"message":"Invalid Form Body","errors":{"login":{"_errors":[{"code":"INVALID_LOGIN","message":"Login or password is invalid."}]},"password":{"_errors":[{"code":"INVALID_LOGIN","message":"Login or password is invalid."}]}}}"#;
        assert_eq!(err(400, invalid), LoginError::new("Login or password is invalid.", false));
        let email = r#"{"code":50035,"message":"Invalid Form Body","errors":{"login":{"_errors":[{"code":"ACCOUNT_LOGIN_VERIFICATION_EMAIL","message":"New login location detected, please check your e-mail."}]}}}"#;
        assert!(err(400, email).message.contains("e-mail"));

        assert_eq!(err(400, r#"{"code":60008,"message":"Invalid two-factor code"}"#), LoginError::new("Wrong code. Try again.", true));
        assert_eq!(err(400, r#"{"code":60006,"message":"Invalid two-factor auth ticket"}"#), LoginError::new(EXPIRED, false));
        assert!(err(400, r#"{"code":70003,"message":"Unable to send sms message"}"#).keep_ticket);
        assert!(err(403, r#"{"code":40333,"message":"internal network error"}"#).message.contains("firewall"));
        assert!(err(403, r#"{"code":10008,"message":"Unknown Message"}"#).message.contains("blocked"));
        assert_eq!(err(502, "bad gateway").message, "Discord returned an error (HTTP 502).");
    }

    #[test]
    fn parses_a_captcha_challenge() {
        let full = serde_json::json!({
            "captcha_key": ["captcha-required"], "captcha_service": "hcaptcha",
            "captcha_sitekey": "4c672d35-0701-42b2-88c3-78380b0db560", "captcha_session_id": "sess",
            "captcha_rqdata": "data", "captcha_rqtoken": "tok", "should_serve_invisible": false
        });
        let ch = parse_captcha(&full).unwrap();
        assert_eq!(ch.sitekey, "4c672d35-0701-42b2-88c3-78380b0db560");
        assert_eq!((ch.rqdata.as_deref(), ch.rqtoken.as_deref(), ch.session_id.as_deref()), (Some("data"), Some("tok"), Some("sess")));
        let a = ch.answer(Secret::new("P1_x".into()));
        assert_eq!((a.key.expose(), a.rqtoken.as_deref(), a.session_id.as_deref()), ("P1_x", Some("tok"), Some("sess")));

        let minimal = serde_json::json!({"captcha_service": "hcaptcha", "captcha_sitekey": "k", "captcha_rqdata": ""});
        assert_eq!(parse_captcha(&minimal), Some(CaptchaChallenge { sitekey: "k".into(), rqdata: None, rqtoken: None, session_id: None }));
        let with = |k: &str, v: &str| serde_json::json!({"captcha_service": "hcaptcha", k: v});
        assert!(parse_captcha(&serde_json::json!({"captcha_service": "recaptcha", "captcha_sitekey": "k"})).is_none());
        assert!(parse_captcha(&serde_json::json!({"captcha_service": "hcaptcha"})).is_none());
        assert!(parse_captcha(&with("captcha_sitekey", "</script>")).is_none());
        assert!(parse_captcha(&with("captcha_sitekey", &"a".repeat(65))).is_none());
        assert!(demo_captcha().captcha.is_some());
    }

    #[test]
    fn the_answer_goes_out_as_sensitive_headers() {
        let props = ClientProps::new(&crate::config::Settings::default());
        let c = AuthClient::new(&props, None);
        let full = CaptchaAnswer { key: Secret::new("tok".into()), rqtoken: Some("rq".into()), session_id: Some("sid".into()) };
        let req = c.request("/auth/login", &serde_json::json!({}), Some(&full)).build().unwrap();
        for (name, want) in [("x-captcha-key", "tok"), ("x-captcha-rqtoken", "rq"), ("x-captcha-session-id", "sid")] {
            let v = req.headers().get(name).unwrap();
            assert!(v.is_sensitive() && v == want, "{name}");
        }
        let key_only = CaptchaAnswer { key: Secret::new("tok".into()), rqtoken: None, session_id: None };
        let req = c.request("/auth/login", &serde_json::json!({}), Some(&key_only)).build().unwrap();
        assert!(req.headers().get("x-captcha-key").is_some() && req.headers().get("x-captcha-rqtoken").is_none());
        let req = c.request("/auth/login", &serde_json::json!({}), None).build().unwrap();
        assert!(req.headers().keys().all(|k| !k.as_str().starts_with("x-captcha")));
    }

    #[test]
    fn every_response_updates_the_device_cookies() {
        let resp = |lines: &[&str]| {
            let mut h = HeaderMap::new();
            for l in lines {
                h.append(SET_COOKIE, HeaderValue::from_str(l).unwrap());
            }
            h
        };
        let mut d = Device::default();
        assert!(d.absorb(&resp(&["__dcfduid=a; Path=/", "_cfuvid=b"])));
        // A login, MFA or QR-exchange answer (also an error one) can replace a cookie.
        assert!(d.absorb(&resp(&["_cfuvid=c; HttpOnly"])));
        assert_eq!(d.cookies, "__dcfduid=a; _cfuvid=c");
        assert!(!d.absorb(&resp(&["_cfuvid=c"])) && !d.absorb(&HeaderMap::new()));
    }

    #[test]
    fn a_fingerprint_is_fetched_only_while_none_is_stored() {
        let mut d = Device::default();
        assert!(d.needs_fingerprint());
        d.fingerprint = Some("1.abc".into());
        assert!(!d.needs_fingerprint());
        d.fingerprint = None;
        assert!(d.needs_fingerprint());
    }

    #[test]
    fn the_device_survives_a_restart_but_not_a_login() {
        let dir = std::env::temp_dir().join(format!("rustcord-device-test-{}", std::process::id()));
        let file = dir.join("device.json");
        assert_eq!(Device::load(&file), Device::default());
        let d = Device { cookies: "__dcfduid=a; _cfuvid=b".into(), fingerprint: Some("1.abc".into()) };
        d.save(&file);
        assert_eq!(Device::load(&file), d);
        // A new client picks both up.
        let props = ClientProps::new(&crate::config::Settings::default());
        let c = AuthClient::new(&props, Some(file.clone()));
        assert_eq!(*c.device.lock().unwrap(), d);
        assert!(c.cookie_header().is_some());
        // Login success: the fingerprint goes, the cookies stay.
        forget_fingerprint(&file);
        assert_eq!(Device::load(&file), Device { fingerprint: None, ..d });
        std::fs::write(&file, b"not json").unwrap();
        assert_eq!(Device::load(&file), Device::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merges_cookies() {
        let set = ["__dcfduid=a; Expires=Fri, 14-Nov-2031 03:22:46 GMT; Path=/", "_cfuvid=b; HttpOnly", "garbage"];
        assert_eq!(merge_cookies("", set.into_iter()), "__dcfduid=a; _cfuvid=b");
        assert_eq!(merge_cookies("__dcfduid=a; _cfuvid=b", ["_cfuvid=c; Path=/"].into_iter()), "__dcfduid=a; _cfuvid=c");
        assert_eq!(merge_cookies("", std::iter::empty()), "");
    }

    #[test]
    fn parses_login_responses() {
        let ok = serde_json::json!({"user_id":"1","token":"abcdefghijklmnopqrstuvwxyz.123","user_settings":{"locale":"en-US","theme":"dark"}});
        assert!(matches!(parse_login(&ok), Ok(LoginOutcome::Token { token, update_password: false }) if token.expose() == "abcdefghijklmnopqrstuvwxyz.123"));
        let mut pw = ok.clone();
        pw["required_actions"] = serde_json::json!(["update_password"]);
        assert!(matches!(parse_login(&pw), Ok(LoginOutcome::Token { update_password: true, .. })));

        let mfa = serde_json::json!({"user_id":"1","mfa":true,"ticket":"TICKET","login_instance_id":"li","totp":true,"sms":false,"backup":true,"webauthn":null});
        match parse_login(&mfa) {
            Ok(LoginOutcome::Mfa { ticket, login_instance_id, methods }) => {
                assert_eq!(ticket.expose(), "TICKET");
                assert_eq!(login_instance_id.as_deref(), Some("li"));
                assert_eq!(methods, vec![MfaMethod::Totp, MfaMethod::Backup]);
            }
            other => panic!("{other:?}"),
        }
        // Debug never shows the ticket.
        assert!(!format!("{:?}", parse_login(&mfa)).contains("TICKET"));

        let passkey = serde_json::json!({"mfa":true,"ticket":"t","totp":false,"sms":false,"backup":false,"webauthn":"{}"});
        assert!(parse_login(&passkey).unwrap_err().message.contains("security-key"));
        assert!(parse_login(&serde_json::json!({"user_id":"1"})).is_err());
    }

    #[test]
    fn normalizes_inputs() {
        let pw = Secret::new("hunter2hunter2".into());
        assert_eq!(normalize_login(" +49 (170) 123-4567 ", &pw).unwrap(), "+491701234567");
        assert_eq!(normalize_login(" a@b.c ", &pw).unwrap(), "a@b.c");
        assert!(normalize_login("", &pw).is_err());
        assert!(normalize_login("a@b.c", &Secret::new("short".into())).is_err());

        let code = |m, s: &str| normalize_code(m, &Secret::new(s.into())).map(|c| c.expose().to_owned());
        assert_eq!(code(MfaMethod::Totp, " 123 456 ").unwrap(), "123456");
        assert_eq!(code(MfaMethod::Backup, "ZQS8-OQXK").unwrap(), "zqs8oqxk");
        assert!(code(MfaMethod::Sms, "12345").is_err());
        assert!(code(MfaMethod::Totp, "12345a").unwrap_err().keep_ticket);
    }
}
