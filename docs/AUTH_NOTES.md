# Discord user login for Rustcord: password + 2FA, and QR code (remote auth)

Edited and checked on 2026-11-11 against the sources listed at the end.

- The user logs into **their own** account.
- Base URL: `API_BASE = https://discord.com/api/v9` (`src/api/mod.rs`).

Legend:
- **UD**: Discord Userdoccers. The raw MDX lives at `raw.githubusercontent.com/discord-userdoccers/discord-userdoccers/master/pages/...` and is rendered at docs.discord.food.
- **UNCONFIRMED**: no fetched source states it.
- **INFERENCE**: an editor's conclusion drawn from the sources, not stated by them.

Captchas are covered **only as detection**. This file deliberately leaves out how to solve them, submit them, or get around them.

---

## 0. Shared facts for both flows (Rustcord specifics, checked in the code)

| Fact | Consequence |
|---|---|
| `ApiClient::send` always adds `Authorization`, and returns `Unauthorized` when it has no token. | Login calls **cannot** go through `ApiClient`. |
| `RateLimiter::execute` re-sends on 429 by itself (`ratelimit.rs`: "called again for 429 retries"). | Login calls must **not** use the limiter. Discord must never see an automatic second login attempt. |
| `error_from()` throws away the JSON `code`, `retry_after` and `suspended_user_token`. 429 becomes `RateLimited` and 403 becomes `Forbidden(msg)`. | Login needs its own classifier that works on the raw `(status, body bytes)` (see §1.5). |
| Reusable pieces: `default_headers(&props)` (private, make it `pub(crate)`), `props.user_agent()`, `ratelimit::parse_429(body, header_retry, header_global)`, `first_nested_error` (private), and `gateway/conn.rs::upgrade_request(url, &props)` (private, make it `pub(crate)`; it sets UA, Origin, Accept-Language and Cache-Control). | No new HTTP or WebSocket plumbing is needed. |
| The token from either flow should go through `Token::parse(&s)`, then the **same path as `Command::Login { token, remember }`**: `GET /users/@me`, keyring save if `remember`, then `Session::start`. | No second login path. |
| `Secret` (zeroized, redacted Debug) already wraps the password and the 2FA code. | Never log request or response bodies: they echo the login and contain tickets and tokens. |

**Unauthenticated HTTP client (one per login attempt, no retries):**

```rust
let mut h = default_headers(&props);                  // make pub(crate)
h.insert(header::REFERER, HeaderValue::from_static("https://discord.com/login"));
// h.insert("x-fingerprint", fp) once §1.1 has one
let http = reqwest::Client::builder()
    .user_agent(props.user_agent())                   // must equal super-props browser_user_agent (UD reference)
    .default_headers(h)
    .redirect(reqwest::redirect::Policy::none())
    .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(30))
    .build()?;
// classify(status.as_u16(), &resp.headers(), &resp.bytes().await?) per §1.5. Never loop on errors.
```

**Captcha detection (both flows; UD topics/captcha-handling.mdx):**
- Detect it as **HTTP 400 and a `captcha_key` key present in the body**.
- Do **not** match on the strings inside the array, because they vary.
- Other fields that may appear, for detection or logging only:
  - `captcha_service` (`hcaptcha`, `recaptcha` or `recaptcha_enterprise`)
  - `captcha_sitekey` (`?string`)
  - `captcha_session_id?`, `captcha_rqdata?`, `captcha_rqtoken?`
  - `should_serve_invisible?`
- The body has **no** `code` or `message`, so check for a captcha **before** looking at `code`.
- UD names login, invite acceptance and friend requests as captcha endpoints, and says *any* endpoint may return one. By INFERENCE that includes `/auth/mfa/*`, `/auth/mfa/sms/send`, `/auth/authorize-ip` and `/users/@me/remote-auth/login`.
- `error_from` already does this check (`api/mod.rs:80`). Use the same predicate in the login classifier.
- User text:
  - Password flow: "Discord wants a captcha for this login. Use QR login, or sign in once in the official app, then try again."
  - QR exchange: "... complete the login in the official app."
- Abaddon checks `captcha_service` instead. Prefer UD's `captcha_key`.

---

# Part 1: Password + 2FA login

## 1.1 Pre-login fingerprint (UD topics/experiments.mdx, authentication.mdx §Fingerprints)

**Request:** `GET /experiments?with_guild_experiments=true`. Send **no** `Authorization` and **no** `X-Fingerprint`, because UD says a fingerprint is generated only when neither is present. Acheron sends the query flag; it is optional.

**Response:**
```json
{
  "fingerprint": "1084179945133187083.JQddgNMmwJPghoBtFmaH7jTmdsw",
  "assignments": [...],
  "guild_experiments": [...]
}
```
- `fingerprint` is optional in the response. Its format is `<snowflake>.<hash>`.

**Using it:**
- Send it as the `X-Fingerprint` header on every unauthenticated call until auth completes: `/auth/login`, `/auth/mfa/*`, `/auth/mfa/sms/send`, `/auth/authorize-ip`, `/phone-verifications/*`, and by INFERENCE `/users/@me/remote-auth/login`.
- Stop sending it once you have a token.
- UD also says to put it in any "applicable `fingerprint` JSON parameter". `/auth/login` has none.

**Rate limit:** 3 *valid* fingerprints per 2 minutes per IP. Above that a fingerprint is still returned but is **invalid**. Fetch one per login screen session and reuse it across retries.

**Optional:**
- `POST /auth/fingerprint` → `{fingerprint}` still exists but is **deprecated** (same rate limit).
- Discord4KaiOS also sends `X-Context-Properties: eyJsb2NhdGlvbiI6IkxvZ2luIn0=`, which is base64 of `{"location":"Login"}`, on this call (2023 intercept).

**Failure handling:** if `/experiments` fails or has no `fingerprint`, go on without the header. UD calls the header recommended, not required (INFERENCE from "should").

## 1.2 Headers on unauthenticated auth calls

| Header | Value | Source |
|---|---|---|
| `Content-Type` | `application/json` (reqwest `.json()`) | all clients |
| `User-Agent` | browser UA, **must equal** `browser_user_agent` in the super properties | UD reference.mdx footnote |
| `X-Super-Properties` | base64(JSON client props), from `props.super_properties_header()`. "not required, ... highly recommended due to its significance in anti-abuse systems" | UD reference.mdx |
| `Origin` | `https://discord.com` | Acheron, Discordo, discord.py-self |
| `Referer` | **`https://discord.com/login`** (override Rustcord's default `/channels/@me`) | Acheron `loginPage`, Discordo |
| `X-Fingerprint` | from §1.1 | UD |
| `X-Discord-Locale` | e.g. `en-US`. UD: the locale is taken from this header, then `Accept-Language`, then user settings | UD reference.mdx |
| `X-Discord-Timezone` | IANA zone (only clients send it; UD reference does not mention it) | Acheron, Discordo |
| `X-Debug-Options` | `bugReporterEnabled`, copied from the official client. UD documents only `canary` and `trace` | clients / UD |
| `Accept`, `Accept-Language`, `Sec-Fetch-Dest: empty`, `Sec-Fetch-Mode: cors`, `Sec-Fetch-Site: same-origin` | browser-like, already in `default_headers` | Discordo, discord.py-self |
| **No `Authorization`** | Sending a valid token makes `/auth/login` succeed "irrespective of the request body" | UD |

## 1.3 `POST /auth/login` (UD authentication.mdx)

**Request**, the official 5-field body (Discord4KaiOS intercept 2023-11-16, Acheron):
```json
{"login":"user@example.com","password":"********","undelete":false,"login_source":null,"gift_code_sku_id":null}
```

| Field | Type | Notes |
|---|---|---|
| `login` | string | Email, or **E.164** phone (`+491701234567`). If the input starts with `+`, strip spaces, `-`, `(` and `)` (endcord does). |
| `password` | string | 8 to 72 characters. Validate locally first so a doomed request is never sent. |
| `undelete?` | bool | Default false. Set true **only** after the user confirms restoring an account that hit 20011 or 20013. |
| `login_source?` | ?string | One of `gift`, `guild_template`, `guild_invite`, `dm_invite`, `friend_invite`, `role_subscription`, `role_subscription_setting`. Send `null`. |
| `gift_code_sku_id?` | ?string | Send `null`. |

**200, completed:**
```json
{"user_id":"852892297661906993","token":"<TOKEN>","user_settings":{"locale":"en-US","theme":"midnight"},"required_actions":["update_password"]}
```

**200, MFA required** (no `token`):
```json
{"user_id":"852892297661906993","mfa":true,"ticket":"<TICKET>","login_instance_id":"<id>","totp":true,"sms":true,"backup":true,"webauthn":"<stringified PublicKeyCredentialRequestOptions JSON> | null"}
```

Field notes:
- Types: `user_id` snowflake string; `token?` string; `user_settings?` `{locale: string, theme: string}`; `required_actions?` array[string], the only documented value being `update_password`; `ticket?` string; `login_instance_id?` string; `mfa?` bool (default false); `totp?`, `sms?`, `backup?` bool; `webauthn?` `?string`.
- The 2023 Discord4KaiOS intercept had **no** `login_instance_id`, so treat it as optional everywhere.

**Classify the response:**
1. `token` is a non-empty string → done (§1.7).
2. Otherwise `mfa == true` and a `ticket` is present → 2FA (§1.4).
3. Otherwise → protocol error ("Unexpected response from Discord").

```rust
#[derive(Deserialize)] struct LoginResp {
    token: Option<String>, ticket: Option<String>, login_instance_id: Option<String>,
    #[serde(default)] mfa: bool, #[serde(default)] totp: bool, #[serde(default)] sms: bool,
    #[serde(default)] backup: bool, webauthn: Option<String>,
    #[serde(default)] required_actions: Vec<String>,
    user_settings: Option<serde_json::Value> }   // only locale/theme; bootstrap, optional
```

## 1.4 Two-factor (UD authentication.mdx §Verify MFA Login, §Send MFA SMS)

**Building the method list:**
- Map the response to Rustcord's `MfaMethod`: `totp` → `Totp`, `sms` → `Sms`, `backup` → `Backup`.
- Emit `LoginStep::MfaRequired { methods }`.
- If the list is empty but `webauthn` is a string, the account is passkey-only (Acheron calls this `PasskeyOnly`). WebAuthn is not practical in this native client: emit `Failed("This account only allows security-key 2FA. Use QR login instead.")`.

**Send the SMS code** (only when `sms: true`, and required before verifying with `sms`):
```
POST /auth/mfa/sms/send   {"ticket":"<TICKET>"}   → 200 {"phone":"+*******0085"}   → LoginStep::SmsSent{phone}
```
- Error `70003` means "Unable to send sms message".

**Verify:** `POST /auth/mfa/{authenticator_type}`.
- UD lists `totp | sms | backup | webauthn | password`. `password` is "used ... in certain cases if the user has not enabled any other MFA methods" and is not needed for login.
```json
{"code":"123456","ticket":"<TICKET>","login_instance_id":"<id or omit>","login_source":null,"gift_code_sku_id":null}
```
- `code`: string.
  - For webauthn it is the stringified credential JSON (not used here).
  - Send `login_instance_id` when login returned it (Acheron, arikawa and endcord do). Discord4KaiOS 2023 sent the body without it.
- Response: `200 {"token":"<TOKEN>","user_settings":{"locale":"..","theme":".."}}` → §1.7.
- Endpoint per method: `Totp` → `/auth/mfa/totp`, `Sms` → `/auth/mfa/sms`, `Backup` → `/auth/mfa/backup`. UD: `totp` also accepts "a TOTP code or backup code".

**Code input:**
- Strip whitespace and `-`.
- TOTP and SMS codes are 6 digits (standard TOTP; UD does not state the length).
- Backup codes look like `zqs8oqxk`, 8 characters lowercase alphanumeric (UD resources/user.mdx). Whether the server accepts a dashed form is UNCONFIRMED, so strip the dash.

**Ticket rules:**
- Keep the ticket and `login_instance_id` in the backend's login state.
- **Keep** them after `60008` (wrong code) and after network errors, so the user can retype (Acheron).
- Drop them on any other error. On `60006` or `60009`, restart from `/auth/login`.
- Ticket lifetime: **UNCONFIRMED**.
- A suspended account during MFA returns `403 {suspended_user_token}` (§1.5).

## 1.5 Error classifier (raw status + body; check in this order)

Parse the body as `serde_json::Value`. Read `code` as an integer from `body["code"]`.

| # | Match | Meaning | Action / user text |
|---|---|---|---|
| 1 | status 429 | Rate limited. Body: `{"message","retry_after": float s,"global": bool,"code"?}`. Headers: `Retry-After`, `X-RateLimit-Global`, `X-RateLimit-Scope: user\|global\|shared` (UD rate-limits). Unauthenticated limits are per IP; the global limit is 50 req/s. Login bucket sizes are **UNCONFIRMED**. | Use `parse_429`. **Never auto-retry.** Show "Too many attempts. Wait N s." If `parse_429` returns None (Cloudflare limit without `Retry-After`), do not retry at all: "Blocked for now, try later." |
| 2 | status 400 and `captcha_key` present | Captcha required | Detection only. Show the text from §0. |
| 3 | status 403 and `suspended_user_token` present | Account suspended. The body is `{"user_id","suspended_user_token"}`. That token is **not** an auth token: it is only valid as `token` in the body of safety-hub and appeal endpoints. | "This account is suspended." Do not store the token. |
| 4 | `code` 20011 / 20013 | "This account is scheduled for deletion" / "This account is disabled" (codes list). HTTP status UNCONFIRMED, so match on the code. | Ask "Restore account?". On yes, resend `/auth/login` with `"undelete": true`. |
| 5 | `code` 70007 | "You need to verify your phone number..." Logged in with a **phone number** from a new location; Discord sends an SMS code automatically. | §1.6 phone path. |
| 6 | `code` 50035 and some `errors.**._errors[].code == "ACCOUNT_LOGIN_VERIFICATION_EMAIL"` | New login location with email: "New login location detected, please check your e-mail." (Discord4KaiOS intercept) | §1.6 email path. |
| 7 | `code` 50035 (others) | Form errors, e.g. `errors.login._errors[0] = {"code":"INVALID_LOGIN","message":"Login or password is invalid."}` (the same entry also appears under `password`). Shape per UD reference: `{"code":50035,"errors":{"<field>":{"_errors":[{"code","message"}]}},"message":"Invalid Form Body"}`. | Show the first nested `message` (`first_nested_error`). The `_errors` codes `ACCOUNT_COMPROMISED_RESET_PASSWORD`, `INVALID_TWO_FACTOR_CODE` and `ACCOUNT_DISABLED` come only from small clients (UNCONFIRMED); the generic message display covers them. |
| 8 | `code` 60008 | "Invalid two-factor code / Security key authentication failed" | "Wrong code." Keep the ticket. |
| 9 | `code` 60006 / 60009 | "Invalid two-factor auth ticket" / "Invalid two-factor session" | "Login expired, enter your password again." Drop the ticket. |
| 10 | `code` 70003 | "Unable to send sms message" | Show it. Offer the other methods. |
| 11 | `code` 70009 | "Your Discord password reset link was sent to your email..." Exact login semantics are UNCONFIRMED. | Show the message. The user must reset the password. |
| 12 | `code` 40333 | Cloudflare block (bad request or UA). The UD code list says "Cloudflare access denied (code: 1020)"; UD errors.mdx shows the message "internal network error". | "Request blocked by Discord's firewall." This points to a header or UA bug in Rustcord. |
| 13 | status 403, `code` 10008 | Anti-abuse block despite the "Unknown Message" text (UD errors.mdx) | "Discord blocked this login attempt." |
| 14 | anything else | | Show `message`, or "HTTP {status}". |

- The HTTP status of the MFA errors 60006 and 60008 is UNCONFIRMED. The Spacebar and yepcord reimplementations use 400. Match on `code`.
- Cloudflare bans an IP for 24 h after 10,000 401/403/429 responses in 10 minutes (UD rate-limits). This is another reason for single-shot requests.

## 1.6 New-location verification (UD authentication.mdx, topics/phone-verification.mdx)

UD's trigger: "When first logging into an account (without MFA enabled) from a new location, Discord may reject the login request". After authorizing the IP, retry `/auth/login`. UD says a new captcha "should not be required".

**Email path:**
1. The mail contains a link `https://discord.com/authorize-ip#token=<T>`. The token is in the URL **fragment**.
2. **Robust option:** let the user paste the link. Take `<T>` after `#token=` and send `POST /auth/authorize-ip {"token":"<T>"}` → `204`. Then resend `/auth/login`.
3. **Minimal option:** tell the user "Open the link in the e-mail, then log in again". The web client authorizes the IP itself. Whether that works when the browser and Rustcord are on the same network is INFERENCE (UNCONFIRMED).
4. endcord posts this token to `/phone-verifications/verify`. That is wrong per UD.

**Phone path (`70007`):** the SMS code is sent automatically.
1. `POST /phone-verifications/verify {"phone":"<E.164, same as login>","code":"<sms>"}` → `{"token":"..."}`
2. `POST /auth/authorize-ip {"token":"..."}` → `204`
3. Resend `/auth/login`.
4. To resend the code: `POST /phone-verifications/resend {"phone":"..."}` → `204`.

**Rustcord gap:** `LoginStep` has no variant for "confirm undelete", "paste e-mail link" or "enter phone verification code". The minimum is `Failed(<instruction text>)`. The proper fix is new `LoginStep` variants plus matching `Command`s, and that is a backend.rs contract change.

## 1.7 After the token

- Use the token as `Authorization: <token>`, with **no** `Bot` or `Bearer` prefix (UD reference.mdx examples).
- Steps: `Token::parse`, then the `Command::Login` path (`GET /users/@me` → 401 means a bad token; keyring if `remember`; gateway). Drop `X-Fingerprint` and the throwaway HTTP client.
- `required_actions: ["update_password"]`: show a notice. Implementing the password change is optional.
- Account-level `required_action` arrives in READY. Changes arrive through `USER_REQUIRED_ACTION_UPDATE`. Values include `AGREEMENTS`, `REQUIRE_VERIFIED_EMAIL`, `REQUIRE_VERIFIED_PHONE` and `REQUIRE_REVERIFIED_*`. Show them as a banner (UD resources/user.mdx; not re-fetched in this edit).
- `user_settings.locale` and `theme` are optional bootstrap values. Gateway close `4004` means the token is invalid (already handled).
- The official client's exact post-login call order is **UNCONFIRMED**.

## Implementation checklist (Part 1)

- [ ] `api/mod.rs`: make `default_headers` `pub(crate)`. Add `api/login.rs` with a throwaway unauthenticated client (§0): Referer `/login`, no `Authorization`, **no `RateLimiter`**, redirects off.
- [ ] Fetch the fingerprint once per login screen (`GET /experiments`) and send `X-Fingerprint` on every auth call. Continue without it if the call fails.
- [ ] `LoginPassword`:
  - validate the password length (8 to 72)
  - normalize a phone login to E.164
  - send the 5-field body
  - classify the response: token → Login path; `mfa+ticket` → `MfaRequired{methods}`; passkey-only → `Failed`
- [ ] Keep `{ticket, login_instance_id, login, password: Secret}` in backend state. Clear it on success, on a non-retryable error, and on logout.
- [ ] `SendMfaSms` → `/auth/mfa/sms/send` → `SmsSent{phone}`.
- [ ] `SubmitMfa{method, code}`: strip whitespace and `-`, then `/auth/mfa/{totp|sms|backup}` with `ticket` and `login_instance_id`. Keep the ticket on 60008.
- [ ] Classifier §1.5, in order: 429, then captcha, then suspended, then the codes, then 50035 nested messages. Unit-test it with the JSON examples above.
- [ ] Undelete confirm (20011/20013) and new location (email link / 70007 SMS): `Failed(text)` at minimum. Proper `LoginStep` variants are optional.
- [ ] Never auto-retry `/auth/login` or `/auth/mfa/*`. Never log bodies, the password, codes, the ticket or the token.

---

# Part 2: QR code login (Remote Auth v2)

## 2.1 Connection (UD remote-authentication/desktop.mdx)

**Endpoint and headers:**
- URL: `wss://remote-auth-gateway.discord.gg/?v=2`. `v` is required.
  - **v2 is current; v1 is discontinued.** An invalid version closes with 4000.
- **`Origin` is required:** `https://discord.com` (also accepted: `https://ptb.discord.com`, `https://canary.discord.com`). Any other origin is rejected.
  - Reuse `gateway/conn.rs::upgrade_request(url, &props)` (make it `pub(crate)`). It already sets Origin, the UA, Accept-Language and Cache-Control.
  - Whether a UA is required is UNCONFIRMED; Abaddon and Discordo send a browser UA.

**Frames:**
- Flat JSON objects with a string `op` (lower_snake_case) and the fields next to it. There is **no** `d` wrapper.
- Use uncompressed text frames, as every implementation does. UD does not state the framing.
- Ignore unknown ops (Discordo does).

**Heartbeat:**
- Send `{"op":"heartbeat"}` at least every `heartbeat_interval` ms. The server answers `{"op":"heartbeat_ack"}`.
- The first beat *may* be jittered in [0, interval]. That is optional; a plain `interval_at(now + interval, interval)` is fine.
- If no ack arrives before the next beat is due, the connection is a zombie: close it and start a new session.

**Lifetime:** `timeout_ms` is the lifetime of the **whole session**. UD's example is 142637 ms, about 2.4 min. After it the server closes with **4003**.

## 2.2 Op sequence (v2)

| # | Dir | Payload |
|---|---|---|
| 1 | S→C | `{"op":"hello","timeout_ms":142637,"heartbeat_interval":41250}` (UD example values) |
| 2 | C→S | `{"op":"init","encoded_public_key":"<base64 STANDARD of SPKI DER of a 2048-bit RSA key, e=65537>"}` |
| 3 | S→C | `{"op":"nonce_proof","encrypted_nonce":"<base64 STANDARD>"}` |
| 4 | C→S | `{"op":"nonce_proof","nonce":"<base64url, NO padding, of the RAW decrypted nonce bytes>"}` |
| 5 | S→C | `{"op":"pending_remote_init","fingerprint":"<base64url nopad of SHA-256(SPKI DER)>"}` → show the QR |
| 6 | S→C | `{"op":"pending_ticket","encrypted_user_payload":"<base64>"}` (QR scanned) |
| 7a | S→C | `{"op":"pending_login","ticket":"<opaque>"}` (approved on the phone), then the server closes 1000 |
| 7b | S→C | `{"op":"cancel"}` (declined on the phone), then close 1000 |
| – | ↔ | `{"op":"heartbeat"}` / `{"op":"heartbeat_ack"}` |

**Step 4 is the v2 change.** The client sends the **raw nonce**, not `SHA-256(nonce)`.
- Confirmed by UD desktop.mdx, Abaddon (`remoteauthclient.cpp`: base64 → `/`→`_`, `+`→`-`, strip `=`) and Discordo (`base64.RawURLEncoding.EncodeToString(decryptedNonce)`).
- The old `{"op":"nonce_proof","proof": b64url(sha256(nonce))}` and the ops `pending_finish` / `finish {encrypted_token}` are **v1 only** (teaishealthy/discord_remote_auth uses `?v=1`). Do not copy v1 guides.

**Step 5:**
- Compare `fingerprint` with your own value. On a mismatch, close and start over (UD).
- Test vector: UD's example `encoded_public_key` (`MIIBIjANBgkq…TwIDAQAB`) hashes to `UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k` (checked in a unit test).
- The QR content is **`https://discord.com/ra/<fingerprint>`**. Render it with the existing `qrcodegen` (Discordo uses ECC Low) and a quiet zone, then emit `LoginStep::QrCode{url}`.

**Step 6, the user payload:**
- Decrypt it to UTF-8 `id:discriminator:avatar:username`. UD example: `852892297661906993:0:05145cc5646fbcba277b6d5ea2030610:dolfies`.
  - Split with `splitn(4, ':')`.
  - avatar `"0"` means none (UD).
  - discriminator `"0"` means a new-style username (UD's example; Discordo shows `#disc` only when it is not `"0"`).
- Avatar URL: `https://cdn.discordapp.com/avatars/{id}/{hash}.png?size=64`.
- Emit `LoginStep::QrScanned{username, avatar_url}`.
- In v2 the token is **never** sent over the WebSocket; it only comes from the HTTP exchange in §2.4.

## 2.3 Crypto (rsa 0.10.0 API checked against the crate source in `~/.cargo/registry`)

**Rules:**
- **Key:** RSA **2048**-bit, e = 65537, a **new key per session/QR**, never persisted.
  - Use `PrivateKey::new(&mut rng, 2048)`.
  - Do **not** use `rsa::Generate::generate()`: it makes 3072-bit keys (`DEFAULT_BITS = 3072`), and UD specifies 2048.
- **Every decryption** (nonce, user payload, token): base64 **STANDARD** decode, then RSA-OAEP with **SHA-256 for the OAEP hash and MGF1** and an **empty label**.
  - `key.oaep_decrypt::<Sha256>(ct)` does exactly that (oaep.rs: "Uses `D` for both OAEP and MGF1, with an empty label"). It also blinds with the OS RNG under rsa's default `getrandom` feature.

**rsa 0.10 API notes:**
- `RsaPrivateKey` and `RsaPublicKey` are deprecated aliases; use `rsa::PrivateKey` and `rsa::PublicKey`.
- `Oaep::new::<Sha256>()` does not compile; the type is `Oaep::<Sha256>::new()`. You do not need it.
- rsa re-exports `rand_core`, `pkcs8` and `sha2`, but **no OS RNG**.
- **Cargo.toml:** add `getrandom = { version = "0.4", features = ["sys_rng"] }`. getrandom 0.4.3 is already in Cargo.lock, so no new crate is downloaded. Pass `&mut getrandom::SysRng`.
- Run key generation in `tokio::task::spawn_blocking`.

```rust
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rsa::PrivateKey;
use rsa::pkcs8::EncodePublicKey as _;
use sha2::{Digest as _, Sha256};
pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;

pub struct RaKeys { key: PrivateKey, pub encoded_public_key: String, pub fingerprint: String }
impl RaKeys {
    pub fn generate() -> Result<Self, BoxErr> {                       // spawn_blocking
        let key = PrivateKey::new(&mut getrandom::SysRng, 2048)?;
        let der = key.to_public_key().to_public_key_der()?;
        Ok(Self { encoded_public_key: STANDARD.encode(der.as_bytes()),
                  fingerprint: URL_SAFE_NO_PAD.encode(Sha256::digest(der.as_bytes())), key })
    }
    pub fn decrypt_b64(&self, b64: &str) -> Result<Vec<u8>, BoxErr> {
        Ok(self.key.oaep_decrypt::<Sha256>(&STANDARD.decode(b64)?)?)     // OAEP+MGF1 SHA-256, empty label
    }
    pub fn nonce_proof(&self, encrypted_nonce: &str) -> Result<serde_json::Value, BoxErr> {
        let nonce = self.decrypt_b64(encrypted_nonce)?;                  // v2: raw nonce, NOT sha256
        Ok(serde_json::json!({"op":"nonce_proof","nonce": URL_SAFE_NO_PAD.encode(nonce)}))
    }
}
pub fn parse_user_payload(s: &str) -> Option<(&str, &str, Option<&str>, &str)> {
    let mut it = s.splitn(4, ':');
    let (id, disc, av, name) = (it.next()?, it.next()?, it.next()?, it.next()?);
    Some((id, disc, (av != "0").then_some(av), name))
}
```

This sketch compiles, and 3 unit tests pass offline with exactly Rustcord's crate versions (rsa 0.10.0, sha2 0.11.1, base64 0.23.1, getrandom 0.4.3). The tests cover the doc fingerprint vector, an encrypt → `nonce_proof` round trip, and payload parsing. **It has not been run against Discord.** Port the three tests along with the code.

## 2.4 Ticket → token exchange (UD desktop.mdx; Abaddon `discord.cpp`)

```
POST https://discord.com/api/v9/users/@me/remote-auth/login      (unauthenticated: NO Authorization)
Content-Type: application/json
{"ticket":"<ticket from pending_login>"}
→ 200 {"encrypted_token":"<base64>"}
token = String::from_utf8(oaep_decrypt(b64decode(encrypted_token)))  → Token::parse → Command::Login path
```

- Do it **right after** `pending_login`, even though the socket closes with 1000 at the same moment. Ticket lifetime is **UNCONFIRMED**.
- Headers:
  - Abaddon sends only `Content-Type` and `User-Agent`.
  - Discordo sends its full browser set plus `Referer: https://discord.com/login` and `X-Fingerprint: <the RA fingerprint>`. That RA fingerprint is a SHA-256 value, not the `<snowflake>.<hash>` format from `/experiments`, so whether the official client does this is **UNCONFIRMED**.
  - **Recommendation:** use the same throwaway client as in §0 (`default_headers`, Referer `/login`, no auth, no limiter). Send `X-Fingerprint` only with an `/experiments` fingerprint if one was fetched (INFERENCE from UD's general rule), never with the RA fingerprint.
- Errors:
  - captcha → §0 detection, `Failed("...complete the login in the official app.")`
  - 429 → §1.5 row 1
  - other non-captcha errors (invalid or expired ticket) have an **UNCONFIRMED** shape; expect the generic `{"code","message"}`
  - On any failure, offer a new QR. Do not retry the same ticket.

## 2.5 Close codes, expiry, cancel

| Close | Meaning (UD) | Rustcord action |
|---|---|---|
| 1000 | Normal: finished or cancelled. UD warns it **may also arrive for protocol errors**. | Expected after 7a or 7b. A 1000 without 7a or 7b counts as an error: `QrExpired` (offer a new code). |
| 4000 | Invalid version | A bug in Rustcord: `Failed`. |
| 4001 | Decode error: an invalid payload, or **any handshake step failed** | Most likely a wrong `nonce_proof` (for example the v1 hash) or a bad key: `Failed`. |
| 4002 | Handshake failure | Start a new session (new key, new QR). |
| 4003 | Session timed out (`timeout_ms` elapsed) | `QrExpired`. The user clicks for a new code. |
| stream ends without a close frame / read error | network | `Failed("Network problem...")`. |

**Expiry:**
- `pending_ticket` does **not** extend the session; an unconfirmed scan still ends with 4003.
- Also arm a local timer of `timeout_ms` + a few seconds as a fallback (Abaddon restarts on its local `timeout_ms` timer).
- A new code always means a new connection, keypair, fingerprint and QR.

**Phone declines (`{"op":"cancel"}`):** emit `QrExpired` ("cancelled on phone, get a new code"). Abaddon auto-restarts here; Discordo stops. Rustcord's `QrExpired` covers both (the UI offers a new code). Do not auto-loop.

**`CancelQrLogin`, or a new `StartQrLogin` (which "replaces a running one"):** abort the task (`JoinHandle::abort`). The socket and key drop with it.

**Mobile side, for context only** (UD remote-authentication/mobile.mdx):
- `POST /users/@me/remote-auth {fingerprint}` → `{handshake_token}`. This makes the desktop receive `pending_ticket` (INFERENCE from the op order).
- `POST /users/@me/remote-auth/finish {handshake_token, temporary_token?: bool}` is MFA-gated and returns 204. It sends the token to the desktop (→ `pending_login`). UD: expiring tokens are "not yet supported".
- `POST /users/@me/remote-auth/cancel {handshake_token}` → 204 (→ `cancel`).

## 2.6 Flow loop sketch (tokio-tungstenite 0.30, futures-util)

1. `req = upgrade_request("wss://remote-auth-gateway.discord.gg/?v=2", &props)?`, then `connect_async(req)`, then `.split()`.
2. Generate `RaKeys` in `spawn_blocking`. This can run in parallel with the connect.
3. `tokio::select!` over `rx.next()`, the heartbeat `interval_at` (armed on `hello`) and the local expiry `sleep`. Handle each incoming op:
   - `hello`: send `init`, arm the heartbeat and the expiry.
   - `nonce_proof`: send `keys.nonce_proof(encrypted_nonce)`.
   - `pending_remote_init`: if `fp != keys.fingerprint`, error. Otherwise emit `QrCode{url}`.
   - `pending_ticket`: decrypt, parse, emit `QrScanned`.
   - `pending_login`: break with the ticket, then run §2.4.
   - `cancel`: emit `QrExpired`. `heartbeat_ack`: set `acked = true`. Anything else: ignore.
   - Heartbeat tick: if `!acked`, treat it as a zombie (`Failed`). Otherwise send the heartbeat and set `acked = false`.
4. Send with `Message::text(json.to_string())`. Receive `Message::Text(t)` and read it with `t.as_str()`. For `Message::Close(Some(f))`, map `u16::from(f.code)` through the table in §2.5.

## Implementation checklist (Part 2)

- [ ] Cargo.toml: `getrandom = { version = "0.4", features = ["sys_rng"] }`. No other new crates (`rsa`, `sha2`, `base64`, `qrcodegen`, `tokio-tungstenite` and `reqwest` are already there).
- [ ] `gateway/conn.rs`: make `upgrade_request` `pub(crate)`, which gives the Origin header for free.
- [ ] `RaKeys` plus `parse_user_payload` plus their 3 unit tests. 2048-bit key, generated in `spawn_blocking`, never persisted.
- [ ] Use the v2 `nonce_proof` with the **raw nonce**, base64url without padding. No `proof` field.
- [ ] Check the fingerprint on `pending_remote_init`, then emit `QrCode{url: "https://discord.com/ra/<fp>"}`.
- [ ] `pending_ticket` → `QrScanned{username, avatar_url}`. Do not log the payload.
- [ ] `pending_login` → unauthenticated POST `/users/@me/remote-auth/login` → decrypt `encrypted_token` → `Token::parse` → `Command::Login` path with `remember`.
- [ ] Heartbeat with zombie detection. Local expiry timer of `timeout_ms` plus a margin.
- [ ] Close-code mapping (§2.5). `cancel` / 4003 / 1000-without-ticket → `QrExpired`. No automatic reconnect loop.
- [ ] Wire `StartQrLogin` (abort and replace any running task) and `CancelQrLogin` (abort) into `backend::run`.
- [ ] Captcha on the exchange → `Failed("...complete the login in the official app.")`. Never log the ticket or the token.

---

## Editor's corrections to the draft notes

1. UD's authenticator types also include **`password`** (not needed for login). `totp` also accepts backup codes.
2. `captcha_service` can also be **`recaptcha`**. UD names login, invites and friend requests explicitly, plus "any endpoint". The MFA, SMS and authorize-ip endpoints are INFERENCE.
3. `X-Discord-Timezone` is not in UD reference; it comes from client code only. `X-Super-Properties` is "not required ... highly recommended" (confirmed).
4. Error 40333: the code list says "Cloudflare access denied (code: 1020)", while UD errors.mdx gives the example message "internal network error". Match on the code.
5. Acheron keeps the MFA ticket on 60008 **and** on network errors.
6. Rustcord-specific gaps the drafts missed:
   - `ApiClient` cannot make unauthenticated calls.
   - `RateLimiter` would auto-retry a 429 on login.
   - `error_from` drops `code`, `retry_after` and `suspended_user_token`.
   - `default_headers` and `upgrade_request` are private.
   - `LoginStep` has no variants for undelete or new-location verification.
7. Discordo's `X-Fingerprint` on the QR exchange is the RA SHA-256 fingerprint, not an `/experiments` fingerprint. Do not copy it.
8. Remote-auth draft: everything re-verified (raw nonce in v2, ops, close codes, rsa 0.10 API, the getrandom requirement). The 3 sketch tests were re-run and pass.

## Sources (fetched 2026-11-11 unless noted)

- UD raw MDX (`https://raw.githubusercontent.com/discord-userdoccers/discord-userdoccers/master/pages/`):
  - `authentication.mdx`, `topics/experiments.mdx`, `topics/captcha-handling.mdx`, `topics/errors.mdx`, `topics/rate-limits.mdx`, `topics/phone-verification.mdx`, `reference.mdx`
  - `remote-authentication/desktop.mdx` (the v2 raw-nonce change is UD commit c97d05b, 2026-01-22, per the draft note), `remote-authentication/mobile.mdx`
  - `resources/user.mdx` (backup code format and required actions) and the gateway close codes come from the draft and were not re-fetched.
- UD JSON error-code list: https://docs.discord.food/api/codes
- Abaddon: https://github.com/uowuo/abaddon/blob/master/src/remoteauth/remoteauthclient.cpp, https://github.com/uowuo/abaddon/blob/master/src/discord/discord.cpp (`RemoteAuthLogin`)
- Acheron: https://github.com/ouwou/acheron/blob/HEAD/src/Discord/LoginClient.cpp
- Discordo: https://github.com/ayn2op/discordo/blob/main/internal/ui/login/qr/msg.go, `internal/ui/login/password/msg.go`, `internal/http/headers.go`
- Discord4KaiOS (2023-11-16 intercepts): https://github.com/Discord4KaiOS/backend/blob/HEAD/src/DiscordSetup.ts
- From the draft notes, not re-fetched:
  - endcord: https://github.com/sparklost/endcord/blob/HEAD/endcord/auth.py
  - arikawa: https://github.com/diamondburned/arikawa/blob/HEAD/api/login.go
  - discord.py-self: https://github.com/dolfies/discord.py-self/blob/HEAD/discord/http.py
  - v1 reference: https://github.com/teaishealthy/discord_remote_auth
  - Spacebar and yepcord reimplementations
- Local crate source: rsa-0.10.0 (`lib.rs`, `oaep.rs`, `private_key.rs`), getrandom-0.4.3; Rustcord `src/api/mod.rs`, `src/api/ratelimit.rs`, `src/backend.rs`, `src/gateway/conn.rs`, `src/auth.rs`.

**Still UNCONFIRMED:**
- the MFA ticket lifetime and the remote-auth ticket lifetime
- login rate-limit bucket sizes
- the real HTTP status for 20011, 20013, 60006 and 60008
- the shape of non-captcha errors on the remote-auth exchange
- whether `X-Fingerprint` matters on the QR exchange
- the official client's post-login call order
