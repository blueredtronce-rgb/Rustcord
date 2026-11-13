# Video design (camera, screen share, Go Live; phases D/E of VOICE_NOTES)

Protocol facts live in `docs/VOICE_NOTES.md` §3 (DAVE) and §5 (video, Go Live), which are
authoritative. The audio engine is in `docs/VOICE_DESIGN.md`. This page fixes the split of the
video code so four engineers and the UI can work in parallel.

## Status
Backend wired end to end and tested offline; **nothing has run against live Discord yet**. Camera
(Media Foundation), screen share (windows-capture), watching (ops 18/22/20/19, Go Live
connections), op 12/15, video RTP out/in, NACK/RTX/PLI and decoding into `VideoFrames` all work
in tests. The offline self-test is `voice::transport::video::tests::video_loopback_recovers_a_lost_packet_by_rtx`:
synthetic picture → openh264 → DAVE (off) → packetize → seal → UDP → fake SFU that loses a
keyframe packet → NACK over sealed RTCP → RTX → depacketize → decode → frame slot + size event.
The video UI is built (`ui/call.rs` call view and tiles, `ui/screen_share.rs` picker, camera /
screen buttons in the voice panel, camera picker and preview in Voice & Video settings; offline
states via `RUSTCORD_CALL_DEMO`, docs/ARCHITECTURE.md). Live in calls today: Identify sends `video: true` with one `streams` entry, Select
Protocol declares H264 (PT 103, RTX 104); unwanted video packets are dropped before decryption.
Idle cost (release build, `RUSTCORD_OFFLINE=1` on a copy of `tests/fixtures/home`, 10 s after
start, 2026-11-12): 91 MB private / 99–117 MB working set, 17 threads, no `rustcord-video-*`
thread.

## Data flow
```
UI ──Command::Video(VideoCommand)──► backend::run ──VoiceManager::video──► voice task (voice/mod.rs)
voice task: Manager ──with_video(Ctx)──► video::stream::Video (camera / screen share / watch state)
  │ op 4 self_video = Video::camera_on()  (Manager::op4; a change re-sends op 4)
  │ GatewayCommand::Stream(StreamOp) ──► main gateway: ops 18/19/20/22 (in order, send limiter)
  │ ◄── STREAM_CREATE / STREAM_SERVER_UPDATE / STREAM_DELETE (gateway → voice task only: token)
  │ voice connection:  Video::link_for_call() → ConnectInfo::video {Camera, VideoRx, ctl}
  │ Go Live connection: second transport::run, ConnectInfo {server_id = rtc_server_id,
  │   channel_id = rtc_channel_id, same session_id, stream token, dave_group = rtc_server_id - 1,
  │   video {Screen, VideoRx(stream_key), ctl}}, MediaLink {Ctx::playback, idle captured}
SEND  "rustcord-video-tx" thread (capture.rs): Media Foundation / windows-capture → drop frames
      beyond fps → codec::fit_within + to_i420 → codec::Encoder → EncodedFrame
      ──mpsc ENCODED_QUEUE──► VideoCtl::Send(VideoSend) ──► transport video send task
      (transport/video.rs): Dave::encrypt_h264 (None = drop) → rtp_h264::packetize →
      write_video_header (PT 103, 90 kHz, marker, extensions) → Crypt::seal → UDP; RtxHistory 1 s.
      RTCP in → parse_feedback: NACK → resend as RTX (PT 104, rtx SSRC, OSN + payload);
      PLI → VideoSend::keyframe.
      The send task sends nothing but whole keyframes after a dropped frame (DAVE not ready),
      and turns PLIs into at most one keyframe request per 500 ms.
RECV  UDP rx task → parse_rtp → PT ≠ 120 → transport::video::on_packet → VideoRx::wants(ssrc)?
      no → drop (before decrypting) ; yes → Crypt::open → VideoRx::push → bounded queue
      "rustcord-video-rx" thread (receive.rs, one per connection while anything is wanted):
      unwrap_rtx → Depacketizer → Dave::decrypt_video(user) → codec::Decoder::decode →
      FrameWriter::write (RGBA ≤ 1080p) → VideoFrames → ctx.request_repaint()
      every 20 ms: take_nacks / take_keyframe_request (only while packets flow) →
      rtp_h264::nack / pli → the connection's send task fills in my SSRC, seals, sends
      RTCP in (8-byte clear header) → parse_feedback → VideoRx::feedback → send task (RTX, PLI)
      op 12 in → parse_op12 → VideoRx::on_remote → VideoEvent::RemoteVideo / RemoteVideoEnded
      (on a Go Live connection only the stream owner's op 12 counts, and stream::Video reports)
      wanted set changed → op 15 (op15_payload: 100 for wanted SSRCs, "any": 0; only after
      Ready, only when the set differs from the last op 15)
END   stream::Video calls VideoRx::end when it lets a connection go (call end, Go Live dropped):
      every camera it announced gets RemoteVideoEnded, then it stays silent while closing.
UI    drain_events: BackendEvent::Voice(VoiceEvent::Video(ev)) → AppState::apply_voice →
      state.voice.video (VideoStatus)   — already wired, no UI change needed
      paint: BackendHandle::video.latest(&key) → (generation, Arc<ColorImage>) → texture.set
```
Frames never travel through the event channel. Tokens (voice and stream) only travel gateway →
voice task → transport.

## Tasks and threads
| Where | Kind | Lives | Owns |
|---|---|---|---|
| `stream::Video` | inside the voice task (no task of its own) | backend loop | camera/share/watch state, Go Live connection handles, the voice connection's `ctl` sender + `VideoRx` |
| Go Live connection | 1 tokio task per stream (`transport::run`) | sharing / watching | its WebSocket, UDP, `Dave`, `VideoConn` |
| video send task | tokio task inside `transport::run` (`start_media`) | the connection | RTP seq/timestamp, `RtxHistory`, sealing, RTCP out (also on receive-only connections) |
| "rustcord-video-tx" | std thread per running capture | camera on / sharing | device or capture session, `Encoder`, scratch `YUVBuffer`, preview writer |
| "rustcord-video-rx" | std thread per connection | while ≥ 1 video is wanted | `Depacketizer`s, `Decoder`s, `FrameWriter`s |
| UI | egui thread | — | textures (one per shown tile) |
| listing | `spawn_blocking` per `ListCameras` / `ListScreenSources` | one call | — |

Stop: dropping `capture::Sending` releases the device and ends the tx thread; the transport then
sees `VideoSend::frames` close. Dropping a Go Live connection's stop sender ends it like a call.
No video on = no video thread, no capture handle, no per-packet allocation.

## Buffers (bounded)
| Buffer | Size |
|---|---|
| capture → transport | `ENCODED_QUEUE` = 2 encoded frames; full = drop + next frame is a keyframe |
| UDP task → rx thread | `receive::RTP_QUEUE` = 512 packets per connection; full = drop (NACK later) |
| reorder per SSRC | `rtp_h264::REORDER_PACKETS` = 256 packets + one frame being assembled |
| sender RTX history | `rtp_h264::RTX_HISTORY` = 1 s (≤ 0.5 MB at 4 Mbit/s) |
| frame slots | `MAX_FRAME_SLOTS` = 6 (4 remote + my camera preview + my share preview), each one RGBA frame ≤ `MAX_FRAME_SIZE` 1920×1080 (8.3 MB); decoders downscale bigger pictures |
| decoders | one per wanted video (openh264 keeps its reference frames) |
| encoder scratch | one `YUVBuffer` of the send size |
Worst case ~50 MB of frames (4 remote 1080p videos + both previews; the second preview adds 8.3 MB)
+ decoders; typical 720p calls ~15 MB.

## Send limits
`SendLimits::CAMERA` = 1280×720, 30 fps, 2.5 Mbit/s. `SendLimits::screen(fps, max_height)` =
1..=30 fps, 360..=1080 lines in a 16:9 box, 2.5 Mbit/s up to 720p, 4 Mbit/s above. The op 12
`max_*` fields carry these. Non-Nitro caps are UNCONFIRMED (VOICE_NOTES §5.4).

## Contract (do not change signatures without the contract owner)
- `video/mod.rs`: `VideoCommand`, `VideoEvent`, `ScreenSource`, `MonitorInfo`, `WindowInfo`,
  `CameraInfo`, `VideoKey`, `SendLimits`, `PixelFormat`, `RawFrame`, `EncodedFrame`, `StreamKind`,
  `VideoSend`, `VideoCtl`, `VideoLink`, `RemoteVideo`, `op12_payload`, `parse_op12`,
  `op15_payload`, `VideoFrames::{new, latest, writer, remove}`, `FrameWriter::write`, constants.
- `voice/mod.rs`: `VoiceEvent::Video`, `VoiceManager::{spawn(sink, frames), video}`,
  `Msg::{Video, Stream}`, `Call::playback`, `Manager::with_video` (builds `stream::Ctx`).
- `transport.rs`: `ConnectInfo::{dave_group, video}`; the hook call sites into
  `transport/video.rs` (`VideoConn::{new, kind, on_ready, on_op12, on_client_disconnect, on_ctl,
  start_media, rx}`, `next_ctl`, `on_packet`).
- `dave.rs`: `Dave::{encrypt_h264, decrypt_video}` (davey `encrypt(VIDEO, H264, ..)` /
  `decrypt(user, VIDEO, ..)`; same rules as audio, minus the Opus silence exception).
- `gateway`: `GatewayCommand::Stream(StreamOp)`, `GatewayEvent::{StreamCreate,
  StreamServerUpdate, StreamDelete}` (routed to the voice task only, never to the UI).
- `backend.rs`: `Command::Video(VideoCommand)`, `BackendHandle::video: Arc<VideoFrames>`.
- `state`: `VoiceStatus::video: state::video::VideoStatus`.
- Engineers' files: the pub items as stubbed (signatures + doc comments are the spec), plus
  `VideoRx::{route_feedback, feedback, end}` (integrator: RTCP feedback routing, connection end).

## File ownership
| Owner | Files |
|---|---|
| contract owner | `video/mod.rs`, `state/video.rs`, `gateway/stream.rs` + its wiring in `gateway/conn.rs`, `model.rs` Stream events, `backend.rs` video arms, `voice/mod.rs` video hooks, the hook call sites in `transport.rs`, `dave.rs` H264 methods, this page, `Cargo.toml` |
| A codec | `video/codec.rs` |
| B video RTP | `video/rtp_h264.rs`, `video/receive.rs`, `voice/transport/video.rs` |
| C capture | `video/capture.rs` (+ `windows` features in `Cargo.toml`) |
| D Go Live / signalling | `video/stream.rs` |
Crates (approved, pinned): openh264 =0.9.10 (C++ via `cc` + MSVC; nasm optional, not
installed), windows-capture =2.0.1 (Windows only), `windows` 0.62.2 features
`Win32_Foundation`, `Win32_Media_MediaFoundation`, `Win32_System_Com` (add more features as
needed, never another version). Nothing else without asking.

The placeholder allows and the `PENDING` text are gone; no video item keeps an `allow(dead_code)`.

## What "done" means per engineer
**A codec.** `Encoder` (openh264 `EncoderConfig`: bitrate, `max_frame_rate`, camera vs screen
usage type, SPS/PPS with every IDR, IDR on `keyframe` and every few seconds; Constrained
Baseline, no B-frames) → `EncodedFrame` with 4-byte start codes. `Decoder` → `write_rgba8`
into `FrameWriter::write`, downscaled into `MAX_FRAME_SIZE`; `Err` on undecodable input.
`fit_within` (even sizes, never upscale), `to_i420` for BGRA and NV12 with stride, scaling while
converting. Unit tests: round trip encode → decode of a synthetic picture, sizes, colours.

**B video RTP.** RFC 6184 single NAL + FU-A out; single NAL + STAP-A + FU-A in; u16
wraparound reordering; frame assembly by timestamp until the marker; RTX unwrap/wrap; NACK
(RTPFB 205/1) and PLI (PSFB 206/1) build + parse (compound RTCP); `RtxHistory`; video header
extensions (VOICE_NOTES §5.3: ID 6 playout delay, ID 11 RID "100", ID 7 content type for
screen). `VideoRx`: wanted set (≤ 4 remote), SSRC/RTX map from op 12 (on the voice connection)
or the owner (Go Live), lazy rx thread, `RemoteVideo`/`RemoteVideoEnded` events (size update on
the first decoded frame and on size changes), slot freed on unwant. `transport/video.rs`: Ready
SSRCs, op 12 send (`op12_payload`) on `VideoCtl::Send` / `StopSending`, op 15 on `Wants` and
after op 12s, the video send task (pace by `EncodedFrame::captured`, 90 kHz timestamps,
`encrypt_h264` → drop on None), RTCP in/out, `wants()` before `Crypt::open`. PLI at most every
~500 ms; keyframe request at the start of each wanted video. Unit tests for every packet format
and for loss/reorder/RTX recovery.

**C capture.** `list_cameras` (MF `MFEnumDeviceSources`, friendly name + symbolic link),
`list_screen_sources` (`Monitor::enumerate`, `Window::enumerate`, skip invisible/untitled/our
own windows), `start`: thread opens the source (MF `IMFSourceReader` NV12/RGB32 with video
processing; windows-capture with cursor on, border off where the OS allows), drops frames beyond
`fps`, converts + encodes, `try_send`s into `ENCODED_QUEUE` (full → drop, set keyframe), honours
`VideoSend::keyframe`, writes my preview while one is set; `failed` on device loss / window
closed. `Sending` drop stops everything within one frame. `#[cfg(windows)]` with an error on
other targets. Unit tests: pacing and the non-Windows path; device code is tried by hand.

**D Go Live / signalling.** `Video` state machine: StartCamera (needs `ctx.call`) →
`capture::start` → `VideoCtl::Send` on the voice connection, `camera_on` true (op 4
`self_video`), `CameraStarted`; StopCamera / call end / `CaptureFailed` → drop `Sending`,
`StopSending`, `CameraStopped`. StartScreenShare (`audio: true` → Error) → op 18 → STREAM_CREATE
+ STREAM_SERVER_UPDATE (any order, endpoint None = wait) → Go Live `transport::run` → on
connected `VideoCtl::Send` (Screen) → `ScreenShareStarted{key}`; StopScreenShare → op 19, drop.
WatchStream → op 20 → viewer connection (no media sent) → `RemoteVideo{Stream(key)}`;
StopWatching → op 19 + `RemoteVideoEnded`; STREAM_DELETE → end it, `Error` with a readable
reason (stream_full, unauthorized …) unless user_requested. SetVideoVisible → the right
`VideoRx::set_wanted` → `VideoCtl::Wants`; own keys → `Sending::set_preview`. Go Live ends
(`StreamMsg::Ended`) → clean up, never reconnect on its own. Tests with the manager driven by
hand like `voice/mod.rs` does.

## UI hook-up guide
- **Events** arrive inside `BackendEvent::Voice(VoiceEvent::Video(..))`, which `drain_events`
  already passes to `AppState::apply_voice`; read `state.voice.video`: `camera_on`,
  `screen_share` (my stream key), `cameras`, `monitors`, `windows`, `remote` (shown-able videos →
  size). Errors come back as toast text from `apply_voice`. No new `BackendEvent` and no new
  `drain_events` match arm are needed.
- **Commands** (`Command::Video(..)`, only on clicks; the backend refuses them outside a
  connected call): camera button → `StartCamera { device }` / `StopCamera`; device picker →
  `ListCameras` then `Cameras`; screen button → `ListScreenSources`, the user picks a
  monitor/window from `monitors` / `windows` (never pick one for them) → `StartScreenShare {
  source, fps: 30, max_height: 720 or 1080, audio: false }` / `StopScreenShare`; picking another
  source while sharing just sends `StartScreenShare` again; "Watch" on a user with
  `VoiceState::self_stream` → `WatchStream { stream_key: stream::stream_key(guild_id,
  channel_id, user_id) }` / `StopWatching`.
- **Stop must be obvious**: while `camera_on` or `sharing()` (live, or Go Live clicked and still
  connecting: `share_starting`) the voice panel shows a highlighted Stop for each (the camera light / capture border is the OS's cue, the panel is
  ours). Show the camera/screen buttons only while `state.voice.connected`.
- **Visibility**: send `SetVideoVisible { key, visible: true }` when a tile becomes visible and
  `false` when it scrolls away, is collapsed, the window is minimized, or the panel closes. At
  most 4 remote videos visible at once (more tiles show the avatar). My own preview:
  `User(me)` for the camera, `Stream(my key)` for the share; without it no preview frame is
  made. Nothing is decoded (and no video thread runs) for a key that is not visible, so a
  `remote` entry without `SetVideoVisible` costs nothing. A watched stream appears in `remote`
  as `Stream(key)` (0×0) once its connection is up; mark it visible to start decoding.
- **Frames**: `BackendHandle::video` (`Arc<VideoFrames>`, reachable as `self.backend.video` in
  `RustcordApp`; pass it into `UiCx` as you see fit). Per shown tile and paint:
  ```rust
  if let Some((generation, image)) = video.latest(&key)
      && tile.generation != generation
  {
      tile.generation = generation;
      match &mut tile.texture {
          Some(t) => t.set(image, egui::TextureOptions::LINEAR), // takes the Arc, no pixel copy
          None => tile.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
      }
  }
  ```
  Draw the texture letterboxed into the tile (`image.size` is the frame size). Writers call
  `request_repaint` per new frame; never call it yourself for video. Drop the tile's texture on
  `RemoteVideoEnded`, on hide, and when `state.voice.channel_id` becomes None.
- **Indicators**: camera/LIVE icons in the voice list already come from `VoiceState::self_video`
  / `self_stream`.

## Rules
- Capture only between a user's Start* and the matching Stop, the call's end or a capture error;
  the camera light / capture border is the user's cue, and the voice panel must show a Stop.
- DAVE: once the connection's DAVE protocol version is > 0, a frame `encrypt_h264` does not
  encrypt is dropped, never sent in clear. Received frames that do not decrypt are dropped.
- Decode only wanted videos; unwanted packets cost a header parse and a map lookup.
- Secrets (voice/stream tokens, `secret_key`, MLS keys) never in logs, Debug output or errors.
- Every non-trivial pure function gets a unit test. Shared target dir only:
  `$env:CARGO_TARGET_DIR="C:\rc-target"; $env:CARGO_INCREMENTAL="0"`.

## UNCONFIRMED (VOICE_NOTES §5)
`video: true` needed to receive; op 15 `any` semantics and the default without wants; mandatory
video RTP extensions (transport-cc ID 5); PT rewriting per receiver; Go Live quality caps and
preview cadence (`POST /streams/{key}/preview` is not implemented); Go Live application audio
(not supported: `audio: true` is refused); STREAM_UPDATE (viewers, paused) is not handled.
