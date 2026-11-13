//! REST rate limiting: per-bucket (route + major parameter) state from X-RateLimit-* headers,
//! global limit, 429 retry_after handling. Requests on the same route are serialized.
//!
//! OWNERSHIP: api implementer.
//!
//! User tokens usually get rate-limit headers only on 429s, so the limiter is mostly
//! 429-driven: it serializes requests per bucket, obeys `retry_after`, and uses the
//! `X-RateLimit-*` headers whenever Discord does send them.

use super::ApiError;
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

/// Retries after a 429 before giving up with `ApiError::RateLimited`.
const MAX_429_RETRIES: u32 = 3;
/// Added to every `retry_after` so we never arrive a hair too early.
const RETRY_MARGIN: Duration = Duration::from_millis(100);
/// A user-triggered request should not hang for minutes: longer waits fail fast instead.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Discord's global limit for user tokens.
const GLOBAL_PER_SEC: u32 = 50;
/// Spacing between reaction requests on one channel (commonly cited 1 per 0.25 s).
const REACTION_GAP: Duration = Duration::from_millis(300);

/// Bucket identity of a request: route template (minor ids collapsed) + major parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteKey {
    /// e.g. `"POST /channels/{channel_id}/messages"`.
    pub route: String,
    /// channel id, guild id, `webhook_id/token`, or empty.
    pub major: String,
}

impl RouteKey {
    fn is_reaction(&self) -> bool {
        self.route.starts_with("REACTION ")
    }
}

/// Derive the bucket key from a method and an API path (with or without the `/api/v9`
/// prefix, without query string).
pub fn route_key(method: &str, path: &str) -> RouteKey {
    let path = path.strip_prefix("/api/v9").unwrap_or(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out: Vec<&str> = Vec::with_capacity(segs.len());
    let mut major = String::new();
    let mut i = 0;
    while i < segs.len() {
        let seg = segs[i];
        let prev = if i > 0 { segs[i - 1] } else { "" };
        let is_id = seg.bytes().all(|b| b.is_ascii_digit());
        let templ = match prev {
            "channels" if is_id && major.is_empty() => {
                major = seg.to_owned();
                "{channel_id}"
            }
            "guilds" if is_id && major.is_empty() => {
                major = seg.to_owned();
                "{guild_id}"
            }
            "webhooks" if is_id && major.is_empty() => {
                major = seg.to_owned();
                // The webhook token (next segment) is part of the major parameter.
                if let Some(token) = segs.get(i + 1) {
                    major.push('/');
                    major.push_str(token);
                    out.extend(["{webhook_id}", "{webhook_token}"]);
                    i += 2;
                    continue;
                }
                "{webhook_id}"
            }
            "reactions" => "{emoji}",
            // Free-form keys would make one bucket per value.
            "invites" | "templates" | "gift-codes" => "{code}",
            "attachments" if !is_id => "{upload}",
            "streams" => "{stream_key}",
            _ if is_id => "{id}",
            _ => seg,
        };
        out.push(templ);
        i += 1;
    }
    // Add and remove reaction share one serialized bucket per channel; listing who reacted
    // (GET) keeps its own route.
    let route = if out.contains(&"reactions") && method != "GET" {
        "REACTION /channels/{channel_id}/messages/{id}/reactions".to_owned()
    } else {
        format!("{method} /{}", out.join("/"))
    };
    RouteKey { route, major }
}

/// Timing state of one bucket. Times are injected so the logic is testable.
#[derive(Debug, Default)]
pub struct Bucket {
    remaining: Option<u32>,
    reset_at: Option<Instant>,
    last_sent: Option<Instant>,
    min_gap: Duration,
}

impl Bucket {
    fn with_gap(min_gap: Duration) -> Self {
        Self { min_gap, ..Self::default() }
    }

    /// How long to wait before the next request may be sent.
    pub fn delay(&self, now: Instant) -> Duration {
        let exhausted = match (self.remaining, self.reset_at) {
            (Some(0), Some(reset)) => reset.saturating_duration_since(now),
            _ => Duration::ZERO,
        };
        let spacing = match self.last_sent {
            Some(last) => (last + self.min_gap).saturating_duration_since(now),
            None => Duration::ZERO,
        };
        exhausted.max(spacing)
    }

    /// Record a sent request and whatever X-RateLimit headers came back.
    pub fn on_response(&mut self, now: Instant, remaining: Option<u32>, reset_after: Option<f64>) {
        self.last_sent = Some(now);
        if let Some(r) = remaining {
            self.remaining = Some(r);
        }
        if let Some(secs) = reset_after {
            self.reset_at = Some(now + secs_to_duration(secs));
        }
    }

    /// A 429 on this bucket: nothing may be sent for `retry_after` (+ margin).
    pub fn on_429(&mut self, now: Instant, retry_after: f64) {
        self.remaining = Some(0);
        self.reset_at = Some(now + secs_to_duration(retry_after) + RETRY_MARGIN);
    }
}

/// Global gate: a global 429 block plus a 50 requests/second window.
#[derive(Debug)]
pub struct Global {
    blocked_until: Option<Instant>,
    window_start: Instant,
    count: u32,
}

impl Global {
    fn new(now: Instant) -> Self {
        Self { blocked_until: None, window_start: now, count: 0 }
    }

    /// Takes a slot if one is free (returns zero), else returns how long to wait.
    pub fn reserve(&mut self, now: Instant) -> Duration {
        if let Some(until) = self.blocked_until {
            if until > now {
                return until - now;
            }
            self.blocked_until = None;
        }
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= GLOBAL_PER_SEC {
            return (self.window_start + Duration::from_secs(1)).saturating_duration_since(now);
        }
        self.count += 1;
        Duration::ZERO
    }

    pub fn block(&mut self, now: Instant, retry_after: f64) {
        let until = now + secs_to_duration(retry_after) + RETRY_MARGIN;
        self.blocked_until = Some(self.blocked_until.map_or(until, |u| u.max(until)));
    }
}

fn secs_to_duration(secs: f64) -> Duration {
    // Guards against NaN/negative/absurd values from the network.
    Duration::try_from_secs_f64(secs.clamp(0.0, 86_400.0)).unwrap_or_default()
}

/// Rate-limit relevant response headers.
#[derive(Debug, Default)]
struct RateHeaders {
    remaining: Option<u32>,
    reset_after: Option<f64>,
    bucket: Option<String>,
    retry_after: Option<f64>,
    global: bool,
}

impl RateHeaders {
    fn parse(h: &HeaderMap) -> Self {
        let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
        Self {
            remaining: get("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
            reset_after: get("x-ratelimit-reset-after").and_then(|v| v.parse().ok()),
            bucket: get("x-ratelimit-bucket").filter(|v| !v.is_empty()).map(str::to_owned),
            retry_after: get("retry-after").and_then(|v| v.parse().ok()),
            global: get("x-ratelimit-global").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        }
    }
}

/// `(retry_after seconds, global)` of a 429, or None when it must not be retried
/// (Cloudflare ban: no JSON body and no Retry-After header).
pub fn parse_429(body: &[u8], header_retry: Option<f64>, header_global: bool) -> Option<(f64, bool)> {
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let body_retry = json.as_ref().and_then(|v| v.get("retry_after")?.as_f64());
    let global = header_global || json.as_ref().and_then(|v| v.get("global")?.as_bool()) == Some(true);
    let secs = match (body_retry, header_retry) {
        // Cloudflare sometimes reports milliseconds in the body.
        (Some(b), Some(h)) if b >= 1000.0 && (b / 1000.0 - h).abs() < 1.0 => h,
        (Some(b), _) => b,
        (None, Some(h)) => h,
        (None, None) => return None,
    };
    Some((secs.max(0.0), global))
}

struct Shared {
    /// route template -> learned X-RateLimit-Bucket hash.
    hashes: HashMap<String, String>,
    /// bucket id (`route:major` or `hash:major`) -> state; the async mutex serializes requests.
    // shortcut: never pruned (one small entry per channel/route used), fine for a session.
    buckets: HashMap<String, Arc<AsyncMutex<Bucket>>>,
    global: Global,
}

pub struct RateLimiter {
    shared: Mutex<Shared>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            shared: Mutex::new(Shared {
                hashes: HashMap::new(),
                buckets: HashMap::new(),
                global: Global::new(Instant::now()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bucket_for(&self, key: &RouteKey) -> Arc<AsyncMutex<Bucket>> {
        let mut s = self.lock();
        let id = match s.hashes.get(&key.route) {
            Some(hash) => format!("{hash}:{}", key.major),
            None => format!("{}:{}", key.route, key.major),
        };
        let gap = if key.is_reaction() { REACTION_GAP } else { Duration::ZERO };
        s.buckets.entry(id).or_insert_with(|| Arc::new(AsyncMutex::new(Bucket::with_gap(gap)))).clone()
    }

    /// Remember the bucket hash of a route; the current state object keeps serving it.
    fn learn_hash(&self, key: &RouteKey, hash: &str, bucket: &Arc<AsyncMutex<Bucket>>) {
        let mut s = self.lock();
        if s.hashes.get(&key.route).map(String::as_str) != Some(hash) {
            s.hashes.insert(key.route.clone(), hash.to_owned());
        }
        s.buckets.entry(format!("{hash}:{}", key.major)).or_insert_with(|| bucket.clone());
    }

    async fn wait_global(&self) -> Result<(), ApiError> {
        loop {
            let wait = self.lock().global.reserve(Instant::now());
            if wait.is_zero() {
                return Ok(());
            }
            if wait > MAX_WAIT {
                return Err(ApiError::RateLimited);
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Send a request through the limiter. `send` builds and sends a fresh request each
    /// time it is called (it is called again for 429 retries). Returns the first non-429
    /// response; status handling beyond 429 is the caller's job.
    pub async fn execute<F, Fut>(&self, key: &RouteKey, mut send: F) -> Result<reqwest::Response, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let bucket = self.bucket_for(key);
        let mut state = bucket.lock().await;
        let mut retries = 0;
        loop {
            let wait = state.delay(Instant::now());
            if wait > MAX_WAIT {
                return Err(ApiError::RateLimited);
            }
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            self.wait_global().await?;

            let resp = send().await.map_err(super::net_err)?;
            let now = Instant::now();
            let h = RateHeaders::parse(resp.headers());
            if let Some(hash) = &h.bucket {
                self.learn_hash(key, hash, &bucket);
            }
            state.on_response(now, h.remaining, h.reset_after);
            if resp.status().as_u16() != 429 {
                return Ok(resp);
            }

            let body = resp.bytes().await.unwrap_or_default();
            let Some((retry_after, global)) = parse_429(&body, h.retry_after, h.global) else {
                return Err(ApiError::RateLimited);
            };
            if global {
                self.lock().global.block(now, retry_after);
            } else {
                state.on_429(now, retry_after);
            }
            retries += 1;
            if retries > MAX_429_RETRIES {
                return Err(ApiError::RateLimited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_keys_keep_major_params_only() {
        let k = route_key("GET", "/api/v9/channels/123/messages");
        assert_eq!(k, RouteKey { route: "GET /channels/{channel_id}/messages".into(), major: "123".into() });

        let k = route_key("PATCH", "/api/v9/channels/123/messages/456");
        assert_eq!(k.route, "PATCH /channels/{channel_id}/messages/{id}");
        assert_eq!(k.major, "123");

        let k = route_key("POST", "/channels/123/messages/456/ack");
        assert_eq!(k.route, "POST /channels/{channel_id}/messages/{id}/ack");

        let k = route_key("GET", "/api/v9/users/@me");
        assert_eq!(k, RouteKey { route: "GET /users/@me".into(), major: String::new() });

        let k = route_key("GET", "/guilds/77/members/88");
        assert_eq!(k.route, "GET /guilds/{guild_id}/members/{id}");
        assert_eq!(k.major, "77");

        let k = route_key("POST", "/webhooks/55/tok-en_X");
        assert_eq!(k.route, "POST /webhooks/{webhook_id}/{webhook_token}");
        assert_eq!(k.major, "55/tok-en_X");
    }

    #[test]
    fn free_form_segments_become_placeholders() {
        let invite = |code: &str| route_key("POST", &format!("/api/v9/invites/{code}"));
        assert_eq!(invite("abcDEF"), invite("x9"));
        assert_eq!(invite("abcDEF").route, "POST /invites/{code}");
        assert_eq!(route_key("GET", "/guilds/templates/hgM48av5Q69A").route, "GET /guilds/templates/{code}");
        assert_eq!(route_key("GET", "/entitlements/gift-codes/AbC123").route, "GET /entitlements/gift-codes/{code}");
        // Upload names are one percent-encoded segment.
        let k = route_key("DELETE", "/api/v9/attachments/a1b2c3d4-uuid%2Fimage.png");
        assert_eq!(k, RouteKey { route: "DELETE /attachments/{upload}".into(), major: String::new() });
        let k = route_key("GET", "/streams/guild:1:2:3/preview");
        assert_eq!(k.route, "GET /streams/{stream_key}/preview");
        assert_eq!(route_key("GET", "/streams/call:5:6/preview"), k);
        // Ids keep the usual handling.
        assert_eq!(route_key("GET", "/channels/1/attachments/22").route, "GET /channels/{channel_id}/attachments/{id}");
    }

    #[test]
    fn reaction_routes_share_one_bucket_per_channel() {
        let add = route_key("PUT", "/api/v9/channels/1/messages/2/reactions/%F0%9F%91%8D/@me");
        let del = route_key("DELETE", "/api/v9/channels/1/messages/3/reactions/blob:99/0/@me");
        assert_eq!(add, del);
        assert_eq!(add.major, "1");
        assert!(add.is_reaction());
        let other = route_key("PUT", "/api/v9/channels/9/messages/2/reactions/x/@me");
        assert_ne!(add, other);
        let list = route_key("GET", "/api/v9/channels/1/messages/2/reactions/x");
        assert!(!list.is_reaction());
        assert_eq!(list.route, "GET /channels/{channel_id}/messages/{id}/reactions/{emoji}");
    }

    #[test]
    fn bucket_waits_until_reset_when_exhausted() {
        let t0 = Instant::now();
        let mut b = Bucket::default();
        assert_eq!(b.delay(t0), Duration::ZERO);
        b.on_response(t0, Some(1), Some(2.0));
        assert_eq!(b.delay(t0), Duration::ZERO);
        b.on_response(t0, Some(0), Some(2.0));
        assert_eq!(b.delay(t0), Duration::from_secs(2));
        assert_eq!(b.delay(t0 + Duration::from_millis(1500)), Duration::from_millis(500));
        assert_eq!(b.delay(t0 + Duration::from_secs(3)), Duration::ZERO);
        // Headers absent (typical for user tokens): state is kept, nothing else changes.
        b.on_response(t0 + Duration::from_secs(3), None, None);
        assert_eq!(b.delay(t0 + Duration::from_secs(3)), Duration::ZERO);
    }

    #[test]
    fn bucket_429_and_min_gap() {
        let t0 = Instant::now();
        let mut b = Bucket::default();
        b.on_response(t0, Some(5), None);
        b.on_429(t0, 1.5);
        assert_eq!(b.delay(t0), Duration::from_millis(1500) + RETRY_MARGIN);

        let mut r = Bucket::with_gap(REACTION_GAP);
        r.on_response(t0, None, None);
        assert_eq!(r.delay(t0 + Duration::from_millis(100)), Duration::from_millis(200));
        assert_eq!(r.delay(t0 + REACTION_GAP), Duration::ZERO);
    }

    #[test]
    fn global_window_and_block() {
        let t0 = Instant::now();
        let mut g = Global::new(t0);
        for _ in 0..GLOBAL_PER_SEC {
            assert_eq!(g.reserve(t0), Duration::ZERO);
        }
        assert_eq!(g.reserve(t0 + Duration::from_millis(400)), Duration::from_millis(600));
        assert_eq!(g.reserve(t0 + Duration::from_secs(1)), Duration::ZERO);

        let t1 = t0 + Duration::from_secs(5);
        g.block(t1, 2.0);
        assert_eq!(g.reserve(t1), Duration::from_secs(2) + RETRY_MARGIN);
        assert_eq!(g.reserve(t1 + Duration::from_secs(3)), Duration::ZERO);
    }

    #[test]
    fn parse_429_variants() {
        let body = br#"{"message":"You are being rate limited.","retry_after":0.75,"global":false}"#;
        assert_eq!(parse_429(body, Some(1.0), false), Some((0.75, false)));
        // Body wins over the header.
        assert_eq!(parse_429(br#"{"retry_after":776,"global":true}"#, Some(1337.0), false), Some((776.0, true)));
        // Integer retry_after and global header.
        assert_eq!(parse_429(br#"{"retry_after":3}"#, None, true), Some((3.0, true)));
        // Cloudflare: milliseconds in the body.
        assert_eq!(parse_429(br#"{"retry_after":5000}"#, Some(5.0), false), Some((5.0, false)));
        // HTML body: only the header counts; without it, never retry.
        assert_eq!(parse_429(b"<html>banned</html>", Some(10.0), false), Some((10.0, false)));
        assert_eq!(parse_429(b"<html>banned</html>", None, false), None);
    }
}
