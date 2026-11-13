//! DAVE end-to-end encryption on top of `davey` 0.2 (`davey::DaveSession`): session
//! lifecycle, voice opcodes 21–31 (binary 25–30), epochs and transitions, frame encryption.
//!
//! OWNERSHIP: DAVE engineer. Keep the pub items' signatures (transport.rs uses them). Spec:
//! VOICE_NOTES §3.2–3.3 (opcode table with the exact davey calls).
//!
//! Threading: the transport owns one `Arc<std::sync::Mutex<Dave>>` per connection; the
//! WebSocket task calls the `on_*` methods, the UDP tasks `encrypt_opus`/`decrypt`. Every
//! method is short and non-blocking (no I/O, no awaiting; creating a key package takes ~1 ms).
//! Secrets: MLS keys and key packages are never logged; never install a `tracing` subscriber
//! that enables davey's debug/trace output (it prints key material).
//!
//! Protocol version 0 (DAVE off, e.g. Stage channels): frames pass through, nothing is sent.
//! davey keeps the per-user decryptors and my encryptor; this file only routes the opcodes,
//! tracks which protocol version media uses right now and recovers from bad commits/welcomes
//! (op 31 + a fresh key package).

use crate::model::Id;
use davey::{Codec, DaveSession, MediaType, OPUS_SILENCE_PACKET, ProposalsOperationType};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::num::NonZeroU16;

/// Voice Identify `max_dave_protocol_version`.
pub const MAX_PROTOCOL_VERSION: u16 = 1;

/// After an upgrade from protocol version 0, unencrypted frames that were already in flight
/// are still accepted for this long (DAVE "transition expiry").
const UPGRADE_PASSTHROUGH_SECS: u32 = 10;

/// A message for the voice WebSocket.
#[derive(Debug, PartialEq)]
pub enum Reply {
    /// Text frame with the complete payload, e.g. `{"op":23,"d":{"transition_id":5}}`.
    Json(serde_json::Value),
    /// Binary frame `[op][payload]` (op 26 key package, op 28 commit + welcome).
    Binary(Vec<u8>),
}

/// DAVE state of one voice connection.
pub struct Dave {
    user_id: Id,
    group_id: Id,
    /// What media uses now: Session Description, then each executed transition. 0 = E2EE off.
    protocol_version: u16,
    /// None until the first protocol version > 0.
    session: Option<DaveSession>,
    /// Last op 25 payload; applied when the session is created after it arrived.
    external_sender: Option<Vec<u8>>,
    /// Users in the call (op 11/13) plus me; proposals and welcomes must not add anyone else.
    recognized: Vec<u64>,
    /// Prepared, not yet executed transitions: id -> protocol version.
    pending: HashMap<u16, u16>,
    /// Sent op 31 + a fresh key package. Commits/welcomes that fail before the next good one are
    /// stale (made for the old key package) and are ignored, or each would re-key again forever.
    recovering: bool,
}

impl Dave {
    /// `user_id` = me (credential identity), `group_id` = the voice channel id (MLS group).
    pub fn new(user_id: Id, group_id: Id) -> Dave {
        Dave {
            user_id,
            group_id,
            protocol_version: 0,
            session: None,
            external_sender: None,
            recognized: vec![user_id.0],
            pending: HashMap::new(),
            recovering: false,
        }
    }

    /// Op 4 Session Description's `dave_protocol_version`. >0: create (or re-init) the
    /// session, apply a buffered op 25, return op 26 (key package).
    pub fn on_session_description(&mut self, protocol_version: u16) -> Vec<Reply> {
        self.protocol_version = protocol_version;
        if protocol_version == 0 {
            return Vec::new();
        }
        self.reinit(protocol_version)
    }

    /// Op 11 Clients Connect: add to the recognized user ids (I am always included).
    pub fn on_clients_connect(&mut self, user_ids: &[Id]) {
        for id in user_ids {
            if !self.recognized.contains(&id.0) {
                self.recognized.push(id.0);
            }
        }
    }

    /// Op 13 Client Disconnect: drop from the recognized user ids.
    pub fn on_client_disconnect(&mut self, user_id: Id) {
        if user_id != self.user_id {
            self.recognized.retain(|&u| u != user_id.0);
        }
    }

    /// JSON DAVE ops 21 (prepare transition), 22 (execute transition), 24 (prepare epoch);
    /// `d` = the payload's `d` object. Returns what to send (op 23 transition ready, op 26).
    pub fn on_json(&mut self, op: u8, d: &serde_json::Value) -> Vec<Reply> {
        let num = |key: &str| d.get(key).and_then(Value::as_u64);
        let u16_of = |key: &str| num(key).and_then(|n| u16::try_from(n).ok());
        match (op, u16_of("transition_id"), u16_of("protocol_version")) {
            (21, Some(tid), Some(pv)) => {
                self.prepare(tid, pv);
                transition_ready(tid)
            }
            (22, Some(tid), _) => {
                self.execute(tid);
                Vec::new()
            }
            // Epoch 1 = a new MLS group is formed: start over with a fresh key package.
            (24, _, Some(pv)) if num("epoch") == Some(1) => self.reinit(pv),
            _ => Vec::new(),
        }
    }

    /// Binary DAVE ops 25 (external sender), 27 (proposals), 29 (announce commit), 30 (welcome).
    /// `payload` = the frame after its 2-byte sequence number and op byte. Returns op 28, op 23,
    /// or on a bad commit/welcome op 31 + a fresh op 26 (after re-init).
    pub fn on_binary(&mut self, op: u8, payload: &[u8]) -> Vec<Reply> {
        match op {
            25 => {
                // Ordering against Session Description is unconfirmed: keep it for a later session.
                if let Some(s) = &mut self.session
                    && let Err(e) = s.set_external_sender(payload)
                {
                    eprintln!("voice: DAVE external sender rejected: {e}");
                }
                self.external_sender = Some(payload.to_vec());
                Vec::new()
            }
            27 => self.proposals(payload),
            29 | 30 => self.commit_or_welcome(op, payload),
            _ => Vec::new(),
        }
    }

    /// E2EE-encrypt one Opus frame before RTP packetization. Borrowed passthrough while DAVE is
    /// off; None = drop the frame (DAVE on but not ready: never send it unencrypted).
    pub fn encrypt_opus<'a>(&mut self, frame: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        // Silence frames are never encrypted (DAVE "silence packets").
        if self.protocol_version == 0 || frame == OPUS_SILENCE_PACKET {
            return Some(Cow::Borrowed(frame));
        }
        self.session.as_mut()?.encrypt_opus(frame).ok()
    }

    /// Decrypt a received frame of `user_id` (after transport decryption). Passthrough while
    /// DAVE is off or for passthrough-eligible frames during a transition; None = drop it.
    pub fn decrypt<'a>(&mut self, user_id: Id, frame: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        if frame == OPUS_SILENCE_PACKET {
            return Some(Cow::Borrowed(frame));
        }
        self.decrypt_media(user_id, MediaType::AUDIO, frame)
    }

    /// E2EE-encrypt one H.264 access unit (Annex-B, 4-byte start codes) before packetization;
    /// davey keeps NAL headers in clear (VOICE_NOTES §3.2). Passthrough while DAVE is off; None =
    /// drop the frame (DAVE on but not ready: never send it unencrypted).
    pub fn encrypt_h264<'a>(&mut self, frame: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        if self.protocol_version == 0 {
            return Some(Cow::Borrowed(frame));
        }
        self.session.as_mut()?.encrypt(MediaType::VIDEO, Codec::H264, frame).ok()
    }

    /// Decrypt one reassembled video frame of `user_id` (Annex-B, 4-byte start codes); like
    /// `decrypt`, minus the Opus silence rule.
    pub fn decrypt_video<'a>(&mut self, user_id: Id, frame: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        self.decrypt_media(user_id, MediaType::VIDEO, frame)
    }

    fn decrypt_media<'a>(&mut self, user_id: Id, media: MediaType, frame: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let pv = self.protocol_version;
        match &mut self.session {
            // With DAVE off davey is only asked while it may still hold keys (after a downgrade).
            Some(s) if s.is_ready() && (pv != 0 || s.can_passthrough(user_id.0)) => s.decrypt(user_id.0, media, frame).ok().map(Cow::Owned),
            _ => (pv == 0).then_some(Cow::Borrowed(frame)),
        }
    }

    /// 30-digit voice privacy code (groups of 5) once the group is established.
    #[allow(dead_code, reason = "for the voice panel UI, which is not built yet")]
    pub fn privacy_code(&self) -> Option<String> {
        if self.protocol_version == 0 {
            return None;
        }
        self.session.as_ref()?.voice_privacy_code().map(group_digits)
    }

    /// Create or re-create the MLS session for `pv` and announce a fresh key package (op 26).
    fn reinit(&mut self, pv: u16) -> Vec<Reply> {
        self.pending.clear();
        match self.try_reinit(pv) {
            Ok(key_package) => vec![binary(26, &[&key_package])],
            Err(e) => {
                eprintln!("voice: DAVE session setup failed: {e}");
                Vec::new()
            }
        }
    }

    fn try_reinit(&mut self, pv: u16) -> Result<Vec<u8>, Box<dyn Error>> {
        let pv = NonZeroU16::new(pv).ok_or("protocol version 0")?;
        let (me, group) = (self.user_id.0, self.group_id.0);
        let mut s = match self.session.take() {
            // Keeps the current keys until the next transition, so media continues meanwhile.
            Some(mut s) => {
                s.reinit(pv, me, group, None)?;
                s
            }
            None => {
                let mut s = DaveSession::new(pv, me, group, None)?;
                if let Some(es) = &self.external_sender
                    && let Err(e) = s.set_external_sender(es)
                {
                    eprintln!("voice: DAVE external sender rejected: {e}");
                }
                s
            }
        };
        let key_package = s.create_key_package();
        self.session = Some(s);
        Ok(key_package?)
    }

    /// Op 27: `[0 append | 1 revoke][proposals]`. Op 28 if davey commits to them.
    fn proposals(&mut self, payload: &[u8]) -> Vec<Reply> {
        let (Some(s), Some((&kind, proposals))) = (&mut self.session, payload.split_first()) else {
            return Vec::new();
        };
        let kind = match kind {
            0 => ProposalsOperationType::APPEND,
            1 => ProposalsOperationType::REVOKE,
            _ => return Vec::new(),
        };
        match s.process_proposals(kind, proposals, Some(&self.recognized)) {
            Ok(Some(cw)) => vec![binary(28, &[&cw.commit, cw.welcome.as_deref().unwrap_or_default()])],
            Ok(None) => Vec::new(),
            Err(e) => {
                eprintln!("voice: DAVE proposals rejected: {e}");
                Vec::new()
            }
        }
    }

    /// Op 29 announce commit / op 30 welcome: `[u16 BE transition id][MLS message]`.
    fn commit_or_welcome(&mut self, op: u8, payload: &[u8]) -> Vec<Reply> {
        let (Some(s), Some((tid, message))) = (&mut self.session, payload.split_first_chunk::<2>()) else {
            return Vec::new();
        };
        let tid = u16::from_be_bytes(*tid);
        let pv = s.protocol_version().get();
        let result = if op == 29 {
            s.process_commit(message).map_err(|e| e.to_string())
        } else {
            s.process_welcome(message, Some(&self.recognized)).map(|()| true).map_err(|e| e.to_string())
        };
        match result {
            // Ignored: no group or nothing proposed yet (e.g. right after a re-init).
            Ok(false) => Vec::new(),
            Ok(true) => {
                self.recovering = false;
                self.prepare(tid, pv);
                transition_ready(tid)
            }
            Err(e) if self.recovering => {
                eprintln!("voice: DAVE ignoring stale op {op} (transition {tid}): {e}");
                Vec::new()
            }
            Err(e) => {
                eprintln!("voice: DAVE op {op} (transition {tid}) invalid, re-keying: {e}");
                self.recovering = true;
                let mut out = vec![transition_json(31, tid)];
                out.extend(self.reinit(pv));
                out
            }
        }
    }

    /// Op 21 or a processed commit/welcome: receivers get the new keys now (old ones stay
    /// valid ≤10 s in davey); transition 0 executes at once, others on op 22.
    fn prepare(&mut self, tid: u16, pv: u16) {
        if let Some(s) = &mut self.session {
            if pv != 0 && self.protocol_version == 0 {
                // Upgrade: until op 22 members send plain or DAVE frames, so the decryptors made
                // now must take both (`apply` closes this at the transition).
                s.set_passthrough_mode(true, None);
            }
            if let Err(e) = s.prepare_transition(tid, pv) {
                eprintln!("voice: DAVE prepare transition {tid} failed: {e}");
            }
            if pv == 0 {
                // Downgrade: senders stop encrypting at the transition.
                s.set_passthrough_mode(true, None);
            }
        }
        if tid == 0 {
            self.apply(pv);
        } else {
            self.pending.insert(tid, pv);
        }
    }

    /// Op 22: senders (me included) switch keys or protocol version now.
    fn execute(&mut self, tid: u16) {
        if let Some(s) = &mut self.session
            && let Err(e) = s.execute_transition(tid)
        {
            eprintln!("voice: DAVE execute transition {tid} failed: {e}");
        }
        if let Some(pv) = self.pending.remove(&tid) {
            self.apply(pv);
        }
    }

    fn apply(&mut self, pv: u16) {
        if self.protocol_version == 0
            && pv != 0
            && let Some(s) = &mut self.session
        {
            // Upgrade: open a bounded passthrough window for every decryptor, then close it.
            s.set_passthrough_mode(true, None);
            s.set_passthrough_mode(false, Some(UPGRADE_PASSTHROUGH_SECS));
        }
        self.protocol_version = pv;
    }
}

/// Client binary frame `[op][parts...]`.
fn binary(op: u8, parts: &[&[u8]]) -> Reply {
    let mut out = Vec::with_capacity(1 + parts.iter().map(|p| p.len()).sum::<usize>());
    out.push(op);
    parts.iter().for_each(|p| out.extend_from_slice(p));
    Reply::Binary(out)
}

/// `{"op":op,"d":{"transition_id":tid}}` (op 23 ready, op 31 invalid commit/welcome).
fn transition_json(op: u8, tid: u16) -> Reply {
    Reply::Json(json!({ "op": op, "d": { "transition_id": tid } }))
}

/// Op 23 for a transition the server waits on; transition 0 needs no answer.
fn transition_ready(tid: u16) -> Vec<Reply> {
    if tid == 0 { Vec::new() } else { vec![transition_json(23, tid)] }
}

/// "123451234512345…" -> "12345 12345 12345 …".
fn group_digits(code: &str) -> String {
    let groups: Vec<&str> = code.as_bytes().chunks(5).filter_map(|c| std::str::from_utf8(c).ok()).collect();
    groups.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use davey::{SessionStatus, SigningKeyPair};

    const ME: Id = Id(1);

    /// A well-formed MLS `ExternalSender` (P-256 key + basic credential), like op 25 carries.
    fn external_sender() -> Vec<u8> {
        let key = SigningKeyPair::generate().public;
        assert_eq!(key.len(), 65);
        let mut out = vec![0x40, 65]; // MLS varint length (2 bytes because ≥ 64)
        out.extend_from_slice(&key);
        out.extend_from_slice(&[0, 1, 4, 0, 1, 1, 0]); // basic credential, 4-byte identity
        out
    }

    fn status(d: &Dave) -> Option<SessionStatus> {
        d.session.as_ref().map(DaveSession::status)
    }

    fn is_key_package(r: &Reply) -> bool {
        // [op 26][MLS 1.0][ciphersuite 2: DHKEMP256_AES128GCM_SHA256_P256]…
        matches!(r, Reply::Binary(b) if b.len() > 100 && b[..5] == [26, 0, 1, 0, 2])
    }

    fn json(op: u8, tid: u16) -> Reply {
        transition_json(op, tid)
    }

    #[test]
    fn passthrough_only_while_dave_is_off() {
        let mut d = Dave::new(ME, Id(2));
        assert_eq!(d.encrypt_opus(b"opus").as_deref(), Some(&b"opus"[..]));
        assert_eq!(d.decrypt(Id(3), b"opus").as_deref(), Some(&b"opus"[..]));
        assert!(d.on_session_description(0).is_empty());
        assert!(d.session.is_none() && d.privacy_code().is_none());
        d.on_session_description(1);
        assert!(d.encrypt_opus(b"opus").is_none(), "never unencrypted once DAVE is on");
        assert!(d.decrypt(Id(3), b"opus").is_none(), "no keys yet: drop, don't play garbage");
    }

    #[test]
    fn video_frames_follow_the_same_rules_without_the_silence_exception() {
        let mut d = Dave::new(ME, Id(2));
        let h264 = [0, 0, 0, 1, 0x65, 1, 2, 3];
        assert_eq!(d.encrypt_h264(&h264).as_deref(), Some(&h264[..]), "DAVE off: passthrough");
        assert_eq!(d.decrypt_video(Id(3), &h264).as_deref(), Some(&h264[..]));
        d.on_session_description(1);
        assert!(d.encrypt_h264(&h264).is_none(), "never unencrypted once DAVE is on");
        assert!(d.decrypt_video(Id(3), &h264).is_none());
        assert!(d.encrypt_h264(&OPUS_SILENCE_PACKET).is_none(), "the silence rule is audio only");
        assert!(d.decrypt_video(Id(3), &OPUS_SILENCE_PACKET).is_none());
    }

    #[test]
    fn silence_frames_always_pass() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(1);
        let silence = OPUS_SILENCE_PACKET;
        assert_eq!(d.encrypt_opus(&silence).as_deref(), Some(&silence[..]));
        assert_eq!(d.decrypt(Id(3), &silence).as_deref(), Some(&silence[..]));
    }

    #[test]
    fn session_description_sends_a_key_package() {
        let mut d = Dave::new(ME, Id(2));
        let replies = d.on_session_description(1);
        assert_eq!(replies.len(), 1);
        assert!(is_key_package(&replies[0]));
        // No external sender yet: no pending group.
        assert_eq!(status(&d), Some(SessionStatus::INACTIVE));
        d.on_binary(25, &external_sender());
        assert_eq!(status(&d), Some(SessionStatus::PENDING));
    }

    #[test]
    fn external_sender_before_session_description_is_buffered() {
        let mut d = Dave::new(ME, Id(2));
        assert!(d.on_binary(25, &external_sender()).is_empty());
        assert!(d.session.is_none());
        assert!(is_key_package(&d.on_session_description(1)[0]));
        assert_eq!(status(&d), Some(SessionStatus::PENDING));
    }

    #[test]
    fn bad_external_sender_is_ignored() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(1);
        assert!(d.on_binary(25, &[1, 2, 3]).is_empty());
        assert_eq!(status(&d), Some(SessionStatus::INACTIVE));
    }

    #[test]
    fn unsupported_protocol_version_sends_nothing() {
        let mut d = Dave::new(ME, Id(2));
        assert!(d.on_session_description(MAX_PROTOCOL_VERSION + 1).is_empty());
        assert!(d.encrypt_opus(b"opus").is_none());
    }

    #[test]
    fn bad_welcome_sends_invalid_and_rekeys_once() {
        let mut d = Dave::new(ME, Id(2));
        d.on_binary(25, &external_sender());
        d.on_session_description(1);
        let replies = d.on_binary(30, &[0, 7, 0xde, 0xad]);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], json(31, 7));
        assert!(is_key_package(&replies[1]));
        assert_eq!(status(&d), Some(SessionStatus::PENDING), "re-init keeps the external sender");
        // A second stale welcome (for the old key package) must not start another round.
        assert!(d.on_binary(30, &[0, 8, 0xbe, 0xef]).is_empty());
    }

    #[test]
    fn commit_for_a_pending_group_is_ignored() {
        let mut d = Dave::new(ME, Id(2));
        d.on_binary(25, &external_sender());
        d.on_session_description(1);
        assert!(d.on_binary(29, &[0, 9, 1, 2, 3]).is_empty());
        assert!(!d.recovering);
    }

    #[test]
    fn bad_proposals_send_nothing() {
        let mut d = Dave::new(ME, Id(2));
        assert!(d.on_binary(27, &[0, 1, 2]).is_empty(), "no session");
        d.on_binary(25, &external_sender());
        d.on_session_description(1);
        assert!(d.on_binary(27, &[0, 3, 1, 2, 3]).is_empty(), "garbage append");
        // A length byte with both top bits set: tls_codec's debug_assert, off via Cargo.toml.
        assert!(d.on_binary(27, &[0, 0xC0, 1, 2, 3]).is_empty(), "bad length prefix");
        assert!(d.on_binary(27, &[1]).is_empty(), "empty revoke");
        assert!(d.on_binary(27, &[9, 0]).is_empty(), "unknown operation type");
        assert!(d.on_binary(27, &[]).is_empty());
        assert!(!d.recovering, "proposals never trigger op 31");
    }

    #[test]
    fn short_or_unknown_binary_frames_are_ignored() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(1);
        for op in [29, 30] {
            assert!(d.on_binary(op, &[]).is_empty());
            assert!(d.on_binary(op, &[1]).is_empty());
        }
        assert!(d.on_binary(26, &[1, 2, 3]).is_empty());
        assert!(d.on_binary(99, &[]).is_empty());
    }

    #[test]
    fn downgrade_transition_waits_for_execute() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(1);
        let replies = d.on_json(21, &json!({"transition_id": 3, "protocol_version": 0}));
        assert_eq!(replies, vec![json(23, 3)]);
        assert!(d.encrypt_opus(b"opus").is_none(), "still DAVE until op 22");
        assert!(d.on_json(22, &json!({"transition_id": 4})).is_empty());
        assert_eq!(d.protocol_version, 1, "unknown transition id changes nothing");
        d.on_json(22, &json!({"transition_id": 3}));
        assert_eq!(d.protocol_version, 0);
        assert_eq!(d.encrypt_opus(b"opus").as_deref(), Some(&b"opus"[..]));
        assert_eq!(d.decrypt(Id(3), b"opus").as_deref(), Some(&b"opus"[..]));
    }

    #[test]
    fn transition_zero_executes_at_once() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(1);
        assert!(d.on_json(21, &json!({"transition_id": 0, "protocol_version": 0})).is_empty());
        assert_eq!(d.protocol_version, 0);
        assert!(d.pending.is_empty());
    }

    #[test]
    fn upgrade_from_off() {
        let mut d = Dave::new(ME, Id(2));
        d.on_session_description(0);
        assert!(d.on_json(24, &json!({"protocol_version": 1, "epoch": 2})).is_empty());
        let replies = d.on_json(24, &json!({"protocol_version": 1, "epoch": 1}));
        assert!(is_key_package(&replies[0]));
        assert_eq!(d.encrypt_opus(b"opus").as_deref(), Some(&b"opus"[..]), "off until the transition");
        assert_eq!(d.on_json(21, &json!({"transition_id": 5, "protocol_version": 1})), vec![json(23, 5)]);
        d.on_json(22, &json!({"transition_id": 5}));
        assert_eq!(d.protocol_version, 1);
        assert!(d.encrypt_opus(b"opus").is_none(), "no group key: drop");
    }

    #[test]
    fn malformed_json_ops_are_ignored() {
        let mut d = Dave::new(ME, Id(2));
        assert!(d.on_json(21, &json!({"transition_id": 70000, "protocol_version": 1})).is_empty());
        assert!(d.on_json(21, &json!({"transition_id": 1})).is_empty());
        assert!(d.on_json(22, &json!(null)).is_empty());
        assert!(d.on_json(24, &json!({"epoch": 1})).is_empty());
        assert!(d.on_json(5, &json!({})).is_empty());
        assert!(d.session.is_none() && d.pending.is_empty());
    }

    #[test]
    fn recognized_users_always_include_me() {
        let mut d = Dave::new(ME, Id(2));
        d.on_clients_connect(&[Id(5), Id(6), Id(5), ME]);
        assert_eq!(d.recognized, vec![1, 5, 6]);
        d.on_client_disconnect(Id(5));
        d.on_client_disconnect(ME);
        assert_eq!(d.recognized, vec![1, 6]);
    }

    #[test]
    fn privacy_code_is_grouped_by_five() {
        assert_eq!(group_digits("123456789012345678901234567890"), "12345 67890 12345 67890 12345 67890");
        assert_eq!(group_digits(""), "");
    }

    #[test]
    fn replies_are_framed_per_voice_notes() {
        assert_eq!(binary(28, &[&[1, 2], &[], &[3]]), Reply::Binary(vec![28, 1, 2, 3]));
        assert_eq!(transition_ready(0), vec![]);
        assert_eq!(transition_ready(9), vec![Reply::Json(json!({"op": 23, "d": {"transition_id": 9}}))]);
    }

    #[test]
    fn dave_can_live_in_a_shared_mutex() {
        fn send<T: Send>() {}
        send::<Dave>();
    }
}
