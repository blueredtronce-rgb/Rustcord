//! QR-code login (Remote Auth v2, docs/AUTH_NOTES.md part 2): a WebSocket to
//! remote-auth-gateway.discord.gg, a fresh RSA-2048 key per code, and the ticket -> token
//! exchange over HTTP. One flow = one code; it never reconnects on its own.
//!
//! OWNERSHIP: email/QR login implementer.

use crate::api::auth::AuthClient;
use crate::api::props::ClientProps;
use crate::auth::{Secret, Token};
use crate::backend::{BackendEvent, EventSink, LoginStep};
use crate::model::{Id, User};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rsa::PrivateKey;
use rsa::pkcs8::EncodePublicKey as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Added to the server's `timeout_ms` before the local fallback expiry fires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5);

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// How a flow ended without a token.
#[derive(Debug, PartialEq)]
enum End {
    /// Timed out / cancelled on the phone / closed by the server: the UI offers a new code.
    Expired,
    Failed(String),
}

fn network() -> End {
    End::Failed("Network problem during QR login. Check your internet connection and get a new code.".into())
}

fn protocol(e: impl std::fmt::Display) -> End {
    End::Failed(format!("QR login failed ({e}). Get a new code, or use another login method."))
}

/// Per-code RSA key. No Debug: it holds the private key.
struct RaKeys {
    key: PrivateKey,
    encoded_public_key: String,
    fingerprint: String,
}

impl RaKeys {
    /// Slow (~100 ms+): call from `spawn_blocking`.
    fn generate() -> Result<Self, BoxErr> {
        // 2048 bits as the protocol requires (rsa's `Generate` default is 3072).
        let key = PrivateKey::new(&mut getrandom::SysRng, 2048)?;
        let der = key.to_public_key().to_public_key_der()?;
        Ok(Self {
            encoded_public_key: STANDARD.encode(der.as_bytes()),
            fingerprint: URL_SAFE_NO_PAD.encode(Sha256::digest(der.as_bytes())),
            key,
        })
    }

    /// base64 -> RSA-OAEP (SHA-256 for OAEP and MGF1, empty label).
    fn decrypt_b64(&self, b64: &str) -> Result<Vec<u8>, BoxErr> {
        Ok(self.key.oaep_decrypt::<Sha256>(&STANDARD.decode(b64)?)?)
    }

    /// v2: the raw decrypted nonce, base64url without padding (v1 sent a SHA-256 "proof").
    fn nonce_proof(&self, encrypted_nonce: &str) -> Result<Value, BoxErr> {
        let nonce = self.decrypt_b64(encrypted_nonce)?;
        Ok(json!({ "op": "nonce_proof", "nonce": URL_SAFE_NO_PAD.encode(nonce) }))
    }
}

/// `id:discriminator:avatar:username` -> (username shown, avatar URL).
fn scanned_user(payload: &str) -> Option<(String, Option<String>)> {
    let mut it = payload.splitn(4, ':');
    let (id, disc, avatar, name) = (it.next()?, it.next()?, it.next()?, it.next()?);
    let username = if disc == "0" { name.to_owned() } else { format!("{name}#{disc}") };
    let avatar_url = id.parse::<Id>().ok().map(|id| {
        let user = User {
            id,
            avatar: (avatar != "0").then(|| avatar.to_owned()),
            discriminator: Some(disc.to_owned()),
            ..User::default()
        };
        crate::api::cdn::avatar_url(&user, 64, false)
    });
    Some((username, avatar_url))
}

/// Close code -> outcome (AUTH_NOTES §2.5). Only called before `pending_login`.
fn close_end(code: Option<u16>) -> End {
    match code {
        // 1000 without a ticket, 4002 handshake failure, 4003 session timeout: new code.
        None | Some(1000 | 4002 | 4003) => End::Expired,
        Some(c) => protocol(format_args!("server closed with code {c}")),
    }
}

/// Runs one QR login. Emits `LoginStep`s; returns the token after the phone approved.
/// Cancel by aborting the task (socket and key drop with it).
pub async fn run(props: Arc<ClientProps>, auth: Arc<AuthClient>, sink: EventSink) -> Option<Token> {
    match flow(&props, &auth, &sink).await {
        Ok(token) => Some(token),
        Err(end) => {
            sink.send(BackendEvent::LoginStep(match end {
                End::Expired => LoginStep::QrExpired,
                End::Failed(m) => LoginStep::Failed(m),
            }));
            None
        }
    }
}

async fn flow(props: &ClientProps, auth: &AuthClient, sink: &EventSink) -> Result<Token, End> {
    let keys = tokio::task::spawn_blocking(RaKeys::generate);
    // Same browser-like headers as the gateway (Origin is mandatory here).
    let req = super::conn::upgrade_request(URL, props).map_err(protocol)?;
    let (ws, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| network())?
        .map_err(|_| network())?;
    let keys = keys.await.map_err(protocol)?.map_err(protocol)?;
    let (mut tx, mut rx) = ws.split();

    // Heartbeat and expiry only start after hello: without this a silent server hangs the QR tab.
    let hello = timeout(CONNECT_TIMEOUT, recv(&mut rx)).await.map_err(|_| network())??;
    if op(&hello) != "hello" {
        return Err(protocol("no hello"));
    }
    let ms = |k: &str| hello.get(k).and_then(Value::as_u64).filter(|&v| v > 0);
    let (Some(interval), Some(lifetime)) = (ms("heartbeat_interval"), ms("timeout_ms")) else {
        return Err(protocol("bad hello"));
    };
    let interval = Duration::from_millis(interval);
    let mut heartbeat = tokio::time::interval_at(Instant::now() + interval, interval);
    let expires = Instant::now() + Duration::from_millis(lifetime) + EXPIRY_MARGIN;
    let mut acked = true;
    send(&mut tx, json!({ "op": "init", "encoded_public_key": keys.encoded_public_key })).await?;

    let ticket = loop {
        let msg = tokio::select! {
            m = recv(&mut rx) => m?,
            _ = heartbeat.tick() => {
                if !acked {
                    return Err(network()); // zombie connection
                }
                acked = false;
                send(&mut tx, json!({ "op": "heartbeat" })).await?;
                continue;
            }
            _ = sleep_until(expires) => return Err(End::Expired),
        };
        let field = |k: &str| msg.get(k).and_then(Value::as_str).ok_or_else(|| protocol(format_args!("missing {k}")));
        match op(&msg) {
            "nonce_proof" => send(&mut tx, keys.nonce_proof(field("encrypted_nonce")?).map_err(protocol)?).await?,
            "pending_remote_init" => {
                let fp = field("fingerprint")?;
                if fp != keys.fingerprint {
                    return Err(protocol("fingerprint mismatch"));
                }
                sink.send(BackendEvent::LoginStep(LoginStep::QrCode { url: format!("https://discord.com/ra/{fp}") }));
            }
            "pending_ticket" => {
                let payload = keys.decrypt_b64(field("encrypted_user_payload")?).map_err(protocol)?;
                let (username, avatar_url) = std::str::from_utf8(&payload)
                    .ok()
                    .and_then(scanned_user)
                    .ok_or_else(|| protocol("unreadable user"))?;
                sink.send(BackendEvent::LoginStep(LoginStep::QrScanned { username, avatar_url }));
            }
            "pending_login" => break Secret::new(field("ticket")?.to_owned()),
            "cancel" => return Err(End::Expired),
            "heartbeat_ack" => acked = true,
            _ => {}
        }
    };
    drop((tx, rx)); // the server closes with 1000 now anyway

    // Exchanged right away: the ticket's lifetime is unknown. Never retried.
    let encrypted = auth.remote_auth_login(&ticket).await.map_err(|e| End::Failed(e.message))?;
    let token = keys.decrypt_b64(&encrypted).map_err(protocol)?;
    std::str::from_utf8(&token).ok().and_then(Token::parse).ok_or_else(|| protocol("unreadable token"))
}

fn op(v: &Value) -> &str {
    v.get("op").and_then(Value::as_str).unwrap_or_default()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(tx: &mut futures_util::stream::SplitSink<Ws, WsMessage>, v: Value) -> Result<(), End> {
    tx.send(WsMessage::text(v.to_string())).await.map_err(|_| network())
}

/// Next JSON text frame. Cancel-safe: only non-text frames are consumed before an await.
async fn recv(rx: &mut futures_util::stream::SplitStream<Ws>) -> Result<Value, End> {
    loop {
        match rx.next().await {
            Some(Ok(WsMessage::Text(t))) => return serde_json::from_str(t.as_str()).map_err(|_| protocol("bad frame")),
            Some(Ok(WsMessage::Close(frame))) => return Err(close_end(frame.map(|f| u16::from(f.code)))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(network()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_proof_round_trip() {
        let keys = RaKeys::generate().unwrap();
        // The fingerprint is SHA-256 of the SPKI DER we announce.
        let der = STANDARD.decode(&keys.encoded_public_key).unwrap();
        assert_eq!(keys.fingerprint, URL_SAFE_NO_PAD.encode(Sha256::digest(&der)));

        let nonce = b"\x00\x01server-nonce\xff";
        let ct = keys.key.to_public_key().oaep_encrypt::<Sha256>(nonce).unwrap();
        let proof = keys.nonce_proof(&STANDARD.encode(ct)).unwrap();
        assert_eq!(proof, json!({ "op": "nonce_proof", "nonce": URL_SAFE_NO_PAD.encode(nonce) }));
        assert!(keys.decrypt_b64("not base64!").is_err());
    }

    #[test]
    fn parses_scanned_user() {
        let (name, avatar) = scanned_user("852892297661906993:0:05145cc5646fbcba277b6d5ea2030610:dolfies").unwrap();
        assert_eq!(name, "dolfies");
        assert_eq!(
            avatar.unwrap(),
            "https://cdn.discordapp.com/avatars/852892297661906993/05145cc5646fbcba277b6d5ea2030610.webp?size=64"
        );
        let (name, avatar) = scanned_user("80351110224678912:1337:0:old:name").unwrap();
        assert_eq!(name, "old:name#1337");
        assert_eq!(avatar.unwrap(), "https://cdn.discordapp.com/embed/avatars/2.png");
        assert!(scanned_user("1:0:0").is_none());
    }

    #[test]
    fn close_codes() {
        assert_eq!(close_end(Some(4003)), End::Expired);
        assert_eq!(close_end(Some(1000)), End::Expired);
        assert!(matches!(close_end(Some(4001)), End::Failed(_)));
    }
}
