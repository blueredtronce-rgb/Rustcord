# Server tag card, join / apply, server marks (protocol notes, Nov 2026)

Implemented in `src/ui/guild_card.rs` (card), `src/ui/cosmetics.rs` (`tag_button`, badge
popover), `src/ui/server_menu.rs` (`Modal::JoinApplication`, header mark),
`src/ui/sidebar.rs` (rail tooltip mark), `src/api/guild_admin.rs` (REST),
`src/state/account.rs` (cache, ops). Sources: discord-userdoccers [UD] (discovery, guild,
gateway events), the de-minified mobile client [MOB] (`useGuildProfileCTA`,
`GuildProfileActionCreators`, `_joinGuild`, `MemberVerificationActionCreators`,
`GuildBadge`), the web route table [WEB], support article "Server Tags" [SUP].
**UNCONFIRMED** marks an inference not checked against a source; the code takes the stated
fallback so a wrong guess fails gracefully.

Rules kept: every request below follows a click (opening the card is the click on the tag).
No captcha is ever solved: a captcha answer says "Join this server in the official app". No
Discord icon files, path data or UI copy: own vector icons, own words.

## 1. Card data: `GET /guilds/{g}/profile` [UD, WEB `GUILD_PROFILE`]

- Answers for members, discoverable servers and `visibility` 1 or 3; otherwise 403 (code
  likely 50001, UNCONFIRMED). 403, 404 and 10004 become the card's private state
  (`Ok(None)`), not an error.
- Shape (all keys; Rustcord keeps the ones in `model::GuildProfile`):
  `{id, name, icon_hash, member_count, online_count, description, brand_color_primary,
  banner_hash, game_application_ids[], game_activity{}, tag, badge, badge_color_primary,
  badge_color_secondary, badge_hash, traits[{emoji_id, emoji_name, emoji_animated, label,
  position}], features[], visibility, custom_banner_hash, premium_subscription_count,
  premium_tier}`.
- `features` holds community features only: COMMUNITY, DISCOVERABLE, PARTNERED, VERIFIED,
  MEMBER_VERIFICATION_GATE_ENABLED, MEMBER_VERIFICATION_MANUAL_APPROVAL.
- `visibility`: 1 public, 2 invite only, 3 public and anyone may apply.
- Images: icon `icons/{g}/{icon_hash}` (`cdn::guild_icon_hash_url`), banner
  `discovery-splashes/{g}/{custom_banner_hash}.png?size=` (`cdn::discovery_splash_url`), tag
  badge `clan-badges/{g}/{badge_hash}.png` (`cdn::guild_tag_badge_url`).
- Cache: mobile keeps it 60 s; Rustcord 10 minutes, an error 30 s, 16 servers
  (`AppState::request_guild_profile`).

## 2. Join: `PUT /guilds/{g}/members/@me?lurker=false`, body `{}` [UD "Join Guild", MOB, WEB]

- "The guild must be discoverable" [UD]. 200 = the guild (+ `show_verification_form`,
  `welcome_screen`, `join_request`), 204 = already a member. GUILD_CREATE follows; the joined
  server opens (`select_on_create`).
- Mobile also sends `recommendation_load_id`, `location`, `from_directory` (null outside
  discovery) and `X-Context-Properties {source}`. Rustcord sends none (UNCONFIRMED whether
  Discord wants them; `accept_invite` has the same shortcut).
- MEMBER_VERIFICATION_GATE_ENABLED servers add me as `pending` until the rules are accepted:
  Rustcord opens the rules form right after the join when the cached profile has that feature
  (`AccountState::verify_after_join`; the join answer's `show_verification_form` is not read).

## 3. Apply (visibility 3) and accept the rules [UD guild, MOB]

1. `GET /guilds/{g}/member-verification` (`with_guild` left out = false) →
   `{version: ISO8601|null, description: string|null, form_fields: [{field_type, label,
   choices?, values?, required, description, automations, placeholder?}]}`.
2. `PUT /guilds/{g}/requests/@me {"version": <same>, "form_fields": [every field object as
   received + "response"]}`. `response`: TERMS `true`, TEXT_INPUT string ≤ 150, PARAGRAPH
   string ≤ 1000, MULTIPLE_CHOICE the choice index. An unanswered optional field is sent with
   `"response": null` (UNCONFIRMED). Answer: a partial join request with
   `application_status` STARTED / SUBMITTED / REJECTED / APPROVED; a TERMS-only form is
   APPROVED at once.
- No join is needed before applying (mobile opens the form directly for visibility 3).
- Another `field_type` (e.g. a future verification step): the form says to apply in the
  official app and stays disabled.
- "Application sent" lasts for the session only (`AccountState::ops`); the READY
  `guild_join_requests` and GUILD_JOIN_REQUEST_* events are not read (shortcut).

## 4. Errors in plain words (`guild_admin::join_readable`)

| Answer | Text |
|---|---|
| 400 without a code (captcha) | Discord wants a captcha before you join. Join this server in the official app. |
| 403 | This server only takes new members by invite. |
| 404 / 10004 | This server isn't available. |
| 30001, 40007, 20028 / 20029 | `readable` (server limit, banned, too often) |
| anything else | Discord's own message |

UNCONFIRMED: the codes for a full server, an age gate and the join-request cooldown; they keep
Discord's message.

## 5. Which button (`guild_card::cta`)

Simplified from [MOB] `useGuildProfileCTA` (its decompiled control flow is partly garbled;
parity UNCONFIRMED). First match: server in my list → Go to server; JoinGuild pending / done →
Joining… / Opening server…; SubmitJoinRequest approved → Opening server…, pending → Sending…,
submitted → Application sent; `visibility == 3` → Apply to join; DISCOVERABLE → Join server;
loaded otherwise → "This server only takes new members by invite."; private or loading → none.
A public (visibility 1) server that is not discoverable shows no join button.

## 6. Server marks (rail tooltip, server header, tag card)

- Discord shows one mark before a server's name: verified, else partnered, else the boost
  level gem ([MOB] `GuildBadge`: VERIFIED > PARTNERED). Where boost tiers rank on the web is
  UNCONFIRMED. Community / discoverable marks are skipped.
- Gateway [UD gateway-events]: with CLIENT_STATE_V2 `features` and `premium_tier` are in
  `properties`, `premium_subscription_count` at the top level; GUILD_UPDATE is a flat guild
  with all three. `gateway/normalize.rs` keeps only VERIFIED / PARTNERED of `features` (a big
  guild lists ~40); widen that filter for any later feature check.
- Icons (`ui/icons.rs`, own geometry, monochrome per DESIGN §8.2): `Verified`, `Partnered`,
  `BoostTier1-3`; `icons::guild_mark`. Header tooltip "Boost level 2, 9 boosts" ("1 boost";
  verified / partnered without the count).

## 7. Badges

A click (or Enter / Space) on a profile badge opens an info popover, never its `link`: the
icon at 40 px (loaded at 72), Rustcord's own title and text (`cosmetics::badge_info`), and the
API description under the title when it adds something ("Subscriber since …"). UNCONFIRMED:
the `premium_tenure_*` ids, what earns `orb_profile_badge`, the booster gem's month steps.

Skipped on purpose: tag clicks in member and DM rows, the card's game list, the server's
creation date, "Adopt tag", lurking / preview, join-request gateway events, the header mark's
click action.
