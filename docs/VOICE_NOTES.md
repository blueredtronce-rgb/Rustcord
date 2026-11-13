# Rustcord voice, video and screen share (user account, Windows first)

Edited 2026-11-11 from the voice research draft, checked against the cached sources, the davey 0.2.0 source and the current `Cargo.lock`. Text features are in `docs/FEATURES_NOTES.md`; gateway basics in `docs/PROTOCOL_NOTES.md`.

**Source keys**
| Key | Source |
|---|---|
| UD-V | https://docs.discord.food/topics/voice-connections (raw `pages/topics/voice-connections.mdx`) |
| UD-GW / UD-OP / UD-VR / UD-CH | https://docs.discord.food/gateway/gateway-events , /gateway/opcodes-and-close-codes , /resources/voice , /resources/channel |
| DAVE | https://daveprotocol.com (github.com/discord/dave-protocol `protocol.md` v1.1.4) |
| OFF | https://discord.com/developers/docs/topics/voice-connections and change log "Deprecating Non-E2EE Voice Calls" |
| DPS | dolfies/discord.py-self `gateway.py`, `voice_state.py`, `voice_client.py`, `voice_media.py`, `channel.py`, `http.py`, `state.py` |
| SB | serenity-rs/songbird `src/driver/crypto.rs` (reference only) |
| DAVEY | Snazzah/davey 0.2.0 `davey/src/session.rs` (signatures checked) |

No live test was possible (no browser connected). Every "official client does X" without a source is UNCONFIRMED.

## Editor's corrections to the draft

| # | Draft | Correction |
|---|---|---|
| 1 | Phase A "11 crates / 1.09 MiB with 0.11 crypto; 0 extra crypto on 0.10" | Re-measured against the **current** Cargo.lock: Phase A on the recommended 0.10 crypto versions, standalone = **22 crates / 1.36 MiB**. After davey is in, A adds only **3 crates / 0.76 MiB** (cpal, dasp_sample, opus-rs). Because A is useless outside Stage channels without C, ask for approval of **A+C together: 109 crates / 6.32 MiB**. |
| 2 | Totals | Confirmed: davey alone 106 / 5.55 MiB; A–E 115 / 7.57 MiB; plus the `emojis` crate from FEATURES_NOTES = 118 / 7.73 MiB. |
| 3 | Licenses | The davey tree is not only MIT/Apache: it contains **MPL-2.0** (`hpke-rs`, `hpke-rs-crypto`, `hpke-rs-rust-crypto`, `hpke-rs-libcrux`; file-level copyleft, fine for linking unmodified) and **BSD-3** (`curve25519-dalek`, `ed25519-dalek`, `x25519-dalek`, `subtle`, `instant`). No GPL/AGPL anywhere in the recommended set. |
| 4 | Capabilities 253 → 4349 | Correct: `src/gateway/conn.rs:36` is 253; +AUTO_CALL_CONNECT (4096) = **4349**. |
| 5 | davey API | `process_proposals(ProposalsOperationType, &[u8], Option<&[u64]>) -> Result<Option<CommitWelcome>>`; `process_commit(&[u8]) -> Result<bool>`; `process_welcome(&[u8], Option<&[u64]>) -> Result<()>`; `prepare_transition(u16 tid, u16 pv)`; `execute_transition(u16) -> Result<bool>`; `encrypt(MediaType, Codec, &[u8])`, `encrypt_opus(&[u8])`, `decrypt(u64 user, MediaType, &[u8]) -> Result<Vec<u8>>`; `new/reinit(NonZeroU16 pv, u64 user, u64 channel, Option<&SigningKeyPair>)`. |
| 6 | DAVE mandatory | Confirmed twice: UD-V "only support E2EE calls starting on March 1st, 2026 … disconnected … with close code 4017 … Stage channels will not be affected", and the OFF change log entry. |
| 7 | Go Live DAVE group id | Now confirmed by UD-V: "stream RTC uses the media-session ID, which is one less than the stream `rtc_server_id`, as the DAVE/MLS group ID". Still one source family. |
| 8 | `rtc_connection_id` | Needs a UUID v4: format 16 bytes from `getrandom` (already a direct dependency; set version/variant bits). No `uuid` crate. |

---

## 0. Key decisions

1. **DAVE is mandatory** (since 2026-03-01) for DMs, group DMs, guild voice channels and Go Live; non-DAVE clients get close **4017**. **Stage channels are exempt** → the only place to test phase A without DAVE. DAVE must ship right after A.
2. **Transport: native UDP**, not WebRTC. Allowed modes: `aead_aes256_gcm_rtpsize` (preferred when offered) and `aead_xchacha20_poly1305_rtpsize` (must support; always offered). xsalsa20 modes and plain `aead_aes256_gcm` were discontinued 2024-11-18 (OFF).
3. **Stack (pure Rust except openh264):** cpal 0.18.2 (audio I/O), opus-rs 0.1.42 (Opus, pure Rust port of libopus 1.6), davey 0.2.0 (DAVE; same engine as songbird 0.6 and the Python/JS bindings), aes-gcm 0.10.3 + chacha20poly1305 0.10.1 (the versions davey already pulls), openh264 0.9.10 (H.264 decode/encode, C++ via `cc`/MSVC), windows-capture 2.0.1 (screen capture).
4. **Declare only Opus + H264** to the SFU → every sender sends you H264 and one library covers both directions (§5.1).
5. Everything is user-initiated: join, ring, stream, watch only on click; no auto-join, no auto-reconnect on 4014/4021/4022.

---

## 1. Joining (main gateway)

### 1.1 Op 4 Update Voice State (send) [UD-GW]
```json
{"op":4,"d":{"guild_id":"<gid>|null","channel_id":"<cid>|null","self_mute":false,"self_deaf":false,
 "self_video":false,"flags":0,"preferred_region":"<r0>","preferred_regions":["r0","r1"]}}
```
- Required: `guild_id`, `channel_id` (`null` = leave), `self_mute`, `self_deaf`. Optional: `self_video` (camera on), `preferred_region(s)` (DPS sends them only on join, from READY `geo_ordered_rtc_regions`, `preferred_region = regions[0]`), `flags` (1<<0 CLIPS_ENABLED, 1<<1 ALLOW_VOICE_RECORDING, 1<<2 ALLOW_ANY_VIEWER_CLIPS; UD example sends 3; **send 0** — no clips in Rustcord; official value UNCONFIRMED).
- DM/group call: `guild_id:null, channel_id:<dm id>`. Leave: same with `channel_id:null`.
- Mute/deafen/camera: resend op 4 with the same ids. Deafen sets `self_mute` and `self_deaf` (long-standing behaviour, UNCONFIRMED). Debounce ~250 ms (op 4 counts against 120 events / 60 s).
- One voice connection per user session; switching = op 4 with new ids.
- Op 5 Voice Server Ping `{"op":5,"d":null}`: ask the gateway to recheck a bad voice server; only as a manual "reconnect".

### 1.2 Dispatches after op 4
Wait for **both** VOICE_STATE_UPDATE (yours; carries `session_id`) and VOICE_SERVER_UPDATE `{token, guild_id?, channel_id? (private), endpoint?}`. `endpoint` has no scheme (`smart.loyal.discord.media:1337`): keep the port, prepend `wss://`, never cache. `endpoint:null` = reallocating → close the voice WS and wait.
- **Channel full:** no events at all → check `user_limit` locally first (MOVE_MEMBERS bypasses; video limit +1 with MANAGE_CHANNELS); time out after ~10 s with "channel full".
- **Moved / channel change in the same guild:** new VOICE_STATE_UPDATE + VOICE_SERVER_UPDATE (maybe same endpoint) with a **new token** → close and Identify again (no resume).
- Own VOICE_STATE_UPDATE with `channel_id:null` you didn't request → kicked/disconnected; tear down, no reconnect (DPS).
- Own `user_id` with another `session_id` → you are in voice on another device; show it, don't connect media.

### 1.3 Voice channel user list (phase 0)
- Initial: READY_SUPPLEMENTAL `guilds[].voice_states` (caps bit 5 is set; entries omit `guild_id` and `member`).
- Updates: VOICE_STATE_UPDATE full object `{guild_id?, channel_id (null = left), user_id, member?, session_id, deaf, mute, self_deaf, self_mute, self_stream?, self_video, suppress, request_to_speak_timestamp, connected_at?}`.
- Model `HashMap<channel_id, HashMap<user_id, VoiceState>>`; remove the user from the old channel on every update.
- Channel status + "in voice since": main op **43** `{"guild_id":"..","fields":["status","voice_start_time"]}` → CHANNEL_INFO `{guild_id, channels:[{id,status?,voice_start_time?}]}`; live VOICE_CHANNEL_STATUS_UPDATE `{id,guild_id,status}`, VOICE_CHANNEL_START_TIME_UPDATE `{id,guild_id,voice_start_time}`.
- Caps 253 lack PASSIVE_GUILD_UPDATE (bits 11/14), so plain VOICE_STATE_UPDATE should arrive for all guilds (behaviour in unsubscribed large guilds UNCONFIRMED).

### 1.4 DM and group calls (phase 0)
- Set capability **AUTO_CALL_CONNECT (4096)** → `CAPABILITIES = 4349`; otherwise send op 13 `{"op":13,"d":{"channel_id":".."}}` per DM to learn about existing calls.
- Events: CALL_CREATE `{channel_id, message_id, region, ringing:[user_id], voice_states:[…]}`, CALL_UPDATE `{channel_id, message_id, region, ringing}`, CALL_DELETE `{channel_id, unavailable?}`.
- Incoming: `ringing` contains you → ring UI (Ctrl+Enter accepts). Accept = op 4 `guild_id:null`. Decline = `POST /channels/{id}/call/stop-ringing {}`.
- Start: op 4 into the DM (server creates the call → CALL_CREATE), then ring. DPS `_initial_ring`: 1:1 → ring if friend, else only if `GET /channels/{id}/call` says `{ringable:true}`; group → always; only for a newly started call.

| Method | Path | Body | Result |
|---|---|---|---|
| GET | `/channels/{id}/call` | — | `{ringable}` |
| POST | `/channels/{id}/call/ring` | `{"recipients": null \| [ids]}` | 204, CALL_UPDATE |
| POST | `/channels/{id}/call/stop-ringing` | `{"recipients":[ids]}` or `{}` (self) | 204 |
| PATCH | `/channels/{id}/call` | `{"region":"<id>"}` | 204, CALL_UPDATE |
Normal per-route buckets; only from clicks. Call history message (type 3) is in PROTOCOL_NOTES.

### 1.5 Optional parity features
- Channel status `PUT /channels/{id}/voice-status {"status":?string}` (SET_VOICE_CHANNEL_STATUS 1<<48; + MANAGE_CHANNELS when not connected).
- Emoji effects `POST /channels/{id}/voice-channel-effects {emoji_id?, emoji_name?, animation_type:1, animation_id:0}`; receive VOICE_CHANNEL_EFFECT_SEND `{channel_id,guild_id,user_id,animation_type,animation_id,emoji?,sound_id?,sound_volume?}`.
- Soundboard `POST /channels/{id}/send-soundboard-sound {sound_id, source_guild_id?}`; clients play the sound locally on the effect event (model UNCONFIRMED).
- Per-user volume / local mute: settings proto `audio_context_settings.user[id] {muted, volume 0–200, soundboard_muted}` (`.stream[...]` for Go Live). MVP: local config.
- Moderation `PATCH /guilds/{gid}/members/{uid}` `{mute}` / `{deaf}` / `{channel_id|null}` (MUTE_MEMBERS / DEAFEN_MEMBERS / MOVE_MEMBERS).
- Stage `PATCH /guilds/{gid}/voice-states/@me {channel_id, suppress?, request_to_speak_timestamp?}`.

### 1.6 Local permission checks
| Permission | Bit | Effect |
|---|---|---|
| CONNECT | 1<<20 | join |
| SPEAK | 1<<21 | send audio |
| MUTE_MEMBERS / DEAFEN_MEMBERS / MOVE_MEMBERS | 1<<22 / 23 / 24 | moderation; MOVE bypasses user limit |
| USE_VAD | 1<<25 | without it **force push-to-talk** |
| PRIORITY_SPEAKER | 1<<8 | priority speaker flag |
| STREAM | 1<<9 | camera and Go Live |
| REQUEST_TO_SPEAK | 1<<32 | stage |
| SET_VOICE_CHANNEL_STATUS | 1<<48 | channel status |
Server `mute`/`deaf` and stage `suppress` = never transmit.

---

## 2. Voice gateway and media transport

### 2.1 URL
`wss://{endpoint}/?v=9` — always pass `v` (no default); no compression. UD-V: "please use version 9" (adds `channel_id` to Identify/Resume); OFF recommends 8 and rejects <4. v8+ = buffered resume, `seq` on replayable JSON, 2-byte sequence prefix on server binary frames.

### 2.2 Opcodes [UD-OP, DAVE]
| Op | Name | Dir | Frame |
|---|---|---|---|
| 0 | Identify | send | JSON |
| 1 | Select Protocol | send | JSON |
| 2 | Ready | recv | JSON |
| 3 | Heartbeat | both | JSON |
| 4 | Session Description | recv | JSON |
| 5 | Speaking | both | JSON |
| 6 | Heartbeat ACK | recv | JSON |
| 7 | Resume | send | JSON |
| 8 | Hello | recv | JSON |
| 9 | Resumed | recv | JSON |
| 11 | Clients Connect `{user_ids}` | recv | JSON |
| 12 | Video | both | JSON |
| 13 | Client Disconnect `{user_id}` | recv | JSON |
| 14 | Session Update | both | JSON |
| 15 | Media Sink Wants | both | JSON |
| 16 | Voice Backend Version (send `{}`) | both | JSON |
| 18 | Client Flags `{user_id,flags}` | recv | JSON |
| 19 | Speed Test | recv | JSON |
| 20 | Client Platform `{user_id,platform}` | recv | JSON |
| 21 / 22 | DAVE Prepare / Execute Transition | recv | JSON |
| 23 | DAVE Transition Ready | send | JSON |
| 24 | DAVE Prepare Epoch | recv | JSON |
| 25 | MLS External Sender | recv | **binary** |
| 26 | MLS Key Package | send | **binary** |
| 27 | MLS Proposals | recv | **binary** |
| 28 | MLS Commit Welcome | send | **binary** |
| 29 | MLS Announce Commit | recv | **binary** (UD's table says JSON; DAVE and DPS say binary) |
| 30 | MLS Welcome | recv | **binary** |
| 31 | MLS Invalid Commit Welcome | send | JSON |
| 32 | No Route `{}` | send | JSON |
**Dispatch on the WebSocket frame type (text vs binary), not on the table.**

### 2.3 Handshake [UD-V payloads; DPS behaviour]
1. Connect → **Identify**:
   ```json
   {"op":0,"d":{"server_id":"<guild_id or dm channel_id>","channel_id":"<cid>","user_id":"<me>",
    "session_id":"<VOICE_STATE_UPDATE>","token":"<VOICE_SERVER_UPDATE>",
    "video":true,"streams":[{"type":"video","rid":"100","quality":100}],"max_dave_protocol_version":1}}
   ```
   `streams[]`: `{type:"video"|"screen"|"test", rid, quality 0–100, active?, max_bitrate?, max_framerate?, max_resolution?{type:"fixed"|"source",width,height}, ssrc?, rtx_ssrc?}`. Audio-only: `video:false, streams:[]` (DPS default); whether `video:true` is needed just to receive video is UNCONFIRMED — send it when rendering video.
2. **Hello** `{v, heartbeat_interval}` → heartbeat every `heartbeat_interval` (desktop) or `min(hbi, 5000)` (web).
3. **Ready** `{ssrc, ip, port, modes[], experiments[], streams[]}` → open UDP to `ip:port`, IP discovery (§2.6), pick AES-GCM if offered else XChaCha; optionally op 16 `{}` (DPS).
4. **Select Protocol** (once per session; a second one closes with 4006):
   ```json
   {"op":1,"d":{"protocol":"udp","data":{"address":"<discovered ip>","port":<port>,"mode":"aead_aes256_gcm_rtpsize"},
    "codecs":[{"name":"opus","type":"audio","priority":1000,"payload_type":120},
              {"name":"H264","type":"video","priority":1000,"payload_type":103,"rtx_payload_type":104,"encode":true,"decode":true}],
    "rtc_connection_id":"<uuid v4>","experiments":[]}}
   ```
   Never PT 96 (probes). Omitting `codecs` = Opus/120 only.
5. **Session Description** `{audio_codec, video_codec, media_session_id, mode, secret_key:[32 u8], keyframe_interval?, dave_protocol_version}` → if `dave_protocol_version > 0`, create the DAVE session and send op 26 (§3.3).
6. **Speaking** `{"speaking":<flags>,"delay":0,"ssrc":<ready.ssrc>}` before sending/receiving media, else "Invalid SSRC". Send `1` when transmission starts, `0` after the trailing silence frames. Receive-only: one `speaking:0` (acceptance UNCONFIRMED; if no media flows, send 1 then 0). Flags 1 VOICE, 2 SOUNDSHARE, 4 PRIORITY.
7. Received during the session: op 5 `{user_id, ssrc, speaking}` (**SSRC → user map**, also for users already present), op 11/18/20 (participants), op 13 (drop the user's SSRCs and decoders), op 12 (video SSRCs), op 14 `{audio_codec?, video_codec?, media_session_id?, keyframe_interval?}`.

### 2.4 Heartbeat, seq, resume, binary frames
- Heartbeat `{"op":3,"d":{"t":<unix ms>,"seq_ack":<last seq or -1>}}`; ACK `{"op":6,"d":{"t"}}`; server op 3 = heartbeat now.
- `seq`: top-level on replayable JSON; server binary frames `[u16 BE seq][u8 op][payload]`; keep the last seen of either. Client binary frames `[u8 op][payload]`.
- Resume: reconnect WS, `{"op":7,"d":{"server_id","channel_id","session_id","token","seq_ack"}}` → op 9 + replay; on failure (close) redo from op 4. UDP socket and keys survive; never re-send Select Protocol.

### 2.5 Close codes [UD-OP]
| Code | Meaning | Action |
|---|---|---|
| 4001/4002/4003/4005/4012/4016/4020 | protocol bug | no retry; log |
| 4004 / 4006 / 4009 / 4011 | auth failed / session invalid / timeout / server not found | full re-join (op 4) |
| 4013 / 4015 | WebRTC / voice server crashed | resume |
| 4014 | disconnected (kicked/moved/main session gone) | **no** reconnect; wait for VOICE_STATE_UPDATE |
| 4022 | channel deleted / call ended | **no** reconnect |
| 4017 | E2EE required | DAVE missing/broken |
| 4021 | rate limited | **no** reconnect |
Backoff for retryable codes 1, 2, 4, 8 … ≤30 s, ≤5 tries, then op 4 `channel_id:null`.

### 2.6 UDP control packets [UD-V]
```rust
let mut req = [0u8; 74];                               // IP discovery, big-endian, retry 3x with 1 s timeout
req[0..2].copy_from_slice(&1u16.to_be_bytes());        // 1 request, 2 response
req[2..4].copy_from_slice(&70u16.to_be_bytes());       // length excluding type+length
req[4..8].copy_from_slice(&ssrc.to_be_bytes());
// response: ip = resp[8..72] up to the first NUL (ASCII), port = u16::from_be_bytes([resp[72], resp[73]])
```
- Ping `[13 37 CA FE][u32 seq]` → reply `13 37 F0 0D` + seq; 5 s timeout; doubles as NAT keepalive while silent (INFERENCE).
- Demux order: 74-byte `00 02…` discovery reply → `13 37 F0 0D` ping reply → RTCP (`b[0]>>6 == 2` and `b[1]` in 192..=223; types 200 SR, 201 RR, 205 RTPFB, 206 PSFB) → RTP (drop PT 96).

### 2.7 Transport encryption (rtpsize AEAD) [UD-V, SB, DPS]
```
[clear: 12B RTP header + 4*CC CSRCs + (X bit) 4B ext preamble 0xBEDE,len][ciphertext: ext elements (len*4) + payload][16B tag][4B BE nonce counter]
```
- AAD = clear part; nonce 12 bytes (AES-256-GCM) or 24 bytes (XChaCha20-Poly1305) with the BE counter in the first 4 bytes, rest zero; key = `secret_key`.
- One u32 counter per key for everything sent (RTP and RTCP), random start, `wrapping_add(1)`.
- RTCP: clear part = 8-byte header (V/P/RC, PT, length, sender SSRC); body encrypted + tag + 4-byte nonce.
- Decrypt: split nonce (last 4) and tag (16 before), decrypt in place, skip `len*4` bytes of decrypted extension elements.
```rust
// aes-gcm 0.10 / chacha20poly1305 0.10 (aead 0.5, AeadInPlace)
let mut n = [0u8; 12]; n[..4].copy_from_slice(&ctr.to_be_bytes());
let tag = aes.encrypt_in_place_detached(Nonce::from_slice(&n), &pkt[..clear], &mut pkt[clear..])?;
pkt.extend_from_slice(&tag); pkt.extend_from_slice(&ctr.to_be_bytes()); ctr = ctr.wrapping_add(1);
```

### 2.8 Sending audio
Opus encode → DAVE `encrypt_opus` → RTP → header extensions → transport encrypt → UDP.
- RTP: `0x80` (`|0x10` with extensions), `0x78` (PT 120), seq u16 BE (random start, wraps), timestamp u32 BE (random start, +960 per 20 ms; after a gap advance by elapsed samples), SSRC from Ready.
- Opus: 48 kHz, **2 channels** (duplicate mono mic), 20 ms = 960 samples/channel; bitrate = channel `bitrate` (default 64000; DM bitrate UNCONFIRMED → 64k); VOIP application, in-band FEC on, expected loss 10–15 % (DPS: fec, loss 0.15).
- Stop: **5 silence frames `F8 FF FE`** (DAVE passes them unencrypted), then op 5 `speaking:0`.
- Audio extensions (one-byte profile 0xBEDE; element `(id<<4)|(len-1)` + data; pad to 4): ID 1 ssrc-audio-level (1 byte, `0x80` voice bit, low 7 bits −dBov, 127 = silence); ID 3 abs-send-time (3 bytes, 6.18 fixed point); ID 9 Discord speaker `((f&3)<<1)|((f&4)>>2)`; ID 10 mid `b"audio"`. Mandatory? UNCONFIRMED (OFF doesn't require them). Minimal: 1 + 9.
- Input modes: VAD = RMS dBFS gate with threshold slider (−100..0 dB) and ~300 ms hangover (INFERENCE; official uses its own VAD/Krisp). Push-to-talk = poll `GetAsyncKeyState` for the bound key every 10 ms (add windows-sys feature `Win32_UI_Input_KeyboardAndMouse`; no new crate; works unfocused, doesn't swallow the key), release delay ~20 ms (UNCONFIRMED default). Force PTT without USE_VAD.
- Mute = stop sending (after silence frames) **and** op 4 `self_mute:true`. Never send while server-muted or suppressed.

### 2.9 Receiving audio
1. Transport-decrypt. 2. SSRC → user from op 5; queue unknown SSRCs ~200 ms, then drop (never guess). 3. DAVE `decrypt(user_id, MediaType::AUDIO, payload)`. 4. Per-SSRC jitter buffer 3–5 packets, u16 wraparound ordering. 5. Loss: `decode_fec` from the next packet, else PLC. 6. Opus decode 48 kHz stereo into a 5760-sample/channel buffer (packets up to 120 ms). 7. Mix with per-user gain, local mute, deafen, clamp ±1.0. 8. cpal output callback reading a `Mutex<VecDeque<f32>>` via `try_lock` (silence if locked).
Speaking ring: on if a non-silence packet arrived in the last ~250 ms (INFERENCE); off on silence frames or timeout; ext 9 missing/0 = VOICE.

### 2.10 RTCP
Audio: Sender Report every ~5 s per sent SSRC and parse RR ("should", UD-V) — optional for the first milestone. Video: Generic NACK (RTPFB FMT 1), PLI (PSFB FMT 1) and RTX are required (§5.3).

---

## 3. DAVE (Discord Audio & Video E2EE)

### 3.1 Status
Mandatory since 2026-03-01 (UD-V, OFF) for DMs, group DMs, guild voice, Go Live; close 4017 otherwise; Stage exempt. Protocol version 1 only: MLS 1.0, ciphersuite 2 `DHKEMP256_AES128GCM_SHA256_P256`, basic credentials, exactly one External Senders extension. Transport encryption stays underneath.

### 3.2 Protocol summary [DAVE, UD-V]
- Identify `max_dave_protocol_version:1`; Session Description returns `dave_protocol_version`.
- MLS group id = voice channel id (guild VC or DM id); **Go Live: `rtc_server_id - 1`**.
- Credential identity = user id as BE u64; key-package lifetime `not_before=0, not_after=2^64-1`.
- The voice gateway is the MLS delivery service and external sender (proposes Add/Remove); every member commits; first valid commit per epoch wins.
- Per-sender key `MLS-Exporter("Discord Secure Frames v0", sender_id LE u64, 16)` → hash ratchet; generation = top byte of the 32-bit nonce; keep old keys ≤10 s; nonce wrap continues the generation.
- Frame trailer: `[interleaved frame][8B truncated AES-128-GCM tag][ULEB128 nonce][ULEB128 (offset,len) clear ranges…][u8 supplemental size][0xFA 0xFA]`; 12-byte nonce = 8 zero bytes + 4-byte nonce; AAD = clear ranges; each key/nonce once.
- Sender codec rules (receiver is codec-agnostic): Opus, VP9 fully encrypted; VP8 10 bytes clear on keyframes else 1; H264/H265 NAL headers and non-VCL clear, VCL encrypted, re-encrypt with nonce+1 (≤10×) if a start code appears, 3-byte start codes in clear parts become 4-byte; AV1 OBU headers clear, drop temporal-delimiter/tile-list/padding OBUs.
- Passthrough during transitions for frames that fail the magic/size check; `F8 FF FE` always passes.
- UI: 30-digit "voice privacy code" (groups of 5) from the epoch authenticator + per-user verification codes.
- Optional persistent identity keys: `PUT /voice/public-keys {key_version:1, public_key:"data:application/octet-stream;base64,<X9.62 compressed P-256>", signature}`; `POST /voice/{user_id}/match-public-key` → `{is_match}`. Ephemeral keys (davey default) are allowed → skip.

### 3.3 Opcode handling with davey 0.2.0
`let mut s = DaveSession::new(NonZeroU16::new(pv).unwrap(), my_id, group_id, None)?;` re-create with `s.reinit(…)`.
| Received | Payload | Action |
|---|---|---|
| op 4 Session Description, `dave_protocol_version>0` | JSON | `new`/`reinit`, then send **op 26** = `[26] ++ s.create_key_package()?` |
| op 25 External Sender | bin `[seq u16][25][ExternalSender]` | `s.set_external_sender(&p)`; **buffer it** if the session doesn't exist yet (ordering vs op 4 UNCONFIRMED) |
| op 11 / op 13 | JSON | maintain `recognized_user_ids` (include yourself) |
| op 27 Proposals | bin `[seq][27][u8 op_type 0 append / 1 revoke][…]` | `s.process_proposals(op_type.into(), &p[1..], Some(&recognized))?` → `Some(CommitWelcome{commit, welcome})` → send **op 28** = `[28] ++ commit ++ welcome.unwrap_or_default()` |
| op 29 Announce Commit | bin `[seq][29][u16 BE tid][commit]` | `s.process_commit(&p[2..])`: `Ok(true)` → `s.prepare_transition(tid, pv)`, and if `tid != 0` send op 23 `{"transition_id":tid}`; `Err` → op 31 `{"transition_id":tid}`, reinit, new op 26 |
| op 30 Welcome | bin `[seq][30][u16 BE tid][welcome]` | `s.process_welcome(&p[2..], Some(&recognized))`, then as op 29 |
| op 21 Prepare Transition | JSON `{protocol_version, transition_id}` | `s.prepare_transition(tid, pv)`; `tid==0` executes immediately; else send op 23. pv 0 = downgrade (passthrough) |
| op 22 Execute Transition | JSON `{transition_id}` | `s.execute_transition(tid)`; senders switch keys now |
| op 24 Prepare Epoch | JSON `{protocol_version, epoch}` | `epoch==1` → `reinit` + new op 26 |
Media: send `s.encrypt_opus(&opus)` / `s.encrypt(MediaType::VIDEO, Codec::H264, &annexb)` when `s.is_ready()` — after the codec, before packetisation. Receive `s.decrypt(user_id, media_type, &frame)`; on error drop and count. UI `s.voice_privacy_code()`, `s.get_verification_code(uid)`.

### 3.4 DAVE implementation options
| Crate | Version | License | Own .crate | Verdict |
|---|---|---|---|---|
| **davey** | 0.2.0 (2026-11-02) | MIT (tree: MIT/Apache/BSD/MPL-2.0) | 59.7 KiB | **Recommended.** Session + cryptor + codec processors (Opus, VP8, VP9, H264, H265, AV1) on openmls 0.8.3. Pin `=0.2.0`. Tree **106 crates / 5.55 MiB**, incl. winapi 0.3.9 (1.17 MiB, via openmls `js` → fluvio-wasm-timer → parking_lot 0.11; davey enables `js`, can't be turned off), libcrux, p384, curve25519, rayon, older RustCrypto duplicates |
| openmls | 0.9.2 + rust_crypto 0.6 | MIT | 509 KB | write DAVE yourself: 100 crates / 5.27 MiB → no saving |
| mls-rs | 0.57.0 + crypto-rustcrypto | Apache-2.0 OR MIT | 281 KB | 45 crates / 2.04 MiB but ~1.5k LoC of hand-written DAVE logic; ciphersuite 2 support UNCONFIRMED |
| dave (cacophony) | 0.3.0 | **AGPL-3.0-only** | 57 KB | license blocks it |
| libdave (C++) | — | — | — | C++ + mlspp + OpenSSL/BoringSSL; no maintained Rust bindings |

---

## 4. Dependency table (Windows MSVC; compressed `.crate` sizes vs the current Cargo.lock)

Measured 2026-11-11: each set added to a copy of the current `Cargo.toml`, resolved with `cargo update --workspace` (index only, nothing downloaded), new packages reachable on `x86_64-pc-windows-msvc` summed from the crates.io `crate_size`. Script: `<local scratch, not in repo>`; per-set listings under `…\scratchpad\sz2\<set>\size.txt`.

### 4.1 Recommended
| Need | Crate | Version | License | New crates / compressed (alone) | Native build needs |
|---|---|---|---|---|---|
| Opus encode/decode, FEC, PLC | **opus-rs** | =0.1.42 | BSD-3-Clause | 1 / 0.53 MiB | none (pure Rust, 0 deps; libopus 1.6 port; very young, pin) |
| Audio I/O (WASAPI) | **cpal** | =0.18.2 | Apache-2.0 | 2 / 0.23 MiB (+ dasp_sample) | none; uses `windows` 0.62 already in Cargo.lock (0.62.2). WASAPI resamples to 48 kHz since 0.17.3 (capture side UNCONFIRMED → rubato fallback) |
| Transport AEAD | **aes-gcm** + **chacha20poly1305** | =0.10.3 / =0.10.1 | Apache-2.0 OR MIT | 0 once davey is in (part of its tree); alone with cpal+opus-rs: phase A = 22 crates / 1.36 MiB | none; AES-NI autodetect |
| DAVE E2EE | **davey** | =0.2.0 | MIT (+ MPL-2.0/BSD-3 in tree) | 106 / 5.55 MiB | none (pure Rust) |
| H.264 decode/encode | **openh264** (+ openh264-sys2, wide, safe_arch, nasm-rs) | =0.9.10 | BSD-2-Clause | 5 / 1.20 MiB | C++ compiled by `cc` with MSVC (installed); **nasm** optional (~3× faster; not installed): 1080p encode 23.7 ms/frame without, 8.2 ms with. Decoder officially Constrained Baseline (High partly works, UNCONFIRMED for all senders). Built from source = no Cisco patent coverage (Cisco's binary DLL via `libloading` is the covered path) |
| Screen capture | **windows-capture** | =2.0.1 | MIT | 5 / 0.37 MiB alone; **+1 crate / 55 KiB** with davey (rayon/crossbeam shared) | none; Windows Graphics Capture (Win10 1903+, border removable on Win11) + DXGI duplication |
| UDP socket | tokio `net` | (feature) | — | 0 (already compiled via reqwest/tungstenite; add `"net"` explicitly) | none |
| Push-to-talk key poll | windows-sys feature `Win32_UI_Input_KeyboardAndMouse` | 0.61 (direct dep) | — | 0 | none |
| Camera | Media Foundation `IMFSourceReader` via existing `windows` 0.62 (`Win32_Media_MediaFoundation`) | — | — | 0 | none (`nokhwa` not measured) |
| UUID v4 | 16 bytes from `getrandom` (direct dep) | — | — | 0 | none |

### 4.2 Optional
| Crate | Version | License | New crates / size | When |
|---|---|---|---|---|
| nnnoiseless (`default-features=false`) | 0.5.2 | BSD-3-Clause | 10 / 0.56 MiB | noise suppression (RNNoise, 48 kHz mono, 480-sample frames); official uses Krisp |
| rubato | 5.0.1 | MIT OR Apache-2.0 | 12 / 0.59 MiB (shares rustfft with nnnoiseless) | only if 48 kHz capture is refused |

### 4.3 Rejected
| Option | Why |
|---|---|
| opus 0.4 → opusic-sys 0.7.7 (C) | 3 crates / **9.98 MiB**, needs **cmake** (only inside VS BuildTools, not on PATH) |
| opus2 → libopus_sys 0.3.3 | 3.98 MiB, cmake |
| audiopus / audiopus_sys 0.2.2 | cmake, libopus 1.3, unmaintained since 2021 |
| unsafe-libopus 0.2.0 | c2rust libopus 1.3.1 (0.34 MiB) — fallback only |
| mousiki 0.2.1 | fixed-point decode gap |
| dav1d 0.11.1 / rav1e 0.8.1 | system libdav1d (meson + nasm) / too slow for realtime |
| vpx-encode / env-libvpx-sys | system libvpx (vcpkg) |
| songbird 0.6.1 | bot-oriented, 175 crates / **12.48 MiB**, duplicate tokio-tungstenite 0.26 — read `crypto.rs`/`udp_rx` only |
| dave (cacophony) | AGPL-3.0 |
Gaps: echo cancellation (official uses WebRTC AEC; no light Rust option verified → recommend headphones); Go Live system audio via WASAPI loopback (cpal loopback on an output device in 0.18 UNCONFIRMED; else `IAudioClient` + `AUDCLNT_STREAMFLAGS_LOOPBACK`; per-app `ActivateAudioInterfaceAsync` PROCESS_LOOPBACK, Win10 2004+).

---

## 5. Video, screen share and Go Live

### 5.1 Codec choice [UD-V]
Server picks per sender the lowest-priority codec every receiver can decode; else the sender's highest-priority encodable codec; no codecs → H264. Preferred AV1/H265, default **H264**, available VP8/VP9. Declare only H264 (PT 103 / RTX 104, encode+decode) → everyone sends you H264; downside: the call is held to H264. Payload-type rewriting per receiver UNCONFIRMED → also identify the codec by Session Description `video_codec` and your PT map.

### 5.2 Signalling
- **Op 12 (send)** before the first video RTP:
  ```json
  {"op":12,"d":{"audio_ssrc":A,"video_ssrc":V,"rtx_ssrc":V+1,
   "streams":[{"type":"video","rid":"100","quality":100,"active":true,"max_bitrate":2500000,
     "max_framerate":30,"max_resolution":{"type":"fixed","width":1280,"height":720},"ssrc":V,"rtx_ssrc":V+1}]}}
  ```
  SSRCs from Ready; stop = `video_ssrc:0, rtx_ssrc:0, streams:[]`; pause = `active:false` (keyframe at once on resume); camera also sets main op 4 `self_video:true`.
- **Op 12 (recv)** `{user_id, audio_ssrc, video_ssrc, streams[{ssrc, rtx_ssrc, rid, quality, active}]}` → cache SSRC → user/stream; drop unknown video SSRCs.
- **Op 15 Media Sink Wants (send)** `{"<video_ssrc>":100, "any":0?, "pixelCounts":{"<ssrc>":w*h}}` (0 off … 100) for visible tiles; default without wants UNCONFIRMED. Received op 15 = SFU asks you to reduce/pause layers.
- Simulcast: camera usually rid 100 + 50, Go Live one `screen` layer; minimal = one layer rid "100".

### 5.3 Video RTP (H264, RFC 6184)
- 90 kHz clock, shared timestamp per frame, **marker on the last packet**, payload ~1100–1200 bytes.
- Send: openh264 Annex-B → DAVE `encrypt(VIDEO, H264, frame)` → split on start codes → single NAL or **FU-A** (indicator `F|NRI|28`, header `S|E|0|type`) → transport encrypt. Extensions: ID 6 playout-delay (3 bytes, "Discord expects this on video"), ID 11 RID `"100"`, ID 7 video-content-type (1 = screen), ID 3 abs-send-time, ID 5 transport-cc (required? UNCONFIRMED).
- Receive: reorder per SSRC; RTX (PT 104 on `rtx_ssrc`) → first 2 bytes = original seq, map to primary; group by timestamp until marker; depacketise FU-A / STAP-A (24, 2-byte lengths); **rebuild Annex-B with 4-byte start codes** (DAVE needs it); DAVE decrypt; openh264 decode; YUV → RGBA; `TextureHandle::set`; `ctx.request_repaint()` per decoded frame.
- Feedback: NACK (PT 205 FMT 1) for gaps; **PLI** (PT 206 FMT 1) on decode failure or join; as sender keep ~1 s for RTX (own seq, payload = OSN + original), keyframe on PLI or every `keyframe_interval` ms; SR every 5 s.

### 5.4 Go Live (main gateway ops 18–22) [UD-GW, UD-V, UD-VR]
- Stream key: `guild:{guild_id}:{channel_id}:{owner_id}` · `call:{channel_id}:{owner_id}` · `test:{user_id}`.
- **Start** (in the channel, STREAM permission): op 18 `{"type":"guild"|"call","guild_id"?,"channel_id","preferred_region"?}` → STREAM_CREATE `{stream_key, rtc_server_id, rtc_channel_id, region, viewer_ids, paused}` + STREAM_SERVER_UPDATE `{token, stream_key, endpoint?}` (null = wait) → **second** voice WS: Identify `server_id=rtc_server_id`, `channel_id=rtc_channel_id`, same `session_id`, stream `token`, `video:true`, `streams:[{"type":"screen","rid":"100","quality":100}]`, `max_dave_protocol_version:1`; DAVE group `rtc_server_id - 1`; op 12 with the active stream; app audio as Opus with op 5 `speaking:2`. Your voice state gets `self_stream:true`.
- **Watch:** op 20 `{"stream_key"}` → STREAM_CREATE + STREAM_SERVER_UPDATE, or STREAM_DELETE `{stream_key, reason: user_requested|stream_ended|stream_full|unauthorized|safety_guild_rate_limited|parse_failed|invalid_channel, unavailable?}` → viewer WS (never send media), op 15 wants for the stream SSRC; parent voice connection stays up; separate SSRC space and DAVE group.
- Stop/leave op 19 `{stream_key}`; pause op 22 `{stream_key, paused}`; ping op 21; STREAM_UPDATE = viewers/paused.
- REST: `GET /streams/{key}/preview` → `{url}` (CONNECT); owner `POST /streams/{key}/preview {thumbnail:"data:image/jpeg;base64,…"}` (cadence UNCONFIRMED; every few minutes, INFERENCE); `PATCH /streams/{key}/stream {region}`; `POST /streams/{key}/notify`. Note the rate-limit `route_key` arm for `streams` (FEATURES_NOTES §0.2).
- Limits ~50 viewers per voice-channel stream (stage `max_stage_video_channel_users`); non-Nitro caps (720p/30) UNCONFIRMED → default 720p30 ~2.5 Mbit/s.

---

## 6. Phased plan

| Phase | Scope | New dependencies (compressed, vs current lock) | Ask the user? |
|---|---|---|---|
| **0** | voice-state store; sidebar voice users (mute/deaf/video/LIVE icons, speaking later); capability 4349 + CALL_*; incoming-call UI; ring / stop-ringing / ringable; op 43 channel status; voice panel shell (§4.4 of FEATURES_NOTES) | none | no |
| **A** | op 4 join/leave; voice WS v9 (heartbeat, seq_ack, resume, close codes); UDP + IP discovery + ping; rtpsize AEAD (both modes); RTP receive; Opus decode; jitter buffer, FEC/PLC, mixer; cpal output; speaking rings. **Test only in a Stage channel** | cpal, opus-rs, aes-gcm 0.10.3, chacha20poly1305 0.10.1: **22 crates / 1.36 MiB** standalone (only 3 crates / 0.76 MiB if C is added first) | yes — ask together with C |
| **C** (immediately after A) | davey session; ops 21–31; binary frames; encrypt/decrypt; privacy code + verification codes in UI. Unlocks DMs, group DMs, normal voice channels | davey: +87 crates / +4.96 MiB on top of A (**A+C = 109 crates / 6.32 MiB**) | **yes (> a few MB)** |
| **B** | mic capture 48 kHz; Opus encode; VAD + PTT; op 5 speaking; silence frames; audio extensions; self-mute/deafen; USE_VAD enforcement | none required; optional nnnoiseless 0.56 MiB, rubato 0.59 MiB | only for the optionals |
| **D** | receive camera + Go Live: op 12/15; H264 depacketiser; RTX/NACK/PLI; openh264 decode → egui texture; watch-stream flow (ops 20/19) | openh264: **5 crates / 1.20 MiB** + C++ build (MSVC) | yes (just over 1 MB, native build) |
| **E** | send camera + screen: windows-capture → BGRA→I420 → openh264 encode → DAVE → FU-A; RTX buffer; SR; Go Live ops 18/22 + second RTC connection; loopback audio with SOUNDSHARE | windows-capture: +1 crate / 55 KiB after davey (0.37 MiB alone); camera via `windows` 0.62: 0 | small |
| **All** | A–E | **115 crates / 7.57 MiB** | — |

Minimal path to web-client voice parity: **0 → A (Stage test) → C → B**; then D, E.

**Implementation checklist**
- [ ] Phase 0: `voice_states` map from READY_SUPPLEMENTAL + VOICE_STATE_UPDATE; CAPABILITIES 4349; CALL_* + ring UI; op 43 / status events
- [ ] voice module skeleton: main-gateway op 4 sender (debounced), voice WS task (tokio-tungstenite, text+binary), UDP task (`tokio::net::UdpSocket`)
- [ ] IP discovery, Select Protocol (Opus + H264), Session Description, AEAD encrypt/decrypt with unit tests from known vectors
- [ ] receive pipeline (jitter, FEC/PLC, mixer, cpal), speaking rings
- [ ] DAVE opcode state machine (§3.3) incl. op 25 buffering and failure path (op 31 + reinit)
- [ ] send pipeline: capture, VAD/PTT, Opus encode, DAVE, RTP + extensions, silence frames, op 5
- [ ] close-code handling + backoff; 4014/4021/4022 no reconnect
- [ ] UI: voice panel (Connected, channel link, disconnect, camera/screen buttons), mute/deafen (Ctrl+Shift+M/D), input/output device + volume + VAD threshold + PTT key settings, per-user volume/local mute, privacy code dialog
- [ ] (D/E) video receive/send and Go Live per §5

## 7. Behaviour rules (normal user client)
- Only on explicit clicks: join, ring, stream, watch. No auto-join.
- Debounce op 4 toggles (shared 120 events / 60 s budget).
- Never auto-retry 4014, 4021, 4022; other reconnects 1→30 s backoff, ≤5 tries, then leave.
- Ring/stop-ringing/preview REST through the existing `RateLimiter`, UI-triggered only; captcha → shown, never solved.
- Never transmit while self-muted, server-muted or suppressed; always send the 5 silence frames before going quiet.

**Not verified:** nothing was tested against live Discord. UNCONFIRMED: op 4 `flags` and deafen pairing, op 5 cadence for receive-only, mandatory audio/video RTP extensions, sink-wants defaults, PT rewriting, op 25 vs op 4 ordering, cpal capture resampling/loopback, H264 High-profile decode coverage, Go Live quality caps and preview cadence. Sizes are compressed downloads; build time and binary size were not measured.

**Risks:** DAVE (davey/openmls, ~106 crates) is unavoidable outside Stage channels. opus-rs (0.1.x) and davey (0.2.0) are young — pin exact versions. openh264 from source has no Cisco patent coverage.
