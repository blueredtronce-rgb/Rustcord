//! Server tag card (docs/GUILD_CARD_NOTES.md): a click on someone's server tag chip
//! (`cosmetics::tag_button`) opens this popup with the server's profile, asked for on that click
//! (`GET /guilds/{g}/profile`, cached 10 minutes): banner, icon, mark, name, tag, online and
//! member counts, description, traits and one action (`cta`): Go to server (I am in it), Join
//! server (discoverable), Apply to join (applications open; the form is
//! `server_menu::join_application_modal`), or none (invite only, private). Join and apply are
//! sent only on their click; a captcha answer says to join in the official app, never solved.
//! OWNERSHIP: profile engineer.

use super::icons;
use super::messages::{Btn, Pal, button};
use super::{Modal, UiCx, cosmetics, semibold};
use crate::api::cdn;
use super::images::ImageCache;
use crate::backend::{AccountCommand, Command};
use crate::model::{GuildProfile, GuildTrait, Id, PrimaryGuild};
use crate::state::account::{GuildProfileEntry, OpState};
use egui::{Color32, CornerRadius, FontId, Frame, Margin, Rect, RichText, Sense, Vec2, pos2, vec2};
use std::time::Instant;

pub const WIDTH: f32 = 300.0;
const BAND: f32 = 72.0;
const ICON: f32 = 64.0;

/// The card's one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cta {
    /// "Go to server": it is in my server list.
    GoTo,
    /// Disabled "Joining…" / "Opening server…" (joined, waiting for its GUILD_CREATE).
    Joining,
    Opening,
    /// Disabled "Sending…" / "Application sent".
    Sending,
    Applied,
    /// "Apply to join" (visibility 3).
    Apply,
    /// "Join server" (DISCOVERABLE).
    Join,
    /// No button: "This server only takes new members by invite."
    InviteOnly,
    /// No button (loading, private, failed).
    None,
}

/// Which action the card offers, first match: member, a join / application on its way or
/// done, applications open, discoverable, invite only. `profile`: the loaded public profile.
/// Simplified from the mobile client's rules (UNCONFIRMED parity).
pub fn cta(member: bool, join: Option<&OpState>, apply: Option<&OpState>, profile: Option<&GuildProfile>) -> Cta {
    if member {
        return Cta::GoTo;
    }
    match (join, apply) {
        (Some(OpState::Pending), _) => return Cta::Joining,
        (Some(OpState::Done(_)), _) | (_, Some(OpState::Done(Some(_)))) => return Cta::Opening,
        (_, Some(OpState::Pending)) => return Cta::Sending,
        (_, Some(OpState::Done(None))) => return Cta::Applied,
        _ => {}
    }
    match profile {
        Some(p) if p.visibility == 3 => Cta::Apply,
        Some(p) if p.features.iter().any(|f| f == "DISCOVERABLE") => Cta::Join,
        Some(_) => Cta::InviteOnly,
        None => Cta::None,
    }
}

/// Asks for the server's profile when its card opens (a click; cached, never twice at once).
pub fn fetch(cx: &mut UiCx<'_>, guild_id: Id) {
    if cx.state.request_guild_profile(guild_id, Instant::now()) {
        cx.cmd.send(Command::Account(AccountCommand::FetchGuildProfile { guild_id }));
    }
}

/// The card (inside the chip's popup, 300 wide, no frame padding): `pg` is the tag clicked.
pub fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pg: &PrimaryGuild) {
    let Some(g) = pg.identity_guild_id else { return };
    let app = Pal::new(cx.theme);
    let pal = Pal { surface: app.tk.raised, ..app };
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
    let (loading, result) = cx.state.guild_profile_entry(g).map_or((true, None), |e| (e.loading, e.result.clone()));
    let profile = result.as_ref().and_then(|r| r.as_ref().ok()).and_then(Option::as_deref);
    let local = cx.state.guild(g).map(|x| (x.name.clone(), x.icon.clone(), x.features.clone(), x.premium_tier));
    let member = local.is_some();
    let tag = pg.tag.clone().unwrap_or_default();
    let name = profile.map(|p| p.name.clone()).filter(|n| !n.is_empty()).or_else(|| local.as_ref().map(|l| l.0.clone())).unwrap_or_else(|| tag.clone());

    // Band: the profile's banner, else its brand colour, else `selected`.
    let r = ui.visuals().menu_corner_radius;
    let (band, _) = ui.allocate_exact_size(vec2(WIDTH, BAND), Sense::hover());
    let banner = profile.and_then(|p| cdn::discovery_splash_url(g, p.custom_banner_hash.as_deref()?, 600));
    let top = CornerRadius { nw: r.nw, ne: r.ne, sw: 0, se: 0 };
    match banner.and_then(|u| cx.images.get(ui.ctx(), &u, 600, false)) {
        Some(tex) => {
            let uv = super::profile::cover_uv(tex.size, band.size());
            ui.painter().add(egui::epaint::RectShape::filled(band, top, Color32::WHITE).with_texture(tex.id, uv));
        }
        None => {
            let fill = profile.and_then(|p| hex(p.brand_color_primary.as_deref()?)).unwrap_or(pal.tk.selected);
            ui.painter().rect_filled(band, top, fill);
        }
    }

    // Icon 64 in a 4 px ring, half over the band.
    let icon = Rect::from_min_size(pos2(band.left() + 16.0, band.bottom() - ICON / 2.0), Vec2::splat(ICON));
    ui.painter().rect_filled(icon.expand(4.0), 20.0, pal.surface);
    let hash = profile.and_then(|p| p.icon_hash.clone()).or_else(|| local.as_ref().and_then(|l| l.1.clone()));
    match hash.and_then(|h| cdn::guild_icon_hash_url(g, &h, 128, false)).and_then(|u| cx.images.get(ui.ctx(), &u, 128, false)) {
        Some(tex) => egui::Image::from_texture(tex).corner_radius(16).paint_at(ui, icon),
        None => super::images::paint_initial(ui.painter(), icon, 16.0, g.0, &name, pal.dark),
    }
    ui.add_space(ICON / 2.0 + 12.0);

    Frame::new().inner_margin(Margin { left: 16, right: 16, top: 0, bottom: 16 }).show(ui, |ui| {
        ui.set_width(WIDTH - 32.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        // Name with the server's mark; private, loading or failed: only the tag chip (no name yet).
        if profile.is_some() || member {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let mark = match (profile, &local) {
                    (Some(p), _) => icons::guild_mark(&p.features, p.premium_tier),
                    (None, Some(l)) => icons::guild_mark(&l.2, l.3),
                    _ => None,
                };
                if let Some((m, what)) = mark {
                    let boosts = profile.map_or(0, |p| p.premium_subscription_count);
                    super::messages::vector_icon(ui, m, 16.0, pal.text, pal.surface).on_hover_text(super::server_menu::mark_tip((m, what), boosts));
                }
                ui.add(egui::Label::new(RichText::new(&name).font(semibold(pal.heading())).color(pal.text)).wrap());
            });
        }
        let chip = PrimaryGuild {
            identity_guild_id: Some(g),
            identity_enabled: Some(true),
            tag: profile.and_then(|p| p.tag.clone()).or(pg.tag.clone()),
            badge: profile.and_then(|p| p.badge_hash.clone()).or(pg.badge.clone()),
        };
        if chip.tag.as_deref().is_some_and(|t| !t.is_empty()) {
            let (row, _) = ui.allocate_exact_size(vec2(cosmetics::tag_chip_width(ui, &chip, pal.size), pal.size + 4.0), Sense::hover());
            cosmetics::tag_chip(ui, cx.images, &chip, row.left_center(), pal.size, pal.text);
        }
        match (&result, profile) {
            (_, Some(p)) => details(ui, cx.images, &pal, cx.theme, p),
            (Some(Ok(None)), None) if !member => {
                ui.add(egui::Label::new(RichText::new("This server keeps its profile private. Ask a member for an invite.").size(pal.ui()).color(pal.text)).wrap());
            }
            (Some(Err(e)), None) => {
                error_line(ui, &pal, &format!("Couldn't load this server. {}.", e.trim_end_matches('.')));
                if !member && button(ui, &pal, Btn::Secondary, "Try again", true).clicked() {
                    cx.state.account.guild_profiles.retain(|e| e.guild_id != g);
                    fetch(cx, g);
                }
            }
            _ if loading && !member => {
                ui.label(RichText::new("Loading server…").size(pal.meta()).color(pal.muted));
            }
            _ => {}
        }
        action(ui, cx, &pal, g, member, profile);
    });
}

/// Counts, description and trait chips of a loaded profile.
fn details(ui: &mut egui::Ui, images: &mut ImageCache, pal: &Pal, theme: &crate::theme::Theme, p: &GuildProfile) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let online = super::presence_color(crate::model::Presence::Online, &theme.colors);
        for (dot, text) in [(online, format!("{} online", thousands(p.online_count))), (pal.muted, count(p.member_count, "member", "members"))] {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
            ui.label(RichText::new(text).size(pal.meta()).color(pal.muted));
            ui.add_space(6.0);
        }
    });
    if let Some(d) = p.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        let d: String = d.chars().take(300).collect();
        ui.add(egui::Label::new(RichText::new(d).size(pal.ui()).color(pal.text)).wrap());
    }
    let mut traits: Vec<&GuildTrait> = p.traits.iter().filter(|t| !t.label.trim().is_empty()).collect();
    traits.sort_by_key(|t| t.position);
    if !traits.is_empty() {
        trait_chips(ui, images, pal, &traits);
    }
}

const CHIP_H: f32 = 24.0;
const CHIP_GAP: f32 = 6.0;
const CHIP_PAD: f32 = 8.0;
const CHIP_ICON: f32 = 16.0;

/// The unicode emoji of a trait's `emoji_name`: the emoji itself or its shortcode ("speaking_head",
/// "detective", ":mag:"); None for anything else, which is dropped rather than printed as a word.
fn trait_unicode(name: &str) -> Option<&'static str> {
    let n = name.trim().trim_matches(':');
    emojis::get(n).or_else(|| crate::emoji::by_shortcode(n)).map(emojis::Emoji::as_str)
}

/// Trait chips as Discord draws them: emoji (custom image or unicode), then the label on one
/// line; chips flow left to right and a chip that does not fit starts the next row whole, a
/// label wider than the card is cut with "…" (full text on hover). Animated custom emoji stay
/// still (shortcut: a card is glanced at, not watched).
fn trait_chips(ui: &mut egui::Ui, images: &mut ImageCache, pal: &Pal, traits: &[&GuildTrait]) {
    let avail = ui.available_width();
    let font = FontId::new(pal.meta(), crate::theme::medium_family());
    let radius = (pal.radius / 2).max(2);
    enum Mark {
        Image(egui::load::SizedTexture),
        Glyph(std::sync::Arc<egui::Galley>),
    }
    // Measure first: every chip's mark, label and width, then the rows.
    let mut chips = Vec::with_capacity(traits.len());
    for t in traits {
        let mark = match (t.emoji_id, t.emoji_name.as_deref()) {
            (Some(id), _) => images.get(ui.ctx(), &cdn::emoji_url(id, false, 48), 48, false).map(Mark::Image),
            (None, Some(n)) => trait_unicode(n)
                .filter(|e| super::messages::has_emoji_glyph(ui, e) && super::pickers::drawable(ui.ctx(), e))
                .map(|e| Mark::Glyph(ui.painter().layout_no_wrap(e.to_owned(), FontId::proportional(CHIP_ICON - 2.0), pal.text))),
            _ => None,
        };
        let lead = if mark.is_some() { CHIP_ICON + 4.0 } else { 0.0 };
        let mut job = egui::text::LayoutJob::single_section(t.label.trim().to_owned(), egui::TextFormat::simple(font.clone(), pal.text));
        job.wrap = egui::text::TextWrapping { max_width: (avail - 2.0 * CHIP_PAD - lead).max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let label = ui.painter().layout_job(job);
        let w = 2.0 * CHIP_PAD + lead + label.size().x;
        chips.push((mark, label, w, &t.label));
    }
    let (mut x, mut y) = (0.0, 0.0);
    let spots: Vec<(f32, f32)> = chips
        .iter()
        .map(|(_, _, w, _)| {
            if x > 0.0 && x + w > avail {
                (x, y) = (0.0, y + CHIP_H + CHIP_GAP);
            }
            let at = (x, y);
            x += w + CHIP_GAP;
            at
        })
        .collect();
    let (area, _) = ui.allocate_exact_size(vec2(avail, y + CHIP_H), Sense::hover());
    for ((mark, label, w, full), (x, y)) in chips.into_iter().zip(spots) {
        let chip = Rect::from_min_size(area.min + vec2(x, y), vec2(w, CHIP_H));
        ui.painter().rect_filled(chip, radius, pal.tk.hover);
        let mut at = chip.left() + CHIP_PAD;
        match mark {
            Some(Mark::Image(tex)) => {
                egui::Image::from_texture(tex).paint_at(ui, Rect::from_center_size(pos2(at + CHIP_ICON / 2.0, chip.center().y), Vec2::splat(CHIP_ICON)));
                at += CHIP_ICON + 4.0;
            }
            Some(Mark::Glyph(g)) => {
                ui.painter().galley(pos2(at + (CHIP_ICON - g.size().x) / 2.0, chip.center().y - g.size().y / 2.0), g, pal.text);
                at += CHIP_ICON + 4.0;
            }
            None => {}
        }
        let (size, elided) = (label.size(), label.elided);
        ui.painter().galley(pos2(at, chip.center().y - size.y / 2.0), label, pal.text);
        if elided {
            ui.interact(chip, ui.make_persistent_id(("trait", x as i32, y as i32)), Sense::hover()).on_hover_text(full);
        }
    }
}

/// The full-width button (268 x 32) and what a failed join / application said.
fn action(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, g: Id, member: bool, profile: Option<&GuildProfile>) {
    let join_op = AccountCommand::JoinGuild { guild_id: g }.op();
    let apply_op = AccountCommand::SubmitJoinRequest { guild_id: g, version: None, form_fields: Vec::new() }.op();
    let (join, apply) = (cx.state.account.ops.get(&join_op).cloned(), cx.state.account.ops.get(&apply_op).cloned());
    let which = cta(member, join.as_ref(), apply.as_ref(), profile);
    let (label, kind, enabled) = match which {
        Cta::GoTo => ("Go to server", Btn::Primary, true),
        Cta::Joining => ("Joining…", Btn::Primary, false),
        Cta::Opening => ("Opening server…", Btn::Primary, false),
        Cta::Sending => ("Sending…", Btn::Primary, false),
        Cta::Applied => ("Application sent", Btn::Secondary, false),
        Cta::Apply => ("Apply to join", Btn::Primary, true),
        Cta::Join => ("Join server", Btn::Primary, true),
        Cta::InviteOnly => {
            ui.label(RichText::new("This server only takes new members by invite.").size(pal.meta()).color(pal.muted));
            return;
        }
        Cta::None => return,
    };
    ui.add_space(4.0);
    let clicked = ui
        .scope(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.allocate_ui_with_layout(vec2(WIDTH - 32.0, 32.0), egui::Layout::top_down_justified(egui::Align::Center), |ui| button(ui, pal, kind, label, enabled)).inner
        })
        .inner
        .clicked();
    if which == Cta::Applied {
        ui.label(RichText::new("The server's moderators will review it.").size(pal.meta()).color(pal.muted));
    }
    for op in [join, apply] {
        if let Some(OpState::Failed(e)) = op {
            error_line(ui, pal, &e);
        }
    }
    if !clicked {
        return;
    }
    match which {
        Cta::GoTo => {
            cx.state.select_guild(Some(g));
            cx.view.settings_open = false;
            cx.view.profile_card = None;
            if matches!(cx.view.modal, Some(Modal::Profile { .. })) {
                cx.view.modal = None;
            }
            ui.close();
        }
        Cta::Join => {
            if cx.state.account.begin(join_op) {
                cx.cmd.send(Command::Account(AccountCommand::JoinGuild { guild_id: g }));
            }
        }
        Cta::Apply => {
            cx.state.account.ops.remove(&apply_op);
            super::server_menu::open_application(cx, g);
            ui.close();
        }
        _ => {}
    }
}

/// An error in `danger` after a 12 px mark (meta, selectable: a captcha hand-off names the
/// official app).
fn error_line(ui: &mut egui::Ui, pal: &Pal, text: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let row_h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(pal.meta())));
        let (r, _) = ui.allocate_exact_size(vec2(12.0, row_h), Sense::hover());
        icons::paint(ui.painter(), icons::Icon::Info, Rect::from_center_size(r.center(), Vec2::splat(12.0)), pal.danger, pal.surface);
        ui.add(egui::Label::new(RichText::new(text).size(pal.meta()).color(pal.danger)).wrap().selectable(true));
    });
}

/// "#3d6f8e" -> its colour.
fn hex(s: &str) -> Option<Color32> {
    let v = u32::from_str_radix(s.strip_prefix('#')?, 16).ok().filter(|_| s.len() == 7)?;
    Some(super::rgb(v))
}

/// 1284 -> "1,284".
fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 member", "1,284 members".
fn count(n: u32, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// Test hook `RUSTCORD_NAV_DEMO=profile,tag-card:<member|join|apply|applied|invite|private|
/// loading|error>` with `RUSTCORD_OPEN=card:<user id>`: that user wears the CRAB tag (of the
/// first cached server for `member`, else of a server I'm not in) and its card holds a demo
/// profile in that state; the chip's card opens once (`demo_open`). No request.
pub(super) fn demo(cx: &mut UiCx<'_>, user_id: Id) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let Some(kind) = super::sidebar::nav_demo_value("tag-card") else { return };
    if ONCE.is_completed() {
        return;
    }
    ONCE.call_once(|| {
        let first = cx.state.guilds.first().map(|g| (g.id, g.name.clone()));
        let (g, name) = match first.filter(|_| kind == "member") {
            Some(f) => f,
            None => (DEMO_GUILD, "Crab Club".to_owned()),
        };
        if let Some(u) = cx.state.users.get_mut(&user_id) {
            let pg = PrimaryGuild { identity_guild_id: Some(g), identity_enabled: Some(true), tag: Some("CRAB".into()), badge: None };
            u.primary_guild = Some(Box::new(pg));
        }
        let features = |f: &[&str]| -> Vec<String> { f.iter().map(|s| (*s).to_owned()).collect() };
        let trait_ = |e: &str, l: &str, position| GuildTrait { emoji_name: Some(e.into()), label: l.into(), position, ..Default::default() };
        // A custom emoji (id, name), unicode emoji, shortcode names and labels past the card width.
        let custom = GuildTrait { emoji_id: Some(Id(1565443437363201030)), emoji_name: Some("crab_dance".into()), label: "Crab dance".into(), position: 3, ..Default::default() };
        let mut p = GuildProfile {
            id: g,
            name,
            member_count: 1284,
            online_count: 312,
            description: Some("Building small, fast tools in Rust. Weekly code reviews and a friendly help channel.".into()),
            brand_color_primary: Some("#3d6f8e".into()),
            tag: Some("CRAB".into()),
            traits: vec![
                trait_("🦀", "Rust", 0),
                trait_("🛠", "Tools", 1),
                trait_("🌙", "Late nights", 2),
                custom,
                trait_("speaking_head", "Community", 4),
                trait_("detective", "Federal", 5),
                trait_("mag", "Investigation", 6),
                trait_("united_nations", "Agency", 7),
                trait_("nope_not_an_emoji", "No emoji", 8),
                trait_("🤝", "A very long trait label that cannot fit on one line of the card at all", 9),
            ],
            features: features(&["COMMUNITY", "DISCOVERABLE"]),
            visibility: 1,
            premium_subscription_count: 9,
            premium_tier: 2,
            ..Default::default()
        };
        if matches!(kind, "apply" | "applied") {
            (p.features, p.visibility) = (features(&["COMMUNITY", "MEMBER_VERIFICATION_GATE_ENABLED", "MEMBER_VERIFICATION_MANUAL_APPROVAL"]), 3);
        }
        if kind == "invite" {
            p.features = features(&["COMMUNITY"]);
        }
        if kind == "applied" {
            let op = AccountCommand::SubmitJoinRequest { guild_id: g, version: None, form_fields: Vec::new() }.op();
            cx.state.account.ops.insert(op, OpState::Done(None));
        }
        let result = match kind {
            "private" => Some(Ok(None)),
            "loading" => None,
            "error" => Some(Err("Not connected".into())),
            _ => Some(Ok(Some(Box::new(p)))),
        };
        let entry = GuildProfileEntry { guild_id: g, fetched: Instant::now(), loading: kind == "loading", result };
        cx.state.account.guild_profiles.push_back(entry);
    });
}

/// A server none of the fixture's are (`demo`).
const DEMO_GUILD: Id = Id(1565443437363201020);

/// `demo`: the tag chip's card starts open (once).
pub(super) fn demo_open() -> bool {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    super::sidebar::nav_demo_value("tag-card").is_some() && !DONE.swap(true, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_offers_one_fitting_action() {
        let p = |vis: u8, f: &[&str]| GuildProfile { visibility: vis, features: f.iter().map(|s| (*s).to_owned()).collect(), ..Default::default() };
        let (apply, join, invite) = (p(3, &["COMMUNITY"]), p(1, &["DISCOVERABLE"]), p(1, &["COMMUNITY"]));
        assert_eq!(cta(true, None, None, Some(&apply)), Cta::GoTo, "member wins over visibility");
        assert_eq!(cta(true, None, None, None), Cta::GoTo, "private, but I'm in it");
        assert_eq!(cta(false, None, None, Some(&apply)), Cta::Apply);
        assert_eq!(cta(false, None, None, Some(&p(3, &["DISCOVERABLE"]))), Cta::Apply, "applications first");
        assert_eq!(cta(false, None, None, Some(&join)), Cta::Join);
        assert_eq!(cta(false, None, None, Some(&invite)), Cta::InviteOnly);
        assert_eq!(cta(false, None, None, None), Cta::None, "private or loading");
        assert_eq!(cta(false, Some(&OpState::Pending), None, Some(&join)), Cta::Joining);
        assert_eq!(cta(false, Some(&OpState::Done(Some(Id(1)))), None, Some(&join)), Cta::Opening);
        assert_eq!(cta(false, Some(&OpState::Failed("x".into())), None, Some(&join)), Cta::Join, "a failed join may be tried again");
        assert_eq!(cta(false, None, Some(&OpState::Pending), Some(&apply)), Cta::Sending);
        assert_eq!(cta(false, None, Some(&OpState::Done(None)), Some(&apply)), Cta::Applied);
        assert_eq!(cta(false, None, Some(&OpState::Done(Some(Id(1)))), Some(&apply)), Cta::Opening, "approved at once");
    }

    #[test]
    fn trait_emoji_names_become_emoji() {
        assert_eq!(trait_unicode("🦀"), Some("🦀"));
        assert_eq!(trait_unicode("speaking_head"), emojis::get_by_shortcode("speaking_head").map(emojis::Emoji::as_str));
        assert_eq!((trait_unicode(":mag:"), trait_unicode("detective").is_some(), trait_unicode("united_nations").is_some()), (trait_unicode("mag"), true, true));
        assert_eq!((trait_unicode("crab_dance"), trait_unicode("")), (None, None), "unknown names are dropped");
        let t: GuildTrait = serde_json::from_str(r#"{"emoji_id":"55","emoji_name":"crab","emoji_animated":null,"label":"x"}"#).unwrap();
        assert_eq!((t.emoji_id, t.emoji_animated), (Some(Id(55)), false));
    }

    #[test]
    fn counts_read_well() {
        assert_eq!((thousands(0), thousands(999), thousands(1284), thousands(1_234_567)), ("0".into(), "999".into(), "1,284".into(), "1,234,567".into()));
        assert_eq!((count(1, "member", "members"), count(2, "member", "members")), ("1 member".into(), "2 members".into()));
        assert_eq!(hex("#3d6f8e"), Some(Color32::from_rgb(0x3d, 0x6f, 0x8e)));
        assert_eq!((hex("3d6f8e"), hex("#3d6f8"), hex("#zzzzzz")), (None, None, None));
    }
}
