# Chat essentials design (uploads, typing, presence, pins, search, @members, GIFs, message extras)

Protocol facts: `docs/FEATURES_NOTES.md` Part 1 and §0 (authoritative). This page fixes the
split so three engineers can work in parallel. The UI comes in a later block.

## Data flow
```
UI ──Command::Chat(ChatCommand)──► backend::run ──► handle_chat (backend.rs, done)
     REST: rest(session, …) ──► api/chat.rs  (ApiClient methods)  ──► ChatEvent / ActionFailed
           SendWithFiles   ──► api/upload.rs send_with_files      ──► UploadProgress*, SendResult
     gateway: SetStatus     ──► GatewayCommand::UpdatePresence  (op 3)
              SearchMembers ──► GatewayCommand::RequestMembers  (op 8) ──► ChatEvent::MembersFound
gateway dispatches ──► normalize.rs ──► GatewayEvent::{TypingStart, PresenceUpdate, MyStatus,
                                         ChannelPinsUpdate} ──► AppState::apply_gateway
UI drain_events: BackendEvent::Chat(ev) ──► AppState::apply_chat (state/chat.rs, done) ──► toast?
```
Logged out, `rest` answers at once with "Not connected" (reads get their typed event with `Err`,
actions an `ActionFailed`, uploads a failed `SendResult`; typing and member search stay silent).

## Contract (do not change without the contract owner; additive only)
- `backend.rs`: `Command::Chat`, `ChatCommand`, `ChatEvent`, `BackendEvent::Chat`, `UploadFile`,
  `UploadSource`, `SearchScope`, `SearchQuery`, `SearchSort`, `SearchOutcome`.
- `model.rs`: `UserStatus`, `UserPresence`, `CustomStatus`, `Activity`, `PinnedMessage`, `PinsPage`,
  `SearchPage`, `GifItem`, `Id::from_timestamp_ms`, `User::premium_type`, `Guild::premium_tier`,
  `Channel::last_pin_timestamp`, the four new `GatewayEvent` variants.
- `gateway/mod.rs`: `GatewayCommand::{UpdatePresence, RequestMembers}`.
- `api/chat.rs`: the `ApiClient` method signatures. `api/upload.rs`: `send_with_files`, `MAX_FILES`.
- `state/chat.rs`: `ChatState` and its view structs (UI reads fields directly), `AppState::{apply_chat,
  typing_users, typing_text, prune_typing, presence, upload_limit, mark_unread, need_member_search}`.
- `emoji.rs`: `CATEGORIES`, `category_emojis`, `by_shortcode`, `shortcode`, `search`, `with_skin_tone`.

## File ownership
| Owner | Files | Status |
|---|---|---|
| contract owner | `backend.rs` (`handle_chat`, `rest`, `TypingThrottle`), `model.rs`, `state/chat.rs` + its hooks in `state.rs`, `emoji.rs`, this page, `Cargo.toml` (`emojis` 0.9.1) | done, tested |
| upload engineer | `api/upload.rs` (private helpers/`impl ApiClient` blocks inside it) | done, tested (not live) |
| gateway engineer | `gateway/normalize.rs` (new dispatches), `gateway/conn.rs` (op 3, op 8, nonce map) | done, tested (not live) |
| REST engineer | `api/chat.rs` | done, tested (not live) |

## Per-area rules
- **Upload** (§1.1): step 1 `POST /channels/{c}/attachments`, step 2 `PUT upload_url` from a
  separate client with **no** `Authorization`/`Content-Type`/Discord headers (5xx-only retry,
  ≤ 5), step 3 the message with `uploaded_filename`. Cancel (`oneshot` from `handle_chat`) or a
  failed step 3 → `DELETE /attachments/{upload_filename}` for each uploaded file, `Ok(None)` on
  cancel. `Path` sources via `spawn_blocking`. Error codes 40005/413, 50045, 50046, 50146, 400001
  → readable text. Progress per finished file.
- **Typing** (§1.2): backend sends ≤ 1 `POST /typing` per channel per 8 s and resets on send
  (`TypingThrottle`); a 200 body's `message_send_cooldown_ms` → `SendCooldown`. Indicator: 10 s
  after receipt, cleared by that user's MESSAGE_CREATE; me and blocked users ignored (state, done).
- **Presence** (§1.3): normalize PRESENCE_UPDATE (vec of one), READY_SUPPLEMENTAL
  `merged_presences` (friends + guilds, index-aligned), member-list item presences (extra event
  next to `MemberListUpdate`), all as `PresenceUpdate`; `client_status` with only `mobile` →
  `mobile`; type 4 → `custom_status`; first other activity → `activity`. `MyStatus` from READY
  `user_settings.status/custom_status` (or `sessions`), SESSIONS_REPLACE, USER_SETTINGS_UPDATE.
  Op 3: full activity list (keep the custom status), ≤ 5 / 20 s, debounced newest-wins, never on
  a timer. Persisting the status (`PATCH /users/@me/settings` / proto) is the settings block.
- **Member search** (§1.7): op 8 `query` only (never `""`), nonce ≤ 32 bytes, small bounded
  nonce map, ~600 ms debounce per guild, one retry after RATE_LIMITED `retry_after`.
- **Pins/search/GIFs/extras** (§1.4, 1.5, 1.8, 1.9, 1.10): exact routes in the `api/chat.rs` doc
  comments. Search 202 → one delayed retry inside the call, then `SearchOutcome::Indexing`.
  GIFs only via Discord's `/gifs/*` (`media_format=gif`, `limit ≥ 20`); GIF images load through
  the image cache (no token). No 4xx is retried; 429 goes through the limiter.

## Memory bounds (state/chat.rs)
Typing: live entries only (pruned on insert). Presences: offline removed, cap 5,000. Member
search: 16 answers. Views: one each (pins, search, GIFs, jump window, reaction users).
Cooldowns: expired ones pruned on insert. Uploads: one per unsent message (UI removes on Dismiss).

## Notes for the UI block
- Open a view by setting it, then send the command; stale answers are dropped by key.
- Pending attachment send = `PendingMessage` + `chat.uploads[nonce] = Upload { files, .. }`;
  Retry re-sends `SendWithFiles` with the kept files; Dismiss/cancel removes both (+ `CancelUpload`).
- `mark_unread` pauses automatic acks (`mark_read` returns None) until another channel is
  opened or I send there; an explicit "Mark as read" should clear `chat.ack_paused` first.
- Call `prune_typing(now)` per frame while non-empty and `request_repaint_after` its result.
- Not covered yet: pins ack / pin dot (needs `ReadState.last_pin_timestamp`), search query text
  parser (`from:` …), `:shortcode:` conversion on send, custom status editing, byte-level upload
  progress (needs reqwest `stream`, not approved).

## UI hook-up guide
Commands are `Command::Chat(ChatCommand::X)`; answers arrive as `BackendEvent::Chat` and are
already applied in `ui/mod.rs` `drain_events` (`apply_chat`, toast on `Some`). Gateway events
are already applied by `apply_gateway`. The UI only sends commands and reads state.

| Feature | UI sends | Comes back as | UI reads |
|---|---|---|---|
| Attach / paste files | `SendWithFiles { nonce, files, .. }` + `PendingMessage` + `chat.uploads.insert(nonce, Upload { files, .. })`; pre-check sizes with `upload_limit(channel)`, ≤ `api::upload::MAX_FILES` | `UploadProgress`*, then `SendResult` (nothing on cancel) | `chat.uploads[nonce].sent/total`; failed send keeps `files` for Retry |
| Cancel / Dismiss an upload | `CancelUpload { nonce }`; remove the pending message and `chat.uploads[nonce]` | – | – |
| Typing (mine) | `Typing { channel_id }` on every keystroke that changes non-empty text (not `/…`, not editing, only with send permission); the backend throttles | `SendCooldown` (slowmode) | `chat.send_cooldowns[channel]` (block send until then) |
| Typing (others) | nothing (op 37 already subscribes `typing`) | `GatewayEvent::TypingStart` | `typing_text(channel, now)` / `typing_users`; per frame `prune_typing(now)` → `request_repaint_after` |
| Status dots, phone glyph, activity line | nothing | `GatewayEvent::PresenceUpdate` | `presence(user)` (dot), `presence_of(user)` (`mobile`, `custom_status`, `activity.label()`) |
| My status picker | `SetStatus { status }` (op 3, this session only; the PATCH is the settings block) | `GatewayEvent::MyStatus` after the op 3 goes out | `chat.my_status` (None until known; dot via `UserStatus::presence()`), `chat.my_custom_status` |
| Pins panel | set `chat.pins = Some(PinsView { channel_id, loading: true, .. })`, `FetchPins { before: None }`; more: `before = Some(last.pinned_at)` | `PinsLoaded` | `PinsView.items/has_more/error`; `stale` → refetch from `None` |
| Pin / unpin | `PinMessage` / `UnpinMessage` | `ActionFailed` on error; CHANNEL_PINS_UPDATE marks the open view `stale` | `Channel::last_pin_timestamp` |
| Message search | set `chat.search = Some(SearchView { scope, query, outcome: None })`, `Search { scope, query }`; pages: same query with `offset += 25` | `SearchResults` (matched by scope + query) | `SearchView.outcome`: `Page` / `Indexing` ("Indexing…", manual retry) / `Failed`; dates via `Id::from_timestamp_ms` |
| `@` autocomplete | local members first; if `need_member_search(guild, q, limit)` → `SearchMembers { guild_id, query, limit }` (gateway debounces 600 ms) | `MembersFound` (users go into `users`) | `chat.member_search` (newest last) |
| GIF picker | set `chat.gifs = Some(GifView { query, result: None })`, `Gifs { query }` (`None` = trending; debounce typing ≥ 500 ms) | `GifResults` | `GifView.result`; images via `LoadImage { url: src, max_side }`; sending = `SendMessage` with `GifItem.url` |
| Mark unread | `if let Some((target, n)) = state.mark_unread(ch, msg)` → `MarkUnread { channel_id: ch, message_id: target, mention_count: n }` | `ActionFailed` on error | `chat.ack_paused` (clear it for "Mark as read") |
| Jump to message (reply / search hit / pin) | set `chat.around = Some(AroundView { channel_id, message_id, result: None })`, `FetchAround` | `AroundLoaded` (toast if missing) | `AroundView.result` (sorted ascending) |
| Remove embeds | `SuppressEmbeds { flags: message.flags }` | `ActionFailed` on error; MESSAGE_UPDATE updates the message | – |
| Who reacted | set `chat.reaction_users = Some(ReactionUsersView { channel_id, message_id, emoji, loading: true, .. })`, `FetchReactionUsers { burst, after: None }`; more: `after = last user id` | `ReactionUsers` | `ReactionUsersView.users/has_more/error` |
| Emoji picker / `:name:` | nothing (local data) | – | `emoji::{CATEGORIES, category_emojis, search, by_shortcode, shortcode, with_skin_tone}` |

Removing the `#[allow(dead_code, reason = "…UI…not built yet")]` markers on `Command::Chat`,
`ChatCommand`, `UploadSource`, `SearchScope`, `SearchSort`, `SearchOutcome`, `state/chat.rs`,
`emoji.rs` and the model helpers is part of the UI block once they are used.
