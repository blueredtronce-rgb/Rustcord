# Quests in Rustcord

What was built for "Discord Quests", and why it stops where it does. Sources: discord-userdoccers
(UD) `resources/quests.mdx`, `gateway/gateway-events.mdx`, `resources/store.mdx`,
`topics/captcha-handling.mdx`. Discord Help Center facts came from search snippets only.

Code: `src/quests.rs` (wire shapes, state, phases, `VideoProgress`), `src/api/quests.rs` (REST),
`src/gateway/quests.rs` (dispatches), `src/ui/quests.rs` (page, cards, modals, demo hook), the
"Quests" row in `ui/sidebar.rs` (`friends_row(.., quests: true)`), `Icon::{Quest, Orb, Gift}`.

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| D0 | **No automatic quest solver.** | Faking progress is fraud against Discord's quest program. |
| D1 | Video quests (`WATCH_VIDEO`) are the only quests Rustcord advances, and only with what the in-app player really played, capped by real play time (§3). | UD: video progress is plain REST; Discord says video quests work in the desktop app or the browser. Rustcord identifies as the web client (`api/props.rs`). |
| D2 | **No heartbeats.** Play, stream and activity quests (`PLAY_ON_DESKTOP`, `STREAM_ON_DESKTOP`, `PLAY_ACTIVITY`) are shown and can be accepted; the card says to finish them in the Discord desktop app. | UD, Send Quest Heartbeat: heartbeated tasks only complete from desktop clients; without Electron version info in the user agent the request fails with a 401. Sending an Electron UA would impersonate the official desktop app. Rustcord never does. |
| D3 | Mobile, console and achievement tasks: shown with what to do and where; nothing sent. | Completed on other devices or tracked by the game. |
| D4 | Accept and claim only on a click. A captcha answer becomes "… in the official app"; never solved or retried. | ARCHITECTURE hard rules. |
| D5 | No polling: `GET /quests/@me` and the Orbs balance when the Quests page opens (at most every 10 min); live changes from three gateway events. | Normal client behaviour; 0 % idle CPU. |
| D6 | Entry point: a "Quests" row under "Friends" in the DM list. | Rustcord has no Discover; next to Friends is where users look. |
| D7 | P1 (list, accept, claim, Orbs) works without a player. P2 (watching) uses the media area's `media::Player`; a video the player can't play says "Watch this video in the Discord app." | §6 |
| D8 | A started quest video plays on after its window closes, off the Quests page, minimized or in the tray, until it ends, the quest is done or the user stops it. One at a time; never started by Rustcord. | A quest video is minutes long; the user should not have to keep a window open. Same real playback, same sends. |

Not built on purpose: the claimed-quests history (`/quests/@me/claimed`), quest bars and embeds in
chat, ads endpoints (`/quests/decision`, `/quests/get-decisions`), dismissible content, console
start, the Orbs shop, reward art on cards.

## 1. Protocol

Base `https://discord.com/api/v9`, through `ApiClient` (rate limiter, auth). Bodies are JSON.

| Call | When | Request | Answer |
|---|---|---|---|
| `GET /quests/@me` | Quests page opened and connected; never fetched, or the last fetch ≥ 10 min ago; after a failure only on Retry | – | `{quests: Quest[], excluded_quests: [{id, replacement_id?}], quest_enrollment_blocked_until: ISO8601?}` |
| `GET /users/@me/virtual-currency/balance` | with the fetch above | – | `{balance: int}` |
| `POST /quests/{id}/enroll` | "Accept quest" | `{"location": 11}` (+ `traffic_metadata_sealed` when the quest has one) | quest user status |
| `POST /quests/{id}/video-progress` | while the quest video the user started plays in Rustcord (§3), window open or not | `{"timestamp": <int s>}` | quest user status |
| `POST /quests/{id}/claim-reward` | "Claim reward" | `{"platform": <int>, "location": 11}` (+ `traffic_metadata_sealed`) | `{claimed_at?, entitlements, errors: [{code, message}]}` |
| `GET /quests/{id}/reward-code` | "Show code" (reward type 1, claimed) | – | `{quest_id, code, platform, user_id, claimed_at, tier}` |

- `location` 11 = QUEST_HOME_DESKTOP. That Quest Home sends exactly 11 is **UNCONFIRMED**.
- `traffic_metadata_sealed`: passed through when present, never invented. Whether enroll fails
  without it is **UNCONFIRMED**.
- `platform`: 0 cross-platform, 1 Xbox, 2 PlayStation, 3 Switch, 4 PC. `platforms` empty or one
  entry → that one (0 when empty); several → the claim modal asks. Sending 0 for Orbs /
  collectible rewards is **UNCONFIRMED**.
- Video progress cadence: UD "every 10-15 seconds a video plays". The server's own slack for
  `timestamp` ahead of enrolment time is **UNCONFIRMED**; Rustcord never reports more than was
  played.
- Captcha: a 400 with `captcha_key` (`api::CAPTCHA_MSG`) becomes "Discord wants a captcha to
  claim this. Claim it in the official app." (accept: "… to accept this. Accept it in the
  official app."). How often claims ask for one is **UNCONFIRMED**.
- **A 401 on a quest route never logs out.** `api/quests.rs` `quest_call` holds the client's
  token-rotation guard, so a 401 neither reaches the session as `Unauthorized` nor arms the
  fail-fast lock; it reads "Discord didn't accept this from Rustcord. Finish this quest in the
  official app." A real token failure is caught by the next non-quest request.
- Per-route rate limits: **UNCONFIRMED**; the limiter handles buckets and 429s. Rustcord sends at
  most 2 GETs per 10 min, one request per click, and one video POST per 10 s of real play.

Never called: `POST /quests/{id}/heartbeat`, `POST /activities`, `/console/*`, `/preview/*`,
`/quests/decision`, `/quests/get-decisions`, `/quests/creatives/*`, `/dismissible-content/*`,
`/virtual-currency/skus/*/redeem`. No heartbeat code exists.

### Gateway

| Event | `d` | Rustcord |
|---|---|---|
| `QUESTS_USER_STATUS_UPDATE` | `{user_status}` | replaces that quest's status |
| `QUESTS_USER_COMPLETION_UPDATE` | `{quest_enrollment_blocked_until?}` | the blocked banner |
| `VIRTUAL_CURRENCY_BALANCE_UPDATE` | `{balance}` | the Orbs balance |

Whether a web-identified session receives these for progress made in the desktop app is
**UNCONFIRMED**; the page's next fetch shows it either way.

### Shapes read (everything defaulted, unknown fields ignored, a quest that fails to parse is skipped)

```
Quest            { id, config, user_status?, preview, traffic_metadata_sealed? }
QuestConfig      { expires_at, features[], application {id, name}, assets {hero, game_tile},
                   colors {primary, secondary}, messages {quest_name, game_title, game_publisher},
                   task_config_v2? {join_operator "and"|"or", tasks {"<TYPE>": {target, assets {video, video_low_res}, messages {video_title, task_description}}}},
                   rewards_config {rewards [{type, messages {name}, orb_quantity?}], rewards_expire_at?, platforms[]} }
QuestUserStatus  { quest_id, enrolled_at?, completed_at?, claimed_at?, progress {"<TYPE>": {value}} }
```

Features: 21 DO_NOT_DISPLAY (hidden), 18 VIDEO_QUEST_FORCE_HLS_VIDEO (no in-app video). No
`task_config_v2` (v1 is discontinued): "Open this quest in the Discord app."

Assets (fixed after the Quests page showed only fallbacks on a real account; UD's `/assets/quests/{id}/{name}`
was wrong). Source: the web client's asset function in discord.com's public JS bundle (read
anonymously, no account): a name with a `/` -> `https://cdn.discordapp.com/{name}`; a bare name ->
`https://cdn.discordapp.com/quests/{id}/{name}`, and for the game tile (and logotype) with a
`dark/` or `light/` folder in between; `http(s)://` and `blob:` values pass through. The client
shows `hero_video` when present and the still `hero` otherwise; Rustcord only paints the still.
Rustcord (`cdn::quest_asset_url` / `quest_tile_url`) takes the same three forms, absolute only on
Discord's media hosts, no `..`. Not seen on a live response: the exact value shape of the real
`hero` / `game_tile` (bare or `quests/..`) and whether the tile folder exists for every quest, so
a tile that fails to load keeps the letter fallback (a failed load is retried at most every 60 s
and only while it is drawn). Only `.png .jpg .jpeg .webp .gif` load as images; other heroes fall
back to the quest's colour gradient. The video: `video_low_res`, else `video`, `.mp4` only
(`cdn::quest_video_url`); its codec (expected H.264 + AAC) is **UNCONFIRMED**. Never HLS.

## 2. Tasks and what Rustcord does

| Task | Card line | Rustcord |
|---|---|---|
| `WATCH_VIDEO` (playable) | "Watch the video for {3 minutes}." | the video modal (P2, §3) |
| `WATCH_VIDEO` (HLS-only, no MP4, no player, player failed) | "Watch this video in the Discord app." | nothing sent |
| `PLAY_ON_DESKTOP` | "Play {game} for {15 minutes} in the Discord desktop app." + "Rustcord can't track play time for quests. Progress from the desktop app shows here." | accept only (D2) |
| `STREAM_ON_DESKTOP` | "Stream {game} to a friend for {…} in the Discord desktop app." | accept only |
| `PLAY_ACTIVITY` | "Play the activity for {…} in the Discord app." | accept only |
| `WATCH_VIDEO_ON_MOBILE` | "Watch the video in the Discord mobile app." | accept only |
| `PLAY_ON_XBOX` / `_PLAYSTATION` | "Play {game} on Xbox or PlayStation. Start it in the Discord app." | accept only |
| `ACHIEVEMENT_*` | `task_description`, else "Reach the goal in {game}." | accept only |

Card task ("or" quests list several): the first present of WATCH_VIDEO, PLAY_ON_DESKTOP,
STREAM_ON_DESKTOP, PLAY_ACTIVITY, ACHIEVEMENT_IN_GAME, ACHIEVEMENT_IN_ACTIVITY, PLAY_ON_XBOX,
PLAY_ON_PLAYSTATION, WATCH_VIDEO_ON_MOBILE, else the first key. Progress: "or" the furthest task,
"and" the least done one.

Phases (`Quest::phase`): Hidden (preview, feature 21, past `expires_at`), Ready to claim
(completed, not claimed, before `rewards_expire_at`, else before `expires_at`), Claimed, In
progress (enrolled), Available. Excluded quests are dropped on load.

`RUSTCORD_FAKE_GAME` never touches quests.

## 3. The video progress rule (`quests::VideoProgress`)

The only progress Rustcord reports. Driven by `ui::quests::logic` on every app pass (drawn or
not, also minimized or in the tray): `tick(pos, player_played_ms, now) -> Option<timestamp>`,
`answered(ok)`, `close()`. While the session plays, `logic` asks for a repaint after 1 s, so it
ticks at least once a second; with the modal open it ticks at frame rate. Foreground and
background are the same code, the same request, the same 10 s spacing.

- `timestamp` ≤ the picture's position (`min(position_ms, start + played_ms)`), ≤ server value +
  real seconds in which the player's own `played_ms` grew, ≤ `target`; strictly increasing; ≥ 10 s
  apart except the final one (the target).
- The wall time since the last tick counts only if the player's `played_ms` grew since then, and
  at most 2 s per tick (`MAX_TICK_DT`, against suspend and hangs). So nothing can exceed what the
  player really played, at normal speed; a pause, a stall or a seek credits nothing. A new player
  starts its counter at 0, which credits nothing and resets the baseline.
- This replaced "wall time at drawn frames, gaps capped at 250 ms": at 1 Hz that rule would drop
  the final partial second and every second whose tick fell on buffering, and a quest whose target
  equals the video length would end 1-2 s short. `same_sends_at_30_fps_and_1_hz` pins that both
  rates send the same timestamps.
- One request in flight. A failed send stops the session for good ("Progress stopped saving.
  Try the video again later."); no retry.
- Only while enrolled and not completed, connected, not offline, never in the demo. While not
  connected no ticks are sent and no time is credited; the player goes on from its buffered
  blocks, and once it fails with `Fail::Network` the card says "The video stopped: no connection.
  Continue watching when you're back online." (the quest stays playable, unlike other failures).
- **When the session ends** (`keeps_playing`): the user presses Stop (modal or panel); the user
  paused and closed the modal; the player ended or freed with no send in flight; it failed; a send
  failed; the quest is done (only while the modal is closed); the quest left the list; logout
  (nothing sent); quit (nothing sent). Ending calls `close()`: the last whole seconds go out once,
  except for the demo, logout and quit, which lose at most the last 10 s (and a send still in
  flight when it ends is lost: `take` refuses).
- Starting another quest's video stops the running one first (its last seconds are sent). "Open
  video" on the quest that plays only reopens its modal.

Unit-tested in `quests.rs` `video_progress_reports_only_real_play`, `same_sends_at_30_fps_and_1_hz`
and in `ui/quests.rs` `keeps_playing_rules`, `one_quest_video_at_a_time`.

## 4. UI

- DM list: "Quests" (`Icon::Quest`) under "Friends", badge = rewards ready to claim.
- Page (`ui/quests.rs` `show`): header (Quest 20, "Quests", right "{n} Orbs" with `Icon::Orb`,
  tooltip "Your Orbs balance"); the blocked banner ("You can accept new quests again on {date}.");
  sections Ready to claim, In progress, Available, Claimed (empty ones left out); cards in 1-3
  columns 300-420 wide, equal height per row; footer "Rustcord counts quest progress only while you
  watch a quest video here." and a "Quest privacy settings" link (Settings > Data & privacy).
- States: loading (three card outlines), not connected ("Quests load when you're connected."),
  failed ("Couldn't load quests", the error, Retry), none ("No quests right now").
- Card: hero (image cover-fit, else the quest colours as a gradient), game tile 40 over its
  bottom edge, quest name, "{game} by {publisher}", reward ("{n} Orbs" or its name), "Ends" /
  "Claim by" date, the task line, a progress bar ("1 of 3 min"; done: Check + "Completed"; the
  playing quest shows the larger of the server's value and the session's seconds), the
  card notice (errors), and one secondary button: Accept quest (disabled while blocked) / Watch
  video / Continue watching / Open video (its video plays) / Claim reward / Show code. A shown code sits in a well with Copy;
  it lives only in memory (`auth::Secret`).
- `Modal::QuestVideo`: title (video title, else quest name), the player frame, "1:12 / 3:00", a
  display-only bar with the time left, "Progress counts while the video plays, also after you
  close this.", Close, "Stop video" (ends the session) and, once done, "Claim reward". No seeking
  control. Closing it (Close, ×, Esc, scrim) only hides it: the video plays on, unless the user
  paused it (then the session ends and sends its last seconds).
- User panel, "Quest video" section on top of the voice section (visible on every page while a
  session exists, 56 px + hairline): "Quest video" in `positive_text`, the quest name under it (a
  link: opens the modal; tooltip "Open the video"), Mute / Unmute (only if the video has sound;
  shared with the modal's button) and Stop video (`Icon::Close`), and a 3 px live progress bar.
  No DM-list or title-bar glyph: the panel is enough.
- Sound: a hidden session keeps the user's volume and the Voice & Video output; the panel's mute
  is the way to silence it. Chat videos stay independent (the viewer pauses only its own players),
  so two sounds can overlap.
- The updater (`update::busy`) counts a playing quest video while Rustcord waits in the tray, so
  it does not restart mid-video.
- `Modal::QuestClaim` (several platforms): "Where will you use this reward?", radio rows (PC,
  Xbox, PlayStation, Nintendo Switch, All platforms), Cancel / Claim reward.

## 5. Test hooks

`RUSTCORD_OPEN=quests` (DM home, Quests page with `ui::quests::DEMO_JSON`: Orbs 1240, ready to
claim with a captcha notice, video in progress 40 %, desktop play 60 %, two available, claimed
with the code "RCDEMO-1234-ABCD"; dates relative to today). `quests:blocked` adds the banner,
`quests:empty`, `quests:error`, `quests:loading` the states, `quests:watch` the video modal (a
still frame at 1:12 of 3:00, nothing sent), `quests:watch-play` (the fixture clip plays in it, muted,
looped), `quests:bg` (the same without the modal: the panel section and the card's "Open video"),
`quests:complete` (the Friends page; quest 101 completes, so the next pass shows the "Quest
complete" notification, with 104 counted as told, and writes `quests-notified.txt` in the home's
cache; screenshot runs have no tray, so no balloon), `quests:claim` the claim modal. None sends a request.
`RUSTCORD_NAV_DEMO=icons` shows `Quest`, `Orb`, `Gift`.

## 6. P2: the quest video in the media player

`ui/quests.rs` owns one `crate::media::Player` per watch session (`Watch`), opened on "Watch
video" / "Continue watching" for `Quest::video_url()`, seeked to `Quest::video_value()` and
playing; `close_video` drops it (the worker stops). No `no_seek_ahead`: it would clamp that resume
seek to 0, and the modal has no seek control anyway.

- The session (`Watch`) is one slot, `QuestsUi::watch`; it outlives the modal (§3). The player is
  opened with `media::Opts::background`: it does not pause when not drawn and its `played_ms`
  counts while Playing, drawn or not. Pictures not drawn for 1 s are decoded (H.264 references)
  but neither converted nor published, so no UI wakes at 30 fps. Shortcut: audio-only playback
  with a keyframe resync when drawn again is not built (`shortcut:` in `media/mod.rs`).
- Every paint of the modal: `drawn()`, `set_target()` (frame size in physical px), the frame from
  `media::frames(ctx).latest(key)` letterboxed into the 16:9 rect. `logic` drops the texture
  while the modal is not shown (GPU memory).
- `logic` computes `pos` = `min(position_ms, start + played_ms)` (the picture, never past what the
  player itself counted as played) and `playing` = state Playing and not buffering, and hands
  `played_ms` to `VideoProgress::tick`.
- `State::Failed(Fail::Network)`: the card notice "The video stopped: no connection. …"; Play in
  the modal opens a new player. Any other `Failed` puts the quest into `QuestState::no_video`
  (the card falls back to "Watch this video in the Discord app.") and the modal says "This video
  can't play in Rustcord. Watch it in the Discord app."
- `State::Freed` (ended and not drawn for 30 s, or paused by the user and hidden): with the modal
  open the handle is dropped and Play opens a new player at the seconds already saved, and `start`
  moves there (its `played_ms` counts from 0 again).
- Minimized or hidden in the tray: eframe runs only `App::logic`; `quests::logic` is one of its
  calls, so the session keeps ticking (§3).

## 7. Risks

1. Account risk is Discord's call: any third-party client sending quest calls can look unusual.
   Rustcord sends only user-started actions and real playback, no heartbeat, no identity change.
2. Play quests can't progress in Rustcord (D2); the card says where to finish them.
3. If Discord gates `video-progress` like heartbeats later, the first send fails and the session
   stops with the notice; no logout (§1).
4. Asset URL form, video codec, `location`, `traffic_metadata_sealed`: UNCONFIRMED, each with a
   safe fallback (gradient hero, "Watch this video in the Discord app", pass-through).
5. Gateway events may not reach a web-identified session: the next page open after 10 min shows
   the new state.
6. Background audio and network: a started video keeps playing, with sound at the user's volume,
   until it ends or the user stops it. The panel's Stop and Mute are always one click away.
7. The progress rule changed from "wall time at drawn frames" to "wall time in which the player's
   counter grew, ≤ 2 s per tick" (§3). It never exceeds the player's real `played_ms`, but it is a
   fraud-sensitive rule: review it as one.

## 8. Quest complete notification

When a quest's phase becomes Claimable (completed, not claimed, reward not expired, not hidden)
and its id is not in the notified set, Rustcord notifies once: title "Quest complete", body
"{quest name}: claim your reward." (`ui::quests::completed`, called from `RustcordApp::logic`).

- The set lives in `<cache dir>/quests-notified.txt` (one decimal id per line, bad lines skipped),
  pruned to the listed quests on each check, removed by `cache::clear_all` on logout.
- First run (file missing: after this update or a new login) it only records the quests that
  already wait, so there is no burst, and it always writes the file.
- It checks only after quest events (`QuestState::changed`) and only while `Load::Loaded`.
- No notification (the id is still marked, so nobody is told twice; the card and the Quests badge
  show the reward): Do Not Disturb, desktop notifications off, or the user is looking at it
  (window focused, no settings layer, and the Quests page or a quest video modal shown). In a game
  the overlay shows it, as with messages.
- A click on it (Windows `NIN_BALLOONUSERCLICK`, Linux ActionInvoked, `TrayAction::Notification`)
  opens the Quests page, only if the last notification shown was a quest's (any message
  notification clears that flag). macOS reports a click as Show: it opens the window only. A click
  on the tray icon never opens the page.
- Whether the answer to `video-progress` carries `completed_at` is **UNCONFIRMED**. If it does not,
  the gateway status update or the next fetch triggers the notification instead.
- A quest notification merged by the 3 s notification rate limit (`Coalescer`) reads "N new
  messages".

Appendix, heartbeats (not built; recorded so nobody re-researches it): `POST
/quests/{id}/heartbeat {"application_id", "terminal"}` while the detected game is one of the
task's applications; the server credits ≤ 120 s per beat; the official cadence (~60 s) is
UNCONFIRMED. It needs the desktop app's identity, which Rustcord does not assume.
