//! Profile card popup (avatar with status, banner, custom status bubble, names, activity, roles
//! in guild). Opened via `ViewState::profile_card`. My card shows my local banner/avatar
//! overrides (`crate::overrides`); opened above the user panel it is the account popout, whose
//! lower part (status picker, custom status, Edit Profile) is `user_settings::account_popout`.
//! Opening a card loads the profile (`profile_modal::fetch`): pronouns, About Me, mutual counts
//! (open the full profile on that tab), Member Since and my Note. Others' cards end with the
//! Message / friend state (Add, Accept, sent) / View Full Profile actions. Also asks the 80013
//! question each frame (`friends::ask_stranger`).
//! OWNERSHIP: friends/DMs/inbox/profile engineer (docs/SOCIAL_UI_DESIGN.md).

use super::messages::{icon_button, tag, Pal};
use super::profile_modal::{self, Tab};
use super::sidebar::{paint_status_text, presence_label, status_label, status_tooltip, user_dot, user_presence, user_status_text};
use super::{cosmetics, images, rgb, Modal, UiCx};
use crate::api::cdn;
use crate::backend::ContextLocation;
use crate::model::relationship_type;
use crate::config::AnimateMode;
use crate::model::{Activity, Id, Member, MemberListItem, Presence, User};
use crate::overrides::{self, Kind};
use crate::theme::bold_family;
use egui::emath::GuiRounding;
use egui::epaint::{RectShape, Vertex};
use egui::load::SizedTexture;
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Frame, Image, Key, Margin, Order, Rect, RichText, Sense, Stroke, Vec2};

const WIDTH: f32 = 300.0;
const AVATAR: f32 = 80.0;

/// (user, guild, anchor x/y bits) of the shown card and the pass it was opened in.
type Seen = ((Id, Option<Id>, u32, u32), u64);

/// Temp: the banner URL the card shows (freed when it closes or shows another one).
fn banner_id() -> egui::Id {
    egui::Id::unique("rc_profile_card_banner")
}

pub fn show_popup(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    super::friends::ask_stranger(cx);
    let seen_id = egui::Id::unique("rc_profile_card_seen");
    let Some(t) = cx.view.profile_card else {
        // Just closed: a note still being typed is saved like on blur; the banner texture goes
        // too, unless the full profile of that user now shows it.
        if let Some(((user, ..), _)) = ctx.data_mut(|d| d.remove_temp::<Option<Seen>>(seen_id)).flatten() {
            profile_modal::save_note(cx);
            if cx.view.modal.is_none() {
                cosmetics::release(ctx, cx.images);
            }
            let url = ctx.data_mut(|d| d.remove_temp::<String>(banner_id())).unwrap_or_default();
            let reused = matches!(cx.view.modal, Some(Modal::Profile { user_id, .. }) if user_id == user);
            if !url.is_empty() && !reused {
                cx.images.remove(&url);
            }
        }
        return;
    };
    // The click that opened the card must not count as a click outside it.
    let pass = ctx.cumulative_pass_nr();
    let key = (t.user_id, t.guild_id, t.anchor.x.to_bits(), t.anchor.y.to_bits());
    let opened = match ctx.data(|d| d.get_temp::<Option<Seen>>(seen_id)).flatten() {
        Some((k, p)) if k == key => p,
        _ => {
            ctx.data_mut(|d| d.insert_temp::<Option<Seen>>(seen_id, Some((key, pass))));
            pass
        }
    };
    let Some(user) = find_user(cx, t.user_id) else {
        cx.view.profile_card = None;
        return;
    };
    let is_me = cx.state.me.as_ref().is_some_and(|m| m.id == user.id);
    // My card growing up from the user panel is the account popout: global profile + statuses.
    let account = is_me && t.pivot == Align2::LEFT_BOTTOM;
    let guild_id = t.guild_id.filter(|_| !account);
    if pass == opened {
        // Opened by a click: the profile (bio, mutual servers, note; for the account popout
        // my badges: Nitro, boost, quests), cached for minutes.
        profile_modal::save_note(cx);
        if is_me {
            // My profile after a failed load (a network error): asked again now, not in minutes.
            cx.state.social.profiles.retain(|e| e.user_id != user.id || e.loading || !matches!(e.result, Some(Err(_))));
        }
        profile_modal::fetch(cx, t.user_id, guild_id);
    }
    let member = card_member(cx, guild_id, t.user_id);
    // Theme, effect and frame (ui/cosmetics.rs; the account popout: only my local picks).
    let look = cosmetics::profile_look(cx, user.id, guild_id, !account);
    // A themed card has its own colours and light / dark mode (§8), whatever the app's.
    let app = Pal::new(cx.theme);
    let pal = look.theme.map_or(app, |t| cosmetics::theme_pal(&app, t));
    let shadow = ctx.global_style().visuals.popup_shadow;
    let mut bounds = ctx.content_rect().shrink(16.0);
    // A frame's top staples stand above the card: their room too, not the title bar.
    bounds.min.y = ctx.content_rect().top() + super::TITLE_BAR_H + 8.0 + look.overflow_top(WIDTH);
    let resp = egui::Area::new(egui::Id::unique("rc_profile_card"))
        .order(Order::Foreground)
        .pivot(t.pivot)
        .fixed_pos(t.anchor)
        // Keep 16 px off the window edges, 8 px under the title bar (the user-panel anchor sits
        // near the bottom).
        .constrain_to(bounds)
        .show(ctx, |ui| {
            let behind = ui.ctx().layer_painter(ui.layer_id()).add(egui::Shape::Noop);
            // A themed card gets the theme's gradient outline instead (`theme_outline`).
            let stroke = if look.theme.is_some() { Stroke::NONE } else { Stroke::new(1.0, pal.muted.gamma_multiply(0.25)) };
            let framed = Frame::new()
                .fill(pal.surface)
                .corner_radius(10)
                .stroke(stroke)
                .shadow(shadow)
                .show(ui, |ui| {
                    if look.theme.is_some() {
                        cosmetics::theme_visuals(ui, &pal);
                    }
                    let theme_slot = ui.painter().add(egui::Shape::Noop);
                    let mut banner_bottom = 0.0;
                    // Taller than the window (short windows, long About Me): it scrolls.
                    let max_h = bounds.height() - 2.0;
                    // Fixed width like Discord's: content never widens the card.
                    egui::ScrollArea::vertical().max_width(WIDTH).max_height(max_h).min_scrolled_height(max_h).show(ui, |ui| {
                        banner_bottom = card(ui, cx, &pal, &user, member.as_ref(), guild_id, (is_me, account), look.theme);
                        if account {
                            // Status picker, custom status, Edit Profile (user_settings.rs).
                            super::user_settings::account_popout(ui, cx, &pal);
                        } else {
                            actions(ui, cx, &pal, user.id, guild_id, is_me);
                        }
                    });
                    if let Some(colors) = look.theme {
                        ui.painter().set(theme_slot, cosmetics::theme_shapes(ui.ctx(), "card", ui.min_rect(), 10, colors, banner_bottom, pal.dark));
                    }
                });
            let rect = framed.response.rect;
            if let Some(colors) = look.theme {
                ui.painter().add(cosmetics::theme_outline(rect, 10, colors));
            }
            let animate = cosmetics::animates(cx, ui.rect_contains_pointer(rect));
            look.finish(ui, cx.images, rect, 10, behind, cosmetics::shown_for(ui.ctx(), ("card", user.id)), animate);
        })
        .response;
    if ctx.input(|i| i.key_pressed(Key::Escape)) || (pass != opened && resp.clicked_elsewhere()) {
        cx.view.profile_card = None;
    }
}

/// The card's banner, avatar and names; others' cards and my own (not the account popout) also
/// show the loaded profile: pronouns, About Me, Member Since, mutual counts and my Note. With
/// cosmetics (ui/cosmetics.rs): decoration, name style, guild tag, badges; `theme`: the
/// profile theme the caller paints under it. Returns the banner's bottom edge.
#[allow(clippy::too_many_arguments, reason = "one call site; the card's whole context")]
fn card(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, user: &User, member: Option<&Member>, guild_id: Option<Id>, (is_me, account): (bool, bool), theme: Option<(u32, u32)>) -> f32 {
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
    let top = CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 };

    let (banner_url, fill) = banner(cx, user, guild_id, is_me, pal.dark);
    let banner_h = if banner_url.is_some() { 106.0 } else { 60.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, banner_h), Sense::hover());
    let animated = banner_animates(cx, banner_url.as_deref(), is_me, ui.rect_contains_pointer(rect));
    let shown = banner_url.as_deref().unwrap_or_default();
    let old = ui.ctx().data_mut(|d| {
        let s = d.get_temp_mut_or_default::<String>(banner_id());
        (s.as_str() != shown).then(|| std::mem::replace(s, shown.to_owned()))
    });
    if let Some(old) = old.filter(|o| !o.is_empty()) {
        cx.images.remove(&old);
    }
    match banner_url.and_then(|url| cx.images.get(ui.ctx(), &url, 600, animated)) {
        Some(tex) => paint_banner(ui.painter(), rect, top, tex, theme.map(|_| cosmetics::theme_veil(pal.dark))),
        None => {
            ui.painter().rect_filled(rect, top, fill);
        }
    }

    // Avatar overlapping the banner edge, ringed in the card color.
    let ring = theme.map_or(pal.surface, |(primary, _)| cosmetics::theme_ring(primary, pal.dark));
    let center = pos2(rect.left() + 16.0 + AVATAR / 2.0, rect.bottom());
    ui.painter().circle_filled(center, AVATAR / 2.0 + 5.0, ring);
    let avatar_rect = Rect::from_center_size(center, Vec2::splat(AVATAR));
    let avatar_url = overrides::my_avatar_url(user.id)
        .map_or_else(|| images::user_avatar_url(guild_id, user, member, 160, true), |u| overrides::large(&u));
    let animate = overrides::my_avatar_animates(user.id, ui.rect_contains_pointer(avatar_rect)).unwrap_or(true);
    let name = cx.state.member_display_name(guild_id, user, member);
    super::sidebar::user_avatar_at(ui, cx.images, &avatar_url, avatar_rect, animate, 1.0, (user.id, &name));
    // ViewIcons: the avatar and banner open in full size.
    crate::plugins::media::icon_link(ui, rect, "View Banner", || crate::plugins::media::banner_full(user));
    crate::plugins::media::icon_link(ui, avatar_rect, "View Avatar", || Some(crate::plugins::media::avatar_full(user)));
    let fx = overrides::effective_for(user, member);
    let fx_anim = cosmetics::animates(cx, ui.ui_contains_pointer());
    images::decoration_at(ui, cx.images, avatar_rect, fx.decoration.as_ref(), fx_anim, 1.0);
    let dot = user_dot(ui.painter(), avatar_rect, cx.state, user.id, &cx.theme.colors, ring);
    let presence = user_presence(cx.state, user.id);
    let label = match cx.state.chat.my_status.filter(|_| is_me) {
        Some(s) => status_label(s),
        None => presence_label(presence),
    };
    ui.interact(dot, ui.make_persistent_id("rc_card_status"), Sense::hover()).on_hover_text(label);
    // Custom status bubble right of the avatar, at most two lines (mine also while invisible,
    // like the user panel).
    let custom = if is_me { cx.state.chat.my_custom_status.is_some() } else { cx.state.presence_of(user.id).is_some_and(|p| p.custom_status.is_some()) };
    if let Some(st) = user_status_text(cx.state, user.id).filter(|_| custom && (is_me || presence != Presence::Offline)) {
        let left = avatar_rect.right() + 16.0;
        let pos = pos2(left + 12.0, rect.bottom() + 8.0);
        let bubble = ui.painter().add(egui::Shape::Noop);
        let format = egui::TextFormat::simple(FontId::proportional(pal.meta()), pal.text);
        let r = paint_status_text(ui, cx.images, pos, &st, format, 20.0, rect.right() - 16.0 - 12.0 - pos.x, 2).expand2(vec2(12.0, 6.0));
        ui.painter().set(bubble, egui::Shape::rect_filled(r, 16, pal.bg));
        if let Some(tip) = status_tooltip(ui, &st) {
            ui.interact(r, ui.make_persistent_id("rc_card_bubble"), Sense::hover()).on_hover_text(tip);
        }
    }
    ui.add_space(AVATAR / 2.0 + 6.0);

    Frame::new().inner_margin(Margin { left: 16, right: 16, top: 0, bottom: 14 }).show(ui, |ui| {
        ui.set_width(WIDTH - 32.0);
        let style = fx.name_style.as_ref();
        let name_g = cosmetics::name_galley(ui, &name, FontId::new(20.0, bold_family()), style, pal.text, WIDTH - 32.0);
        let (r, _) = ui.allocate_exact_size(name_g.size(), Sense::hover());
        cosmetics::paint_name(ui.painter(), r.min, name_g, style, pal.text, pal.dark);
        let handle = match user.discriminator.as_deref() {
            // Streamer Mode: my own username is personal information.
            _ if is_me && cx.settings.streamer.hides_personal() => String::new(),
            Some(d) if !d.is_empty() && d != "0" => format!("{}#{d}", user.username),
            _ => user.username.clone(),
        };
        let profile = profile_modal::loaded(cx, user.id, guild_id).filter(|_| !account);
        let pronouns = profile.and_then(|p| p.pronouns()).map(str::to_owned);
        let bio = profile.and_then(|p| p.bio()).map(str::to_owned);
        let mutual = profile.filter(|_| !is_me).map(|p| (p.mutual_guilds.len(), p.mutual_friends_count.map_or(p.mutual_friends.len(), |n| n as usize)));
        let note = profile.filter(|_| !is_me).map(|p| p.note.clone().unwrap_or_default());
        // The account popout too: the profile's badges, not only the `public_flags` ones.
        let badges = cosmetics::badges(user, profile_modal::loaded(cx, user.id, guild_id));
        ui.horizontal_wrapped(|ui| {
            if !handle.is_empty() {
                ui.label(RichText::new(handle).color(pal.muted));
            }
            if let Some(pg) = cosmetics::guild_tag(user, member) {
                let (r, resp) = ui.allocate_exact_size(vec2(cosmetics::tag_chip_width(ui, pg, pal.size), ui.text_style_height(&egui::TextStyle::Body)), Sense::hover());
                cosmetics::tag_chip(ui, cx.images, pg, r.left_center(), pal.size, pal.text);
                resp.on_hover_text(cosmetics::tag_tooltip(cx.state, pg));
            }
            if let Some(p) = pronouns {
                ui.label(RichText::new(format!("• {p}")).color(pal.muted));
            }
            if user.bot {
                tag(ui, "BOT", pal.tint(), pal.muted);
            }
        });
        cosmetics::badge_row(ui, cx.images, pal, &badges);
        if let Some((servers, friends)) = mutual.filter(|m| *m != (0, 0)) {
            mutual_line(ui, cx, pal, user.id, guild_id, servers, friends);
        }
        if let Some(bio) = bio {
            ui.separator();
            section(ui, pal, "About Me");
            let opts = super::markdown::MdOpts { guild_id, mentions: &[], id_salt: user.id, color: Some(pal.text), highlight: &[] };
            super::markdown::render(ui, cx, &bio, &opts);
        }
        // Its own section like Roles: the verb as the label.
        if let Some(activity) = cx.state.presence_of(user.id).and_then(|p| p.activity.clone()) {
            ui.separator();
            section(ui, pal, Activity::verb(activity.kind));
            activity_section(ui, cx, pal, &activity, 56.0, pal.surface);
        }
        if !account {
            ui.separator();
            section(ui, pal, "Member Since");
            ui.label(RichText::new(member_since(user.id)).color(pal.text));
        }
        let roles = guild_roles(cx, guild_id, member);
        if !roles.is_empty() {
            ui.separator();
            section(ui, pal, "Roles");
            role_pills(ui, pal, &roles, 3);
        }
        if !account {
            // Plugins: Friends Since, In Voice, Mutual Groups, Connections, Online On, permissions.
            crate::plugins::people::profile_sections(ui, cx, user, guild_id, true, &|ui, list| cosmetics::connections_grid(ui, pal, list));
        }
        if let Some(note) = note {
            ui.separator();
            section(ui, pal, "Note");
            profile_modal::note_field(ui, cx, user.id, &note);
        }
    });
    rect.bottom()
}

/// An activity (profile card, full profile, Active Now): its large image (else its type icon on
/// a tile) with the small image on the corner, `side` px, then the name, details, state and
/// time elapsed. `bg`: the ring around the small image.
pub(super) fn activity_section(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, a: &Activity, side: f32, bg: egui::Color32) {
    let rich = a.rich.as_deref();
    let now = super::threads::unix_ms();
    let elapsed = a.elapsed(now);
    if let Some(start) = rich.and_then(|r| r.start).filter(|_| elapsed.is_some()) {
        // Counted in whole minutes since `start` (<= now): wake once, when the next one is up.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(60_000 - (now - start) % 60_000));
    }
    let meta = FontId::proportional(pal.meta());
    let lines = [
        Some((a.name.clone(), super::bold(pal.size), pal.text)),
        rich.and_then(|r| r.details.clone()).map(|t| (t, meta.clone(), pal.text)),
        rich.and_then(|r| r.state.clone()).map(|t| (t, meta.clone(), pal.muted)),
        elapsed.map(|t| (t, meta.clone(), pal.muted)),
    ];
    let w = ui.available_width();
    let galleys: Vec<_> = lines.into_iter().flatten().map(|(t, f, c)| (super::one_line(ui, t, f, c, w - side - 12.0), c)).collect();
    let text_h = galleys.iter().map(|(g, _)| g.size().y + 2.0).sum::<f32>() - 2.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, side.max(text_h)), Sense::hover());
    let img = Rect::from_min_size(pos2(rect.left(), rect.center().y - side / 2.0), Vec2::splat(side));
    let large = cdn::activity_image_url(a, false, 128).and_then(|u| cx.images.get(ui.ctx(), &u, 128, false));
    match large {
        Some(tex) => Image::from_texture(tex).corner_radius(8).paint_at(ui, img),
        None => {
            ui.painter().rect_filled(img, 8, pal.tint());
            super::sidebar::paint_activity_icon(ui.painter(), a.kind, img.shrink(side * 0.22), pal.muted);
        }
    }
    if let Some(tex) = cdn::activity_image_url(a, true, 64).and_then(|u| cx.images.get(ui.ctx(), &u, 64, false)) {
        let small = Rect::from_center_size(img.right_bottom() - vec2(3.0, 3.0), Vec2::splat((side * 0.36).round()));
        ui.painter().circle_filled(small.center(), small.width() / 2.0 + 2.0, bg);
        Image::from_texture(tex).corner_radius(small.width() / 2.0).paint_at(ui, small);
    }
    if let Some(tip) = rich.and_then(|r| r.large_text.as_deref()) {
        ui.interact(img, ui.make_persistent_id(("rc_activity_art", rect.min.x.to_bits(), rect.min.y.to_bits())), Sense::hover()).on_hover_text(tip);
    }
    let mut y = rect.center().y - text_h / 2.0;
    for (g, c) in galleys {
        let h = g.size().y;
        ui.painter().galley(pos2(img.right() + 12.0, y), g, c);
        y += h + 2.0;
    }
}

/// Section heading, Title Case like the full profile's (`profile_modal::label`).
fn section(ui: &mut egui::Ui, pal: &Pal, text: &str) {
    ui.label(RichText::new(text).font(FontId::new(pal.meta(), crate::theme::semibold_family())).color(pal.muted));
}

/// Role chip height and the gap between chips / rows.
const CHIP_H: f32 = 22.0;
const CHIP_GAP: f32 = 4.0;
/// Chip padding left of the colour dot (8 px) + gap to the name, and right of the name.
const CHIP_LEAD: f32 = 6.0 + 8.0 + 4.0;
const CHIP_PAD: f32 = 6.0;

/// (row, x) of chips of `widths` flowed into rows of `max_w`.
fn flow(widths: &[f32], max_w: f32) -> Vec<(usize, f32)> {
    let (mut row, mut x) = (0, 0.0);
    widths
        .iter()
        .map(|&w| {
            if x > 0.0 && x + w > max_w {
                (row, x) = (row + 1, 0.0);
            }
            let at = (row, x);
            x += w + CHIP_GAP;
            at
        })
        .collect()
}

/// How many of the chips fit in `max_rows` rows, leaving room for a "+N" chip `more_w` wide
/// after them when not all do.
fn chips_shown(widths: &[f32], max_w: f32, max_rows: usize, more_w: f32) -> usize {
    let fits = |ws: &[f32]| flow(ws, max_w).last().is_none_or(|&(r, _)| r < max_rows);
    if fits(widths) {
        return widths.len();
    }
    (0..widths.len()).rev().find(|&k| fits(&[&widths[..k], &[more_w][..]].concat())).unwrap_or(0)
}

/// Role chips (colour dot + name) flowed into rows of the available width: a name longer than
/// a row is cut with "…" (tooltip: the whole name); beyond `max_rows` rows a "+N" chip that
/// shows them all (the card scrolls), until the roles change.
pub(super) fn role_pills(ui: &mut egui::Ui, pal: &Pal, roles: &[(String, u32)], max_rows: usize) {
    let open_id = ui.make_persistent_id(("rc_roles_open", roles));
    let max_rows = if ui.data(|d| d.get_temp::<bool>(open_id).unwrap_or(false)) { usize::MAX } else { max_rows };
    let font = FontId::new(pal.meta(), crate::theme::medium_family());
    let max_w = ui.available_width();
    let galleys: Vec<_> = roles.iter().map(|(name, _)| super::one_line(ui, name.as_str(), font.clone(), pal.text, max_w - CHIP_LEAD - CHIP_PAD)).collect();
    let mut widths: Vec<f32> = galleys.iter().map(|g| CHIP_LEAD + g.size().x + CHIP_PAD).collect();
    let more = |n: usize| super::one_line(ui, format!("+{n}"), font.clone(), pal.muted, max_w);
    let shown = chips_shown(&widths, max_w, max_rows, more(roles.len()).size().x + 2.0 * CHIP_PAD);
    let rest = roles.len() - shown;
    widths.truncate(shown);
    let more_g = (rest > 0).then(|| more(rest));
    if let Some(g) = &more_g {
        widths.push(g.size().x + 2.0 * CHIP_PAD);
    }
    let at = flow(&widths, max_w);
    let rows = at.last().map_or(0, |&(r, _)| r + 1) as f32;
    let (rect, _) = ui.allocate_exact_size(vec2(max_w, rows * CHIP_H + (rows - 1.0).max(0.0) * CHIP_GAP), Sense::hover());
    let chip = |i: usize| {
        let (row, x) = at[i];
        Rect::from_min_size(rect.min + vec2(x, row as f32 * (CHIP_H + CHIP_GAP)), vec2(widths[i], CHIP_H))
    };
    for (i, ((name, color), g)) in roles.iter().zip(galleys).take(shown).enumerate() {
        let r = chip(i);
        ui.painter().rect_filled(r, 4, pal.bg);
        ui.painter().circle_filled(pos2(r.left() + 10.0, r.center().y), 4.0, if *color != 0 { rgb(*color) } else { pal.muted });
        crate::plugins::people::role_dot(ui, pos2(r.left() + 10.0, r.center().y), *color);
        let elided = g.elided;
        ui.painter().galley(pos2(r.left() + CHIP_LEAD, r.center().y - g.size().y / 2.0), g, pal.text);
        if elided {
            ui.interact(r, ui.make_persistent_id(("rc_role_chip", i)), Sense::hover()).on_hover_text(name);
        }
    }
    if let Some(g) = more_g {
        let r = chip(shown);
        let resp = ui.interact(r, ui.make_persistent_id("rc_role_more"), Sense::click());
        ui.painter().rect_filled(r, 4, if resp.hovered() { pal.bg.blend(pal.tint()) } else { pal.bg });
        ui.painter().galley(pos2(r.left() + CHIP_PAD, r.center().y - g.size().y / 2.0), g, if resp.hovered() { pal.text } else { pal.muted });
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Show all roles"));
        if resp.on_hover_text("Show all roles").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            ui.data_mut(|d| d.insert_temp(open_id, true));
        }
    }
}

/// "3 Mutual Servers • 2 Mutual Friends": each opens the full profile on that tab.
fn mutual_line(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, user_id: Id, guild_id: Option<Id>, servers: usize, friends: usize) {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mut first = true;
        for (n, text, tab) in [(servers, "Server", Tab::Servers), (friends, "Friend", Tab::Friends)] {
            if n == 0 {
                continue;
            }
            if !first {
                ui.label(RichText::new("•").size(pal.meta()).color(pal.muted));
            }
            first = false;
            let label = egui::Label::new(RichText::new(format!("{n} Mutual {text}{}", plural(n))).size(pal.meta()).color(pal.muted)).sense(Sense::click());
            let link = ui.add(label);
            if link.hovered() {
                ui.painter().hline(link.rect.x_range(), link.rect.bottom(), Stroke::new(1.0, pal.muted));
            }
            if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                profile_modal::open_tab(cx, user_id, guild_id, tab);
            }
        }
    });
}

/// Account creation date from the user id ("Jan 5, 2021").
pub(super) fn member_since(user_id: Id) -> String {
    let at = chrono::DateTime::from_timestamp_millis(user_id.timestamp_ms() as i64);
    at.map_or_else(String::new, |t| t.format("%b %-d, %Y").to_string())
}

/// (image url, colour under or instead of it) of a user's banner: my local override, the
/// loaded profile's (server profile first), the cached user's. The colour: the profile theme,
/// the accent colour, else the avatar placeholder's hue (never a block of theme accent).
pub(super) fn banner(cx: &UiCx<'_>, user: &User, guild_id: Option<Id>, is_me: bool, dark: bool) -> (Option<String>, egui::Color32) {
    let profile = profile_modal::loaded(cx, user.id, guild_id);
    let local = is_me.then(|| cx.overrides.url(Kind::Banner)).flatten();
    let url = local.or_else(|| match profile.and_then(|p| p.banner()) {
        // shortcut: cdn.rs has no server-profile banner URL; same asset layout under guilds/{g}/users/{u}.
        Some((hash, true)) => {
            let user_url = cdn::user_banner_url(user.id, hash, 600);
            guild_id.map(|g| user_url.replacen(&format!("banners/{}", user.id), &format!("guilds/{g}/users/{}/banners", user.id), 1))
        }
        Some((hash, false)) => Some(cdn::user_banner_url(user.id, hash, 600)),
        None => user.banner.as_deref().map(|h| cdn::user_banner_url(user.id, h, 600)),
    });
    let color = profile.and_then(|p| p.theme_colors()).map(|(primary, _)| primary).or(user.accent_color);
    (url, color.map(rgb).unwrap_or_else(|| images::initial_colors(user.id.0, dark).0))
}

/// Whether the banner `url` animates: my local one per its setting (or while `hovered`),
/// others' when animated.
pub(super) fn banner_animates(cx: &UiCx<'_>, url: Option<&str>, is_me: bool, hovered: bool) -> bool {
    if is_me && cx.overrides.url(Kind::Banner).is_some() {
        cx.overrides.avatar_animate == AnimateMode::Always || hovered
    } else {
        url.is_some_and(|u| u.contains("/a_"))
    }
}

/// Lower share of an uploaded banner that fades into the card.
const BANNER_FADE: f32 = 0.45;

/// An uploaded banner cover-cropped into `rect` (rounded `top` corners), its lower part fading
/// out into what is painted under it: the card surface or the profile theme. `veil`: a themed
/// card's body overlay (`cosmetics::theme_veil`), ramped in under the fade so the banner meets
/// the themed body without a seam.
pub(super) fn paint_banner(painter: &egui::Painter, rect: Rect, top: CornerRadius, tex: SizedTexture, veil: Option<Color32>) {
    // The fade ends on the pixel grid, where the themed body overlay (a rect, snapped by the
    // tessellator) starts: from a fractional edge neither covered the row between them (a 1 px
    // line of bare theme gradient).
    let rect = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y.round_to_pixels(painter.pixels_per_point())));
    let uv = cover_uv(tex.size, rect.size());
    let v = |y: f32| egui::remap(y, rect.y_range(), uv.y_range());
    let split = rect.bottom() - rect.height() * BANNER_FADE;
    let (mut fade, mut ramp) = (egui::Mesh::with_texture(tex.id), egui::Mesh::default());
    const ROWS: u32 = 8;
    for i in 0..=ROWS {
        let t = i as f32 / ROWS as f32;
        // Smoothstep: no visible edge where the fade starts or ends.
        let a = t * t * (3.0 - 2.0 * t);
        let y = egui::lerp(split..=rect.bottom(), t);
        for (x, u) in [(rect.left(), uv.left()), (rect.right(), uv.right())] {
            fade.vertices.push(Vertex { pos: pos2(x, y), uv: pos2(u, v(y)), color: Color32::WHITE.gamma_multiply(1.0 - a) });
            ramp.colored_vertex(pos2(x, y), veil.unwrap_or_default().gamma_multiply(a));
        }
        if i > 0 {
            let k = 2 * i;
            for m in [&mut fade, &mut ramp] {
                m.add_triangle(k - 2, k - 1, k);
                m.add_triangle(k - 1, k + 1, k);
            }
        }
    }
    if veil.is_some() {
        painter.add(ramp);
    }
    // The rounded solid part reaches 1 px into the (still opaque) fade: no seam from its feathered edge.
    let solid = Rect::from_x_y_ranges(rect.x_range(), rect.top()..=split + 1.0);
    let solid_uv = Rect::from_x_y_ranges(uv.x_range(), uv.top()..=v(split + 1.0));
    painter.add(RectShape::filled(solid, top, Color32::WHITE).with_texture(tex.id, solid_uv));
    painter.add(fade);
}

/// The friend-state button of a profile: Add Friend, Accept (incoming), a disabled "sent" mark
/// (outgoing); nothing for friends, blocked users, bots and me.
pub(super) fn friend_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, user_id: Id) {
    use relationship_type::*;
    match cx.state.social.relationships.get(&user_id).map(|r| r.kind) {
        Some(INCOMING) => {
            if icon_button(ui, pal, "✔", "Accept Friend Request", 32.0, false, pal.text).clicked() {
                super::friends::add_friend(cx, user_id, ContextLocation::UserProfile);
            }
        }
        Some(OUTGOING) => {
            let sent = ui.add_enabled_ui(false, |ui| icon_button(ui, pal, "⌛", "Friend Request Sent", 32.0, false, pal.text));
            sent.inner.on_disabled_hover_text("Friend Request Sent");
        }
        _ if super::friends::can_add_friend(cx.state, user_id) && icon_button(ui, pal, "➕", "Add Friend", 32.0, false, pal.text).clicked() => {
            super::friends::add_friend(cx, user_id, ContextLocation::UserProfile);
        }
        _ => {}
    }
}

/// Popout actions on someone's card: Message, Add Friend (friends.rs), View Full Profile
/// (profile_modal.rs). My own card only has the last.
fn actions(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, user_id: Id, guild_id: Option<Id>, is_me: bool) {
    Frame::new().inner_margin(Margin { left: 16, right: 16, top: 0, bottom: 14 }).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let text = RichText::new("Message").color(cx.theme.colors.accent_text.to_color32());
            if !is_me && ui.add(egui::Button::new(text).fill(pal.accent).min_size(vec2(0.0, 32.0))).clicked() {
                super::friends::message_user(cx, user_id);
            }
            // My own card (opened in a server or DM): Discord's footer, Edit Profile.
            let edit = RichText::new("Edit Profile").color(cx.theme.colors.accent_text.to_color32());
            if is_me && ui.add(egui::Button::new(edit).fill(pal.accent).min_size(vec2(0.0, 32.0))).clicked() {
                cx.view.profile_card = None;
                (cx.view.settings_open, cx.view.settings_page) = (true, super::user_settings::Page::Profiles);
            }
            if !is_me {
                friend_button(ui, cx, pal, user_id);
            }
            if icon_button(ui, pal, "👤", "View Full Profile", 32.0, false, pal.text).clicked() {
                profile_modal::open(cx, user_id, guild_id);
            }
        });
    });
}

/// (name, color) of the member's roles in the guild, highest first, without @everyone.
pub(super) fn guild_roles(cx: &UiCx<'_>, guild_id: Option<Id>, member: Option<&Member>) -> Vec<(String, u32)> {
    let (Some(g), Some(m)) = (guild_id, member) else { return Vec::new() };
    let Some(guild) = cx.state.guild(g) else { return Vec::new() };
    let mut roles: Vec<_> = guild.roles.iter().filter(|r| r.id != g && m.roles.contains(&r.id)).collect();
    roles.sort_by_key(|r| std::cmp::Reverse(r.position));
    roles.into_iter().map(|r| (r.name.clone(), r.color)).collect()
}

fn find_user(cx: &UiCx<'_>, id: Id) -> Option<User> {
    let s = &*cx.state;
    if let Some(me) = s.me.as_ref().filter(|u| u.id == id) {
        return Some(me.clone());
    }
    if let Some(u) = s.users.get(&id) {
        return Some(u.clone());
    }
    let in_channel = s.selected_channel.and_then(|c| s.messages.get(&c)).and_then(|c| {
        c.msgs.iter().rev().find_map(|m| {
            if m.author.id == id { Some(m.author.clone()) } else { m.mentions.iter().find(|u| u.id == id).cloned() }
        })
    });
    in_channel.or_else(|| {
        s.member_lists.values().flat_map(|l| &l.items).find_map(|it| match it {
            MemberListItem::Member { member, .. } => member.user.as_ref().filter(|u| u.id == id).cloned(),
            _ => None,
        })
    })
}

/// Member data for the card: cached (mine / member list), else from a message in the open channel.
fn card_member(cx: &UiCx<'_>, guild_id: Option<Id>, user_id: Id) -> Option<Member> {
    let g = guild_id?;
    let s = &*cx.state;
    s.find_member(g, user_id).cloned().or_else(|| {
        s.selected_channel
            .and_then(|c| s.messages.get(&c))?
            .msgs
            .iter()
            .rev()
            .find(|m| m.author.id == user_id && m.member.is_some())
            .and_then(|m| m.member.clone())
    })
}

/// UV rect that crops a texture to fill `target` (like CSS `object-fit: cover`).
pub(super) fn cover_uv(tex: Vec2, target: Vec2) -> Rect {
    let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    if tex.x <= 0.0 || tex.y <= 0.0 || target.x <= 0.0 || target.y <= 0.0 {
        return full;
    }
    let (tex_aspect, target_aspect) = (tex.x / tex.y, target.x / target.y);
    if tex_aspect > target_aspect {
        let w = target_aspect / tex_aspect;
        Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
    } else {
        let h = tex_aspect / target_aspect;
        Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crops_the_longer_side() {
        let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        assert_eq!(cover_uv(vec2(600.0, 200.0), vec2(300.0, 100.0)), full);
        // Square texture into a 3:1 banner: keep the middle third vertically.
        let uv = cover_uv(vec2(600.0, 600.0), vec2(300.0, 100.0));
        assert!((uv.min.y - 1.0 / 3.0).abs() < 1e-5 && (uv.max.y - 2.0 / 3.0).abs() < 1e-5);
        assert_eq!((uv.min.x, uv.max.x), (0.0, 1.0));
        // Wide texture into a square: keep the middle horizontally.
        let uv = cover_uv(vec2(400.0, 100.0), vec2(100.0, 100.0));
        assert!((uv.min.x - 0.375).abs() < 1e-5 && (uv.max.x - 0.625).abs() < 1e-5);
        assert_eq!(cover_uv(Vec2::ZERO, vec2(1.0, 1.0)), full);
    }

    #[test]
    fn role_chips_wrap_and_collapse_into_more() {
        // Rows of 100: 60 + 4 + 30 fit, the next 60 wraps.
        assert_eq!(flow(&[60.0, 30.0, 60.0, 100.0], 100.0), vec![(0, 0.0), (0, 64.0), (1, 0.0), (2, 0.0)]);
        // A chip wider than the row (never: names are cut to it) still gets a row of its own.
        assert_eq!(flow(&[150.0, 10.0], 100.0), vec![(0, 0.0), (1, 0.0)]);
        // 40 chips of 40 in rows of 100: 2 per row; 3 rows show 5 of them and "+35".
        let widths = vec![40.0; 40];
        assert_eq!(chips_shown(&widths, 100.0, 3, 30.0), 5);
        assert_eq!(chips_shown(&widths[..6], 100.0, 3, 30.0), 6);
        assert_eq!(chips_shown(&widths, 100.0, usize::MAX, 30.0), 40);
        assert_eq!(chips_shown(&[], 100.0, 3, 30.0), 0);
    }

    #[test]
    fn banner_fades_out_over_its_lower_part() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 100.0));
        let tex = SizedTexture::new(egui::TextureId::User(7), vec2(600.0, 600.0));
        let veil = Color32::from_black_alpha(153);
        let mut out = ctx.run_ui(Default::default(), |ui| paint_banner(&ui.ctx().layer_painter(egui::LayerId::background()), rect, CornerRadius::same(10), tex, Some(veil)));
        let meshes: Vec<_> = out.shapes.iter().filter_map(|c| if let egui::Shape::Mesh(m) = &c.shape { Some(m.clone()) } else { None }).collect();
        let (ramp, fade) = (&meshes[0], &meshes[1]);
        let (first, last) = (fade.vertices[0], *fade.vertices.last().unwrap());
        // Opaque where the fade starts, gone at the bottom edge, the texture still cover-cropped.
        assert_eq!((first.color.a(), last.color.a()), (255, 0));
        assert!((first.pos.y - 55.0).abs() < 1e-3 && last.pos.y == 100.0);
        assert!((last.uv.y - 2.0 / 3.0).abs() < 1e-5);
        // The themed body overlay is fully there at the bottom edge, absent where the fade starts.
        assert_eq!((ramp.vertices[0].color.a(), ramp.vertices.last().unwrap().color), (0, veil));
        out.textures_delta.clear();

        // 125 %: a bottom edge on a pixel centre (186 pt = 232.5 px) snaps to the grid like the
        // themed body rect under it, no row left between them.
        ctx.set_pixels_per_point(1.25);
        let rect = Rect::from_min_size(pos2(0.0, 80.0), vec2(300.0, 106.0));
        let out = ctx.run_ui(Default::default(), |ui| {
            assert_eq!(ui.ctx().pixels_per_point(), 1.25);
            paint_banner(&ui.ctx().layer_painter(egui::LayerId::background()), rect, CornerRadius::same(10), tex, Some(veil))
        });
        let bottom = out.shapes.iter().find_map(|c| if let egui::Shape::Mesh(m) = &c.shape { m.vertices.last().map(|v| v.pos.y) } else { None }).unwrap();
        assert_eq!(bottom * 1.25, 233.0);
    }
}
