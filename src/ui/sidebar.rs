//! Title bar (current location), server icon column (home/DMs button, 40 px squircle guild
//! icons, unread/selection pill, mention badge; `server_bar()` order with folders, drag and drop
//! to reorder / build folders via `move_guild`, saved debounced), the channel/DM list card with
//! voice users, and the floating user panel (voice connection; me, mute / deafen / settings)
//! over both columns.
//! Presence helpers (status icons, phone glyph, custom status / activity lines) live here too.
//! Social / server surfaces are hooks into their modules (docs/SOCIAL_UI_DESIGN.md): the server
//! header and menus (server_menu.rs), thread rows (threads.rs), the Friends row, group-DM "+"
//! and the user / DM menu items (friends.rs).
//! OWNERSHIP: navigation-social engineer (docs/SOCIAL_UI_DESIGN.md).

use super::images::{ImageCache, initial, initial_colors, paint_initial};
use super::messages::{MenuIcon, Pal, icon_button, menu_item, menu_setup, paint_megaphone};
use super::{UiCx, bold, one_line, presence_color, rgb, server_menu, threads};
use crate::api::cdn;
use crate::backend::{AccountCommand, Command, ConnState};
use crate::config::AnimateMode;
use crate::model::{Activity, CustomStatus, GuildFolder, Id, Presence, UserStatus, channel_type as ct};
use crate::state::account::{GuildDrop, ServerBarItem};
use crate::state::{AppState, SidebarItem};
use crate::theme::{Theme, ThemeColors};
use egui::{Align2, Color32, CornerRadius, FontId, Rect, Role, Sense, Shape, Stroke, Vec2, WidgetInfo, pos2, vec2};
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ICON: f32 = 40.0;
/// CDN size and decode size of guild icons (40 pt at up to 2x scaling).
pub(super) const ICON_PX: u32 = 80;
/// Server icons are squircles; hover/selection only changes their colours.
const ICON_R: f32 = 12.0;
/// Duration of the icon colour change and the pill animation, seconds.
const ANIM: f32 = 0.15;
/// Channel-list header; the chat header (messages.rs) has the same height. Incl. the 1 px border.
const HEADER_H: f32 = 49.0;
/// Account row of the floating user panel, and the voice section above it while in a call.
const ACCOUNT_H: f32 = 56.0;
const VOICE_PANEL_H: f32 = 56.0;
/// Camera / screen-share buttons under it once connected: 32 px + 8 px gap.
const VOICE_BUTTONS_H: f32 = 40.0;
/// Gap between the floating user panel and the window's left / bottom edges.
const PANEL_MARGIN: f32 = 8.0;
const ROW_H: f32 = 32.0;
const DM_ROW_H: f32 = 42.0;
const VOICE_ROW_H: f32 = 32.0;
const ROW_INSET: f32 = 8.0;
/// Panel padding: header title, category labels and the user panel start here.
const PAD: f32 = 16.0;

// ---------------------------------------------------------------------------
// Guild bar
// ---------------------------------------------------------------------------

pub fn guild_bar(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    user_panel_space(ui, cx, "guild_bar_panel_space");
    demo_folders(ui, cx.state);
    save_folders(ui.ctx(), cx);
    egui::ScrollArea::vertical()
        .id_salt("guild_bar")
        .auto_shrink(false)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            // 40 px icons + 8 = the 48 px pitch.
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.add_space(4.0);
            home_button(ui, cx);
            crate::plugins::sidebar::read_all_button(ui, cx);

            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(Rect::from_center_size(r.center(), vec2(32.0, 1.0)), 1, line(&cx.theme.colors).color);

            let badges = cx.state.guild_unreads();
            let mut target = None;
            for (i, item) in cx.state.server_bar().into_iter().enumerate() {
                match item {
                    ServerBarItem::Guild(id) => {
                        guild_button(ui, cx, id, &badges, Slot::Top(i), &mut target);
                    }
                    ServerBarItem::Folder(f) => folder(ui, cx, &f, i, &badges, &mut target),
                }
            }
            add_server_button(ui, cx);
            ui.add_space(16.0);
            finish_drag(ui, cx, target);
        });
}

/// The guild being dragged in the server bar (drag-and-drop payload).
struct GuildDrag(Id);

/// Where a server-bar row sits: top level at `server_bar()` index, or in a folder.
#[derive(Clone, Copy)]
enum Slot {
    Top(usize),
    Child(i64, usize),
}

/// What dropping the dragged guild here does, and its marker: a line between rows or a ring
/// around the icon it merges with.
struct Target {
    to: GuildDrop,
    line: Option<f32>,
    ring: Option<Rect>,
}

/// The guild being dragged, if any.
fn dragged(ctx: &egui::Context) -> Option<Id> {
    egui::DragAndDrop::payload::<GuildDrag>(ctx).map(|d| d.0)
}

/// Drop target for the pointer over the row of `icon` in `slot`: the 8 px at its top / bottom
/// (and half the gap) insert before / after it; the middle does `merge` (a guild: a new folder
/// with it; a closed folder: into it). In a folder the halves insert before / after.
fn zone(pointer: Option<egui::Pos2>, icon: Rect, slot: Slot, merge: Option<GuildDrop>) -> Option<Target> {
    let y = pointer?.y;
    let (top, bottom) = (icon.top() - 4.0, icon.bottom() + 4.0);
    if !(top..bottom).contains(&y) {
        return None;
    }
    let line = |to, y| Some(Target { to, line: Some(y), ring: None });
    match slot {
        Slot::Top(i) if y < icon.top() + 8.0 => line(GuildDrop::Top(i), top),
        Slot::Top(i) if y > icon.bottom() - 8.0 => line(GuildDrop::Top(i + 1), bottom),
        Slot::Top(_) => merge.map(|to| Target { to, line: None, ring: Some(icon) }),
        Slot::Child(id, j) if y < icon.center().y => line(GuildDrop::Folder { id, index: j }, top),
        Slot::Child(id, j) => line(GuildDrop::Folder { id, index: j + 1 }, bottom),
    }
}

/// End of the bar: draws the drop marker and the dragged icon at the pointer; on release moves
/// the guild there (`move_guild`, shown at once) and schedules the save.
fn finish_drag(ui: &mut egui::Ui, cx: &mut UiCx<'_>, target: Option<Target>) {
    let ctx = ui.ctx().clone();
    let Some(id) = dragged(&ctx) else { return };
    let accent = cx.theme.colors.accent.to_color32();
    if let Some(t) = &target {
        if let Some(y) = t.line {
            ui.painter().hline(ui.max_rect().x_range().shrink(12.0), y, Stroke::new(2.0, accent));
        }
        if let Some(r) = t.ring {
            ui.painter().rect_stroke(r.expand(2.0), ICON_R + 2.0, Stroke::new(2.0, accent), egui::StrokeKind::Outside);
        }
    }
    if let (Some(p), Some(index)) = (ctx.pointer_latest_pos(), cx.state.guilds.iter().position(|g| g.id == id)) {
        let ghost = egui::Area::new(egui::Id::unique("rc_guild_drag")).order(egui::Order::Tooltip).interactable(false);
        ghost.fixed_pos(p - Vec2::splat(ICON / 2.0)).show(&ctx, |ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(ICON), Sense::hover());
            ui.multiply_opacity(0.8);
            paint_guild_icon(ui, cx, index, r, ICON_R, 1.0, false);
        });
    }
    if !ctx.input(|i| i.pointer.any_released()) {
        return;
    }
    egui::DragAndDrop::clear_payload(&ctx);
    let Some(t) = target else { return };
    let before = cx.state.server_bar();
    if let Some(folders) = cx.state.move_guild(id, t.to) {
        cx.state.account.settings.folders = folders;
        if cx.state.server_bar() != before {
            ctx.data_mut(|d| d.insert_temp(save_id(), Instant::now()));
        }
    }
}

fn save_id() -> egui::Id {
    egui::Id::unique("rc_folders_save")
}

/// Sends the dragged server-bar order (`SetGuildFolders`, the whole list) 1.5 s after the last
/// drop, so a few quick drags make one request.
fn save_folders(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    const DEBOUNCE: Duration = Duration::from_millis(1500);
    let Some(at) = ctx.data(|d| d.get_temp::<Instant>(save_id())) else { return };
    let left = DEBOUNCE.saturating_sub(at.elapsed());
    if !left.is_zero() {
        ctx.request_repaint_after(left);
        return;
    }
    ctx.data_mut(|d| d.remove::<Instant>(save_id()));
    let folders = cx.state.account.settings.folders.clone();
    cx.cmd.send(Command::Account(AccountCommand::SetGuildFolders { folders }));
}

/// A server folder: closed, a tile in its colour with up to four mini icons; open, a folder
/// glyph over its servers on a tinted background. A click toggles it (local only).
fn folder(ui: &mut egui::Ui, cx: &mut UiCx<'_>, f: &GuildFolder, index: usize, badges: &HashMap<Id, (bool, u32)>, target: &mut Option<Target>) {
    let c = cx.theme.colors.clone();
    let fid = f.id.unwrap_or_default();
    let open_key = folder_open_id(fid);
    let open = ui.data(|d| d.get_temp::<bool>(open_key)).unwrap_or(false);
    let color = f.color.map(rgb).unwrap_or(c.accent.to_color32());
    let name = f.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
        let names = f.guild_ids.iter().filter_map(|g| cx.state.guild(*g)).map(|g| g.name.as_str());
        names.collect::<Vec<_>>().join(", ")
    });
    let (unread, mentions) = f.guild_ids.iter().filter_map(|g| badges.get(g)).fold((false, 0), |(u, m), (gu, gm)| (u || *gu, m + gm));
    let selected = cx.state.selected_guild.is_some_and(|g| f.guild_ids.contains(&g));
    let open = crate::plugins::sidebar::force_open(open, selected);
    let bg = ui.painter().add(Shape::Noop);
    let (row, icon, resp, morph) = icon_slot(ui, ("rc_folder", fid), false);
    if open {
        // The folder glyph on a plain tile; the background spans it and its servers.
        let tab = Rect::from_min_size(icon.center() + vec2(-11.0, -9.0), vec2(9.0, 5.0));
        ui.painter().rect_filled(tab, 2, color);
        ui.painter().rect_filled(Rect::from_center_size(icon.center() + vec2(0.0, 2.0), vec2(22.0, 15.0)), 3, color);
    } else {
        ui.painter().rect_filled(icon, ICON_R, color.gamma_multiply(0.25 + 0.15 * morph));
        for (k, g) in f.guild_ids.iter().take(4).enumerate() {
            let at = icon.min + vec2(3.0 + 18.0 * (k % 2) as f32, 3.0 + 18.0 * (k / 2) as f32);
            if let Some(i) = cx.state.guilds.iter().position(|x| x.id == *g) {
                paint_guild_icon(ui, cx, i, Rect::from_min_size(at, Vec2::splat(16.0)), 5.0, 0.0, false);
            }
        }
    }
    decorate(ui, &c, row, icon, &resp, (selected && !open, unread && !open), if open { 0 } else { mentions });
    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, open, format!("Folder {name}")));
    let dragging = dragged(ui.ctx()).is_some();
    if !dragging {
        side_tooltip(&resp, &name);
    }
    if resp.clicked() {
        ui.data_mut(|d| d.insert_temp(open_key, !open));
        crate::plugins::sidebar::folder_clicked(ui.ctx(), cx.state, fid, !open);
    }
    if dragging {
        let into = GuildDrop::Folder { id: fid, index: if open { 0 } else { usize::MAX } };
        let t = zone(ui.ctx().pointer_latest_pos(), icon, Slot::Top(index), Some(into));
        // Open: below the glyph's top edge everything goes in, first.
        let t = t.map(|t| if open && !matches!(t.to, GuildDrop::Top(i) if i == index) { Target { to: into, line: Some(icon.bottom() + 4.0), ring: None } } else { t });
        if t.is_some() {
            *target = t;
        }
    }
    if !open {
        return;
    }
    let mut last = icon;
    for (j, g) in f.guild_ids.iter().enumerate() {
        last = guild_button(ui, cx, *g, badges, Slot::Child(fid, j), target);
    }
    let back = Rect::from_min_max(icon.min - Vec2::splat(4.0), last.max + Vec2::splat(4.0));
    ui.painter().set(bg, Shape::rect_filled(back, ICON_R + 4.0, color.gamma_multiply(0.15)));
}

/// Memory key of a folder's open state (local, this session).
fn folder_open_id(folder: i64) -> egui::Id {
    egui::Id::unique(("rc_folder_open", folder))
}

/// RUSTCORD_NAV_DEMO=folders | folder-open: saved folders as READY would bring them (the
/// first server alone, the others in a named folder; `folder-open` opens it).
fn demo_folders(ui: &egui::Ui, state: &mut AppState) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if ONCE.is_completed() || !(nav_demo("folders") || nav_demo("folder-open")) {
        return;
    }
    ONCE.call_once(|| {
        let ids: Vec<Id> = state.guilds.iter().map(|g| g.id).collect();
        let Some((first, rest)) = ids.split_first() else { return };
        let folder = GuildFolder { id: Some(7), name: Some("Hobbies".into()), color: Some(0x3fb950), guild_ids: rest.to_vec() };
        state.account.settings.folders = vec![GuildFolder { guild_ids: vec![*first], ..Default::default() }, folder];
        state.account.settings.loaded = true;
        if nav_demo("folder-open") {
            ui.data_mut(|d| d.insert_temp(folder_open_id(7), true));
        }
    });
}

/// Test hook `RUSTCORD_NAV_DEMO=<comma list>`: navigation / social states offline mode can't
/// reach, none sends a request. `folders` / `folder-open` (server-bar folders), `mentions`
/// (inbox mentions from cached messages), `profile` (loaded profiles: bio, pronouns, mutual
/// servers and friends, a note), `tab:<online|all|pending|blocked|add>` (Friends tab).
pub(super) fn nav_demo(flag: &str) -> bool {
    nav_flags().split(',').any(|f| f.trim() == flag)
}

/// The value of a `key:value` entry of `RUSTCORD_NAV_DEMO`.
pub(super) fn nav_demo_value(key: &str) -> Option<&'static str> {
    nav_flags().split(',').find_map(|f| f.trim().strip_prefix(key)?.strip_prefix(':'))
}

fn nav_flags() -> &'static str {
    static FLAGS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FLAGS.get_or_init(|| std::env::var("RUSTCORD_NAV_DEMO").unwrap_or_default())
}

/// One 40x40 icon slot centred in a full-width row. Returns (row, icon rect, click response,
/// morph): morph eases 0 -> 1 (colours only) while the icon is hovered or `selected`.
fn icon_slot(ui: &mut egui::Ui, salt: impl egui::AsIdSalt, selected: bool) -> (Rect, Rect, egui::Response, f32) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), ICON), Sense::hover());
    let icon = Rect::from_center_size(row.center(), Vec2::splat(ICON));
    let resp = ui.interact(icon, ui.make_persistent_id(salt), Sense::click());
    let target = if selected || resp.hovered() { 1.0 } else { 0.0 };
    let morph = ui.ctx().animate_value_with_time(resp.id.with("morph"), target, ANIM);
    (row, icon, resp, morph)
}

/// Left-edge pill (height eases to 8 unread / 20 hover / 40 selected) and the mention badge
/// with a cut-out ring in the bar color.
fn decorate(ui: &egui::Ui, c: &ThemeColors, row: Rect, icon: Rect, resp: &egui::Response, (selected, unread): (bool, bool), mentions: u32) {
    let target = if selected {
        40.0
    } else if resp.hovered() {
        20.0
    } else if unread {
        8.0
    } else {
        0.0
    };
    let h = ui.ctx().animate_value_with_time(resp.id.with("pill"), target, ANIM);
    if h >= 1.0 {
        let pill = Rect::from_min_size(pos2(row.left(), row.center().y - h / 2.0), vec2(4.0, h));
        ui.painter().rect_filled(pill, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, c.text.to_color32());
    }
    if mentions > 0 {
        badge(ui, c, pos2(icon.right() + 4.0, icon.bottom() - 4.0), mentions, Some(c.guild_bar.to_color32()));
    }
}

/// Server-bar tooltip: the name in bold, right of the icon.
fn side_tooltip(resp: &egui::Response, text: &str) {
    let mut tip = egui::Tooltip::for_enabled(resp);
    tip.popup = tip.popup.align(egui::RectAlign::RIGHT).align_alternatives(&[]).gap(12.0);
    tip.show(|ui| ui.label(egui::RichText::new(text).family(crate::theme::bold_family())));
}

fn home_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let c = cx.theme.colors.clone();
    let selected = cx.state.selected_guild.is_none();
    let (row, icon, resp, morph) = icon_slot(ui, "home", selected);
    // The logo mark on a neutral tile; hover/selection turns the tile accent.
    let fill = c.surface.to_color32().lerp_to_gamma(c.accent.to_color32(), morph);
    let mark = c.accent.to_color32().lerp_to_gamma(c.accent_text.to_color32(), morph);
    ui.painter().rect_filled(icon, ICON_R, fill);
    logo_mark(ui.painter(), icon.center(), 30.0, mark, fill);
    decorate(ui, &c, row, icon, &resp, (selected, false), cx.state.dm_unread_count());

    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, "Direct Messages"));
    side_tooltip(&resp, "Direct Messages");
    if resp.clicked() {
        cx.state.select_guild(None);
        cx.view.settings_open = false;
    }
}

/// A server icon in `slot`. Once the saved folders are known it can be dragged: onto a gap to
/// move it, onto a server to make a folder, onto a folder to file it (`target` = the drop
/// spot under the pointer). Returns the icon rect.
fn guild_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id, badges: &HashMap<Id, (bool, u32)>, slot: Slot, target: &mut Option<Target>) -> Rect {
    let c = cx.theme.colors.clone();
    let Some(index) = cx.state.guilds.iter().position(|g| g.id == id) else { return Rect::NOTHING };
    let g = &cx.state.guilds[index];
    let unavailable = g.unavailable;
    // Unavailable guilds (outage) may arrive with only their id.
    let name = if g.name.is_empty() { "Unavailable server".to_owned() } else { g.name.clone() };
    let selected = cx.state.selected_guild == Some(id);
    let (unread, mentions) = badges.get(&id).copied().unwrap_or_default();

    let (row, icon, resp, morph) = icon_slot(ui, ("guild", id), selected);
    let drag = dragged(ui.ctx());
    let resp = if cx.state.account.settings.loaded { resp.interact(Sense::drag()) } else { resp };
    if resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), GuildDrag(id));
    }
    // Animated icons play on hover (or always).
    let animate = resp.hovered() || cx.settings.animate_others == AnimateMode::Always;
    paint_guild_icon(ui, cx, index, icon, ICON_R, morph, animate);
    if drag == Some(id) {
        // Its place while it is dragged: faded.
        ui.painter().rect_filled(icon, ICON_R, c.guild_bar.to_color32().gamma_multiply(0.7));
    }
    decorate(ui, &c, row, icon, &resp, (selected, unread), mentions);

    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, &name));
    if let Some(d) = drag {
        let merge = (d != id && matches!(slot, Slot::Top(_))).then_some(GuildDrop::NewFolder { with: id });
        if let Some(t) = zone(ui.ctx().pointer_latest_pos(), icon, slot, merge) {
            *target = Some(t);
        }
    } else {
        side_tooltip(&resp, &if unavailable { format!("{name} (unavailable)") } else { name.clone() });
    }
    if resp.clicked() {
        cx.state.select_guild(Some(id));
        cx.view.settings_open = false;
    }
    resp.context_menu(|ui| server_menu::guild_menu(ui, cx, id));
    icon
}

/// "Add a Server" (+) under the servers: opens the join / create dialog (server_menu.rs).
fn add_server_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let c = cx.theme.colors.clone();
    let (_, icon, resp, morph) = icon_slot(ui, "add_server", false);
    let fill = c.surface.to_color32().lerp_to_gamma(c.accent.to_color32(), morph);
    ui.painter().rect_filled(icon, ICON_R, fill);
    let fg = c.accent.to_color32().lerp_to_gamma(c.accent_text.to_color32(), morph);
    ui.painter().text(icon.center(), Align2::CENTER_CENTER, "+", FontId::proportional(24.0), fg);
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Add a Server"));
    side_tooltip(&resp, "Add a Server");
    if resp.clicked() {
        server_menu::open(cx, super::Modal::JoinServer);
    }
}

/// Marks channels read here and queues their acks (the shell sends them, debounced, when read
/// receipts are on). Also ends a "Mark Unread" pause of automatic acks.
pub(super) fn mark_read(cx: &mut UiCx<'_>, channels: impl IntoIterator<Item = Id>) {
    for ch in channels {
        if cx.state.chat.ack_paused == Some(ch) {
            cx.state.chat.ack_paused = None;
        }
        if let Some(mid) = cx.state.mark_read(ch)
            && cx.settings.send_read_acks
        {
            cx.view.pending_acks.insert(ch, mid);
        }
    }
}

/// A user's menu (member list, voice channel): profile, mention in the open channel, their DM
/// if one exists, and the id. `anchor` places the profile card.
pub(super) fn user_menu(ui: &mut egui::Ui, cx: &mut UiCx<'_>, user: Id, guild_id: Option<Id>, anchor: egui::Pos2) {
    menu_setup(ui);
    let text = cx.theme.colors.text.to_color32();
    if menu_item(ui, "Profile", MenuIcon::Glyph("👤"), text) {
        cx.view.open_profile(user, guild_id, anchor);
    }
    if let Some(ch) = cx.state.selected_channel.filter(|&ch| cx.state.can_send(ch))
        && menu_item(ui, "Mention", MenuIcon::Glyph("@"), text)
    {
        let draft = cx.view.drafts.entry(ch).or_default();
        if !draft.is_empty() && !draft.ends_with(char::is_whitespace) {
            draft.push(' ');
        }
        draft.push_str(&format!("<@{}> ", user.0));
    }
    // Message, Add Friend (friends.rs); Kick / Ban (server_menu.rs).
    super::friends::user_menu_items(ui, cx, user);
    if let Some(g) = guild_id {
        server_menu::member_menu_items(ui, cx, g, user);
    }
    crate::plugins::menus::user_items(ui, cx, user, guild_id, &mut |ui, label, glyph| menu_item(ui, label, MenuIcon::Glyph(glyph), text));
    // Settings > Advanced > Developer Mode.
    let dev = cx.state.account.settings.developer_mode();
    if dev {
        ui.separator();
    }
    if dev && menu_item(ui, "Copy User ID", MenuIcon::Glyph("🆔"), text) {
        ui.ctx().copy_text(user.0.to_string());
    }
}

/// Server icon `index` in `rect`; without one, its initials on the server's own colour, turning
/// accent as `morph` goes to 1 (unavailable servers grey). An animated icon plays while
/// `animate`, showing the static frame while the animation loads.
fn paint_guild_icon(ui: &mut egui::Ui, cx: &mut UiCx<'_>, index: usize, rect: Rect, radius: f32, morph: f32, animate: bool) {
    let g = &cx.state.guilds[index];
    let (id, unavailable) = (g.id, g.unavailable);
    let text = initials(&g.name);
    let (static_url, anim_url) = (cdn::guild_icon_url(g, ICON_PX, false), cdn::guild_icon_url(g, ICON_PX, true).filter(|_| animate));
    let ctx = ui.ctx().clone();
    let anim_tex = anim_url.and_then(|u| cx.images.get(&ctx, &u, ICON_PX, true));
    let tex = anim_tex.or_else(|| static_url.and_then(|u| cx.images.get(&ctx, &u, ICON_PX, false)));
    let c = &cx.theme.colors;
    match tex {
        Some(tex) => {
            let tint = if unavailable { Color32::from_gray(110) } else { Color32::WHITE };
            egui::Image::from_texture(tex).corner_radius(radius).tint(tint).paint_at(ui, rect);
        }
        None => {
            let (fill, fg) = if unavailable {
                (c.surface.to_color32(), c.text_muted.to_color32())
            } else {
                let (fill, fg) = initial_colors(id.0, ui.visuals().dark_mode);
                (fill.lerp_to_gamma(c.accent.to_color32(), morph), fg.lerp_to_gamma(c.accent_text.to_color32(), morph))
            };
            ui.painter().rect_filled(rect, radius, fill);
            let size = rect.height() * if text.chars().count() > 2 { 0.33 } else { 0.4 };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, bold(size), fg);
        }
    }
}

/// Title bar: the current location (server, DM or home) centred as a 16 px icon and its name;
/// the inbox button on the right (FEATURES_NOTES §4.5), except under a settings layer.
pub fn title_bar(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let rect = ui.max_rect();
    if !cx.view.chat_hidden() {
        let pal = super::messages::Pal::new(cx.theme);
        let slot = Rect::from_center_size(pos2(rect.right() - 8.0 - 14.0, rect.center().y), Vec2::splat(28.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(slot), |ui| super::inbox::button(ui, cx, &pal));
    }
    let c = cx.theme.colors.clone();
    let muted = c.text_muted.to_color32();
    let guild = cx.state.selected_guild.and_then(|g| cx.state.guilds.iter().position(|x| x.id == g));
    let dm = cx.state.selected_channel.filter(|_| guild.is_none());
    let name = match (guild, dm) {
        (Some(i), _) => cx.state.guilds[i].name.clone(),
        (None, Some(ch)) => cx.state.channel_name(ch),
        (None, None) => "Direct Messages".to_owned(),
    };
    let g = one_line(ui, name, bold(cx.theme.font.size - 0.5), muted, rect.width() - 64.0);
    let icon = Rect::from_min_size(pos2(rect.center().x - (24.0 + g.size().x) / 2.0, rect.center().y - 8.0), Vec2::splat(16.0));
    match (guild, dm) {
        (Some(i), _) => paint_guild_icon(ui, cx, i, icon, 4.0, 0.0, false),
        (None, Some(ch)) => {
            dm_avatar_at(ui, cx, ch, icon);
        }
        (None, None) => logo_mark(ui.painter(), icon.center(), 16.0, c.accent.to_color32(), c.guild_bar.to_color32()),
    }
    ui.painter().galley(pos2(icon.right() + 8.0, rect.center().y - g.size().y / 2.0), g, muted);
}

/// Channel-list card: the sidebar colour with a 12 px top-left corner over the server-bar
/// colour, and a hairline along its top and left edges that follows the corner.
pub fn paint_card(p: &egui::Painter, rect: Rect, c: &ThemeColors) {
    const R: f32 = 12.0;
    p.rect_filled(rect, CornerRadius { nw: R as u8, ..Default::default() }, c.sidebar.to_color32());
    let corner = rect.left_top() + Vec2::splat(R);
    let mut pts = vec![pos2(rect.left() + 0.5, rect.bottom())];
    pts.extend((0..=8).map(|i| corner + (R - 0.5) * Vec2::angled(PI + FRAC_PI_2 * i as f32 / 8.0)));
    pts.push(pos2(rect.right(), rect.top() + 0.5));
    p.add(Shape::line(pts, line(c)));
}

/// Rustcord's logo mark (the R of `assets/logo-small.svg` without its tile, its counter a chat
/// bubble with the tail at the stem) in `color`, `size` = about the tile's width (the glyph is
/// ~0.73 `size` tall); the counter is cut out in `bg`.
pub(super) fn logo_mark(p: &egui::Painter, center: egui::Pos2, size: f32, color: Color32, bg: Color32) {
    use egui::epaint::{BandPoint, BandShape};
    // Coordinates in the SVG's 512-unit space, a little larger than on the tile since there is
    // none here. Left out (under a pixel at in-app sizes): the tail tip's round-off and the
    // fillet where the bowl meets the stem.
    let k = size / 440.0;
    let at = |x: f32, y: f32| center + vec2(x - 256.0, y - 256.0) * k;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    let (outer, inner) = ((79.0 * k).round() as u8, (25.6 * k).round() as u8);
    p.rect_filled(rect(101.5, 90.9, 380.6, 283.9), CornerRadius { nw: outer, ne: outer, se: outer, sw: 0 }, color);
    p.rect_filled(rect(101.5, 200.0, 199.2, 411.8), CornerRadius { sw: inner, se: inner, ..Default::default() }, color);
    // The tail, sampled along y (densely near the baseline, where the inner edge turns flat):
    // the outer edge is an arc about (630.5, 257.7) from the bowl's corner, the inner one an arc
    // about (385.2, 283.9) tangent to the baseline.
    let (top, base) = (257.7, 411.8);
    let tail = (0..=16)
        .map(|i| {
            let y = base - (base - top) * (1.0 - i as f32 / 16.0).powi(2);
            let x_out = 630.5 - (270.3f32.powi(2) - (y - top).powi(2)).sqrt();
            let x_in = if y < 283.9 { 257.3 } else { 385.2 - (127.9f32.powi(2) - (y - 283.9).powi(2)).max(0.0).sqrt() };
            let (a, b) = (at(x_in, y), at(x_out, y));
            BandPoint::new(a.y, -b.x..=-a.x)
        })
        .collect();
    // A quarter turn: band x = screen y, band y = -screen x.
    p.add(BandShape { angle: FRAC_PI_2, ..BandShape::filled(tail, color) });
    p.rect_filled(rect(199.2, 153.7, 296.8, 221.1), inner, bg);
    // The bubble's tail: down the stem's edge, squaring the counter's lower-left corner.
    p.add(Shape::convex_polygon(vec![at(199.2, 195.0), at(234.2, 221.1), at(199.2, 252.0)], bg, Stroke::NONE));
}

/// Up to three initials for an icon placeholder ("Rust Workshop" -> "RW").
fn initials(name: &str) -> String {
    let s: String = name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect();
    if s.is_empty() { "?".into() } else { s }
}

/// Count badge (danger pill) whose right edge is centred vertically on `right_center`. `ring`
/// draws a 4 px outline in the background color so the badge stands out on top of icons.
/// Returns the badge width.
pub(super) fn badge(ui: &egui::Ui, c: &ThemeColors, right_center: egui::Pos2, count: u32, ring: Option<Color32>) -> f32 {
    let text = if count > 99 { "99+".to_owned() } else { count.to_string() };
    let galley = ui.painter().layout_no_wrap(text, bold(11.0), Color32::WHITE);
    let h = 16.0;
    let w = (galley.size().x + 8.0).max(h);
    let rect = Rect::from_min_max(pos2(right_center.x - w, right_center.y - h / 2.0), pos2(right_center.x, right_center.y + h / 2.0));
    if let Some(ring) = ring {
        ui.painter().rect_filled(rect.expand(4.0), CornerRadius::same(12), ring);
    }
    ui.painter().rect_filled(rect, CornerRadius::same(8), c.danger.to_color32());
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    w
}

/// Hairline between panel areas (same as the message view's header border).
pub(super) fn line(c: &ThemeColors) -> Stroke {
    Stroke::new(1.0, c.text_muted.to_color32().gamma_multiply(0.2))
}

/// One muted meta-size line (empty states), centred near the top of the area.
pub(super) fn empty_state(ui: &mut egui::Ui, cx: &UiCx<'_>, top: f32, text: &str) -> egui::Response {
    ui.add_space(top);
    let size = cx.theme.font.size - 2.0;
    // The muted text of the visuals: `text_muted`, or a themed profile's (cosmetics::theme_visuals).
    ui.vertical_centered(|ui| ui.label(egui::RichText::new(text).size(size).color(ui.visuals().weak_text_color())))
        .inner
}

/// `super::avatar_at` with the colored-initial placeholder for users (`who` = id, name).
pub(super) fn user_avatar_at(ui: &mut egui::Ui, images: &mut ImageCache, url: &str, rect: Rect, animate: bool, opacity: f32, who: (Id, &str)) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    child.multiply_opacity(opacity);
    super::images::avatar(&mut child, images, url, rect.width(), animate, Some(who));
}

/// (radius, ring, inset) of an avatar's status dot: 10 px on 32 px, 8 px on 24 px, cut out by a
/// 3 px ring; 16 px with a 6 px ring, 4 px in, on the 80 px profile avatar.
fn dot_metrics(avatar: Rect) -> (f32, f32, f32) {
    if avatar.width() >= 80.0 { (8.0, 6.0, 4.0) } else { (avatar.width() / 6.4, 3.0, 0.0) }
}

/// Status icon on the bottom-right of an avatar, cut out of it by a ring in the background
/// color `ring`. Returns the icon's rect (status tooltips).
pub(super) fn status_dot(p: &egui::Painter, avatar: Rect, presence: Presence, c: &ThemeColors, ring: Color32) -> Rect {
    let (r, cut, inset) = dot_metrics(avatar);
    let center = avatar.right_bottom() - Vec2::splat(r + inset);
    p.circle_filled(center, r + cut, ring);
    paint_status(p, center, r, presence, c, ring);
    Rect::from_center_size(center, Vec2::splat(2.0 * r))
}

/// Status icon of radius `r` with its cut-outs in `bg`: online a dot, idle a crescent, do not
/// disturb a bar, offline (and invisible) a ring.
pub(super) fn paint_status(p: &egui::Painter, center: egui::Pos2, r: f32, presence: Presence, c: &ThemeColors, bg: Color32) {
    p.circle_filled(center, r, presence_color(presence, c));
    match presence {
        Presence::Online => {}
        Presence::Idle => {
            p.circle_filled(center - Vec2::splat(r / 2.0), r * 0.75, bg);
        }
        Presence::Dnd => {
            p.rect_filled(Rect::from_center_size(center, vec2(r * 1.5, r * 0.5)), r * 0.25, bg);
        }
        Presence::Offline => {
            p.circle_filled(center, r / 2.0, bg);
        }
    }
}

/// `status_dot` of a user (mine: my status); a phone instead while they are online only on
/// mobile. Returns the icon's rect.
pub(super) fn user_dot(p: &egui::Painter, avatar: Rect, state: &AppState, user: Id, c: &ThemeColors, ring: Color32) -> Rect {
    let presence = user_presence(state, user);
    if presence != Presence::Online || !state.presence_of(user).is_some_and(|x| x.mobile) {
        return status_dot(p, avatar, presence, c, ring);
    }
    let (r, cut, inset) = dot_metrics(avatar);
    let size = vec2(2.0 * r, 3.0 * r);
    let phone = Rect::from_min_size(avatar.right_bottom() - size - Vec2::splat(inset), size);
    p.rect_filled(phone.expand(cut), r * 0.4 + cut, ring);
    p.rect_filled(phone, r * 0.4, presence_color(presence, c));
    // The screen; the strip left below it is the phone's chin.
    p.rect_filled(Rect::from_min_max(phone.min + vec2(r * 0.4, r * 0.4), phone.max - vec2(r * 0.4, r * 0.8)), 0.0, ring);
    phone
}

/// A user's status: mine from my chosen status and the connection, anyone else's from the
/// gateway's presences (offline when unknown).
pub(super) fn user_presence(state: &AppState, user: Id) -> Presence {
    if state.me.as_ref().is_some_and(|m| m.id == user) { my_presence(state) } else { state.presence(user) }
}

/// "Online", "Idle", "Do Not Disturb", "Offline".
pub(super) fn presence_label(p: Presence) -> &'static str {
    match p {
        Presence::Online => "Online",
        Presence::Idle => "Idle",
        Presence::Dnd => "Do Not Disturb",
        Presence::Offline => "Offline",
    }
}

/// A status I can pick, as the status menu names it.
pub(super) fn status_label(s: UserStatus) -> &'static str {
    if s == UserStatus::Invisible { "Invisible" } else { presence_label(s.presence()) }
}

/// A status line: what leads it and text (a unicode emoji leads it).
pub(super) type StatusText = (Option<Lead>, String);

/// The image leading a status line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Lead {
    /// A custom emoji, drawn as an image.
    Emoji(Id),
    /// An activity (its type): its icon, and the text after the verb is bold.
    Activity(u8),
}

/// What shows under a name: the custom status, else the activity ("Playing X").
pub(super) fn status_text(custom: Option<&CustomStatus>, activity: Option<&Activity>) -> Option<StatusText> {
    if let Some(cs) = custom {
        let emoji = cs.emoji.as_ref();
        let image = emoji.and_then(|e| e.id);
        let unicode = emoji.filter(|e| e.id.is_none()).and_then(|e| e.name.as_deref());
        let text = [unicode, cs.text.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        if image.is_some() || !text.is_empty() {
            return Some((image.map(Lead::Emoji), text));
        }
    }
    activity.map(|a| (Some(Lead::Activity(a.kind)), a.label()))
}

/// The whole status line with `:shortcode:`s for the emoji `paint_status_text` leaves out, when
/// it left one out (the tooltip of the line's row).
pub(super) fn status_tooltip(ui: &egui::Ui, (_, text): &StatusText) -> Option<String> {
    super::messages::missing_emoji(ui, text)?;
    let mut text = text.clone();
    while let Some((range, code)) = super::messages::missing_emoji(ui, &text) {
        text.replace_range(range, &format!(":{code}:"));
    }
    Some(text)
}

/// `status_text` of a user; mine uses my own custom status. None while offline.
pub(super) fn user_status_text(state: &AppState, user: Id) -> Option<StatusText> {
    let p = state.presence_of(user);
    let activity = p.and_then(|p| p.activity.as_ref());
    if state.me.as_ref().is_some_and(|m| m.id == user) {
        return status_text(state.chat.my_custom_status.as_ref(), activity);
    }
    status_text(p?.custom_status.as_ref(), activity)
}

/// Paints a status line in `format` with its top-left at `pos`, wrapped to `max_rows` (cut with
/// "…") at `max_w`. Its emoji (a custom one as a static image, or the unicode one leading it) is
/// `emoji_side` px (Discord: 16 in rows, 20 in the profile bubble): at text size the monochrome
/// glyphs blur into dots. It is centred on the first text row and overhangs it evenly. A unicode
/// emoji the fonts can't draw (e.g. 🦀) is left out of these short lines (`status_tooltip` names
/// it). Returns the painted rect.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_status_text(
    ui: &egui::Ui,
    images: &mut ImageCache,
    pos: egui::Pos2,
    (emoji, text): &StatusText,
    format: egui::TextFormat,
    emoji_side: f32,
    max_w: f32,
    max_rows: usize,
) -> Rect {
    let color = format.color;
    let row_h = ui.fonts_mut(|f| f.row_height(&format.font_id));
    // Activity icons at text size: they are line art, not stand-ins for coloured emoji.
    let side = if matches!(emoji, Some(Lead::Activity(_))) { (row_h * 0.85).round() } else { emoji_side };
    let lead = if emoji.is_some() { side + 4.0 } else { 0.0 };
    let mut text = text.clone();
    while let Some((range, _)) = super::messages::missing_emoji(ui, &text) {
        let end = if text[range.end..].starts_with(' ') { range.end + 1 } else { range.end };
        text.replace_range(range.start..end, "");
    }
    let format = egui::TextFormat { valign: egui::Align::Center, ..format };
    let first = text.split(' ').next().unwrap_or_default();
    let uni = if emojis::get(first).is_some() { first.len() } else { 0 };
    let mut job = egui::text::LayoutJob { break_on_newline: false, ..Default::default() };
    if uni > 0 {
        // Body text color, not the muted line color: the emoji stands in for a coloured one.
        let color = ui.visuals().text_color();
        job.append(&text[..uni], lead, egui::TextFormat { font_id: FontId::proportional(emoji_side), color, ..format.clone() });
    }
    let (body, x) = (&text[uni..], if uni == 0 { lead } else { 0.0 });
    match emoji {
        Some(Lead::Activity(kind)) if let Some(name) = body.strip_prefix(Activity::verb(*kind)) => {
            job.append(Activity::verb(*kind), x, format.clone());
            let font_id = FontId::new(format.font_id.size, crate::theme::bold_family());
            job.append(name, 0.0, egui::TextFormat { font_id, ..format });
        }
        _ => job.append(body, x, format),
    }
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(lead + 1.0), max_rows, break_anywhere: max_rows == 1, overflow_character: Some('…') };
    let g = ui.painter().layout_job(job);
    // The first row's centre stays where a plain text row's would be.
    let first_h = g.rows.first().map_or(row_h, |r| r.rect().height());
    let at = pos - vec2(0.0, (first_h - row_h) / 2.0);
    let mut painted = Rect::from_min_size(at, g.size());
    if let Some(lead) = emoji {
        let icon = Rect::from_min_size(at + vec2(0.0, (first_h - side) / 2.0), Vec2::splat(side));
        match lead {
            Lead::Emoji(id) => {
                let px = (side * ui.ctx().pixels_per_point()).ceil() as u32;
                if let Some(tex) = images.get(ui.ctx(), &cdn::emoji_url(*id, false, px), px, false) {
                    egui::Image::from_texture(tex).paint_at(ui, icon);
                }
            }
            Lead::Activity(kind) => paint_activity_icon(ui.painter(), *kind, icon, color),
        }
        painted = painted.union(icon);
    }
    ui.painter().galley(at, g, color);
    painted
}

/// Line-art icon of an activity type in the square `rect`: a controller (playing, also unknown
/// types), broadcast waves (streaming), beamed notes (listening), an eye (watching), a trophy
/// (competing).
pub(super) fn paint_activity_icon(p: &egui::Painter, kind: u8, rect: Rect, color: Color32) {
    let (c, s) = (rect.center(), rect.width());
    let stroke = Stroke::new((0.1 * s).max(1.2), color);
    let at = |x: f32, y: f32| c + vec2(x * s, y * s);
    let arc = |center: egui::Pos2, r: f32, from: f32, to: f32| {
        let pts = (0..=12).map(|i| center + r * s * Vec2::angled(from + (to - from) * i as f32 / 12.0)).collect();
        p.add(Shape::line(pts, stroke));
    };
    match kind {
        1 => {
            p.circle_filled(c, 0.1 * s, color);
            for r in [0.24, 0.42] {
                arc(c, r, PI - 0.7, PI + 0.7);
                arc(c, r, -0.7, 0.7);
            }
        }
        2 => {
            let (h1, h2) = (at(-0.2, 0.28), at(0.26, 0.2));
            for h in [h1, h2] {
                p.circle_filled(h, 0.14 * s, color);
            }
            let (t1, t2) = (at(-0.08, -0.36), at(0.38, -0.44));
            p.line_segment([pos2(t1.x, h1.y), t1], stroke);
            p.line_segment([pos2(t2.x, h2.y), t2], stroke);
            let beam = vec2(0.0, 0.16 * s);
            p.add(Shape::convex_polygon(vec![t1, t2, t2 + beam, t1 + beam], color, Stroke::NONE));
        }
        3 => {
            let lid = |sign: f32| (0..=16).map(move |i| {
                let t = i as f32 / 16.0;
                at(0.46 * (2.0 * t - 1.0), sign * 0.28 * (t * PI).sin())
            });
            p.add(Shape::closed_line(lid(-1.0).chain(lid(1.0).rev().skip(1).take(15)).collect(), stroke));
            p.circle_filled(c, 0.14 * s, color);
        }
        5 => {
            let top = c.y - 0.38 * s;
            let cup = (0..=12).map(|i| pos2(c.x + 0.26 * s * (PI * i as f32 / 12.0).cos(), top + 0.42 * s * (PI * i as f32 / 12.0).sin()));
            p.add(Shape::convex_polygon(cup.collect(), color, Stroke::NONE));
            arc(at(-0.26, -0.22), 0.12, FRAC_PI_2, 3.0 * FRAC_PI_2);
            arc(at(0.26, -0.22), 0.12, -FRAC_PI_2, FRAC_PI_2);
            p.line_segment([at(0.0, 0.0), at(0.0, 0.28)], stroke);
            p.rect_filled(Rect::from_min_max(at(-0.2, 0.26), at(0.2, 0.4)), 0, color);
        }
        _ => {
            let body = Rect::from_center_size(at(0.0, 0.04), vec2(0.92 * s, 0.56 * s));
            p.rect_stroke(body, CornerRadius::same((0.26 * s) as u8), stroke, egui::StrokeKind::Inside);
            let d = at(-0.2, 0.04);
            p.line_segment([d - vec2(0.12 * s, 0.0), d + vec2(0.12 * s, 0.0)], stroke);
            p.line_segment([d - vec2(0.0, 0.12 * s), d + vec2(0.0, 0.12 * s)], stroke);
            for b in [at(0.17, -0.02), at(0.27, 0.1)] {
                p.circle_filled(b, (0.06 * s).max(0.9), color);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel / DM list
// ---------------------------------------------------------------------------

/// Channel list of the selected guild, or the DM list when no guild is selected, on the card
/// `paint_card` draws. Its bottom is left free for the floating user panel.
pub fn channel_list(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let edge = line(&cx.theme.colors);
    let guild = cx.state.selected_guild;
    let title = guild.map(|g| cx.state.guild(g).map(|g| g.name.clone()).unwrap_or_default());

    // No fill: the card's rounded corner shows through.
    egui::Panel::top("channel_header")
        .exact_size(HEADER_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new())
        .show(ui, |ui| {
            let rect = ui.max_rect();
            match guild.zip(title) {
                Some((gid, title)) => server_menu::server_header(ui, cx, gid, title, rect),
                None => dm_search(ui, cx, rect),
            }
            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, edge);
        });
    user_panel_space(ui, cx, "channel_list_panel_space");

    ui.spacing_mut().item_spacing.y = 2.0;
    let area = egui::ScrollArea::vertical()
        .id_salt(("channels", guild))
        .auto_shrink(false)
        .content_margin(egui::Margin { left: 0, right: 0, top: 12, bottom: 12 });
    match guild {
        Some(gid) => {
            area.show(ui, |ui| guild_channels(ui, cx, gid));
        }
        None => dm_list(ui, cx, area.content_margin(egui::Margin { left: 0, right: 0, top: 0, bottom: 12 })),
    }
}

/// DM home header: a field that filters the DM list by name.
fn dm_search(ui: &mut egui::Ui, cx: &mut UiCx<'_>, rect: Rect) {
    let c = &cx.theme.colors;
    let mut frame = egui::Frame::new()
        .fill(c.guild_bar.to_color32())
        .corner_radius(cx.theme.layout.corner_radius)
        .inner_margin(egui::Margin::symmetric(8, 5));
    if c.guild_bar == c.sidebar {
        frame = frame.stroke(line(c));
    }
    let edit = egui::TextEdit::singleline(&mut cx.view.dm_filter)
        .hint_text("Find a conversation")
        .font(FontId::proportional(cx.theme.font.size - 1.0))
        .frame(frame)
        .desired_width(f32::INFINITY);
    ui.put(Rect::from_center_size(rect.center(), vec2(rect.width() - 20.0, 28.0)), edit);
}

fn guild_channels(ui: &mut egui::Ui, cx: &mut UiCx<'_>, guild_id: Id) {
    let mut items = cx.state.sidebar_channels_with(guild_id, crate::plugins::sidebar::show_hidden());
    if cx.view.hide_muted.contains(&guild_id) {
        drop_muted(cx.state, &mut items);
    }
    if items.is_empty() {
        empty_state(ui, cx, 12.0, "No visible channels");
        return;
    }
    // Parent -> its threads listed under it (threads.rs draws them).
    let threads = cx.state.sidebar_threads(guild_id);
    let mut collapsed = false;
    for (i, item) in items.into_iter().enumerate() {
        match item {
            SidebarItem::Category { id, name } => {
                if i > 0 {
                    ui.add_space(12.0); // + item spacing = 16 between sections
                }
                collapsed = cx.view.collapsed_categories.contains(&id);
                let resp = category_row(ui, cx, id, &name, collapsed);
                resp.context_menu(|ui| server_menu::category_menu(ui, cx, guild_id, id));
                if resp.clicked() {
                    collapsed = !collapsed;
                    if collapsed {
                        cx.view.collapsed_categories.insert(id);
                    } else {
                        cx.view.collapsed_categories.remove(&id);
                    }
                }
            }
            SidebarItem::Channel { id } => {
                // Collapsed categories still show the open channel, channels needing attention
                // and occupied voice channels.
                let occupied = cx.state.voice_states.contains_key(&id);
                let keep = cx.state.selected_channel == Some(id)
                    || occupied
                    || cx.state.mention_count(id) > 0
                    || (cx.state.is_unread(id) && !cx.state.is_channel_muted(id));
                if !collapsed || keep {
                    channel_row(ui, cx, id);
                    if occupied {
                        voice_users(ui, cx, guild_id, id);
                    }
                }
                if let Some(t) = threads.get(&id).filter(|_| !collapsed || keep) {
                    threads::sidebar_rows(ui, cx, id, t);
                }
            }
        }
    }
}

/// "Hide Muted Channels": muted channels and categories go, unless open or mentioning me.
fn drop_muted(s: &AppState, items: &mut Vec<SidebarItem>) {
    items.retain(|it| {
        let (SidebarItem::Category { id, .. } | SidebarItem::Channel { id }) = it;
        !s.is_channel_muted(*id) || s.selected_channel == Some(*id) || s.mention_count(*id) > 0
    });
}

/// Category header: its name in the case given, after a 12 px chevron that turns from down
/// (open) to right (collapsed) in 0.1 s.
fn category_row(ui: &mut egui::Ui, cx: &UiCx<'_>, id: Id, name: &str, collapsed: bool) -> egui::Response {
    let c = &cx.theme.colors;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let resp = ui.interact(row, ui.make_persistent_id(("category", id)), Sense::click());
    let color = if resp.hovered() { c.text } else { c.text_muted }.to_color32();
    let open = ui.ctx().animate_bool_with_time(resp.id.with("open"), !collapsed, 0.1);
    let turn = egui::emath::Rot2::from_angle((open - 1.0) * FRAC_PI_2);
    let center = pos2(row.left() + PAD - 2.0, row.center().y);
    let pts = [vec2(-4.0, -2.0), vec2(0.0, 2.0), vec2(4.0, -2.0)].map(|v| center + turn * v);
    ui.painter().add(Shape::line(pts.to_vec(), Stroke::new(1.5, color)));
    let left = row.left() + PAD + 8.0;
    let g = one_line(ui, name, bold(cx.theme.font.size - 2.0), color, row.right() - PAD - left);
    ui.painter().galley(pos2(left, row.center().y - g.size().y / 2.0), g, color);
    resp.widget_info(|| WidgetInfo::selected(Role::DisclosureTriangle, true, !collapsed, name));
    resp
}

/// Prefix glyph per channel type (monochrome emoji glyphs render as plain text). Announcement
/// channels get the painted megaphone instead (`channel_row`): the emoji smudges at body size.
fn channel_glyph(kind: u8) -> &'static str {
    match kind {
        ct::GUILD_VOICE => "🔊",
        ct::GUILD_STAGE_VOICE => "🎙",
        ct::GUILD_FORUM => "💬",
        ct::GUILD_MEDIA => "🖼",
        ct::GUILD_DIRECTORY => "📁",
        _ => "#",
    }
}

/// Row background / unread pill shared by channel, DM and voice-user rows. Returns the fill
/// painted behind the row (transparent when none).
fn row_chrome(ui: &egui::Ui, cx: &UiCx<'_>, bg_slot: egui::layers::ShapeIdx, row: Rect, selected: bool, hovered: bool, unread: bool) -> Color32 {
    let c = &cx.theme.colors;
    let (selected_fill, hover_fill) = row_fills(ui, c);
    let fill = if selected {
        selected_fill
    } else if hovered {
        hover_fill
    } else {
        Color32::TRANSPARENT
    };
    let cr = CornerRadius::from(cx.theme.layout.corner_radius);
    ui.painter().set(bg_slot, Shape::rect_filled(row.shrink2(vec2(ROW_INSET, 0.0)), cr, fill));
    if unread {
        let pill = Rect::from_min_size(pos2(row.left(), row.center().y - 4.0), vec2(4.0, 8.0));
        ui.painter().rect_filled(pill, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, c.text.to_color32());
    }
    fill
}

/// (selected, hovered) fills of rows on the sidebar color. Selected is the theme's resting
/// button fill; hover sits halfway, so it shows in every theme (`message_hover` is tuned for
/// the message background and nearly vanishes on the light sidebar).
pub(super) fn row_fills(ui: &egui::Ui, c: &ThemeColors) -> (Color32, Color32) {
    let selected = ui.visuals().widgets.inactive.weak_bg_fill;
    (selected, c.sidebar.to_color32().lerp_to_gamma(selected, 0.55))
}

/// Name color of a channel/DM row: dim when muted, bright when it wants attention.
fn row_text_color(c: &ThemeColors, dim: bool, bright: bool) -> Color32 {
    if dim {
        c.text_muted.to_color32().gamma_multiply(0.55)
    } else if bright {
        c.text.to_color32()
    } else {
        c.text_muted.to_color32()
    }
}

/// One channel. A click on a voice/stage channel joins it (the hover chat button opens its
/// text chat); the connected one has a green bar on its left.
fn channel_row(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id) {
    // ShowHiddenChannels: a locked, unselectable row.
    if crate::plugins::sidebar::hidden_row(ui, cx, id, ROW_H) {
        return;
    }
    let Some(ch) = cx.state.channels.get(&id) else { return };
    let (kind, guild_id) = (ch.kind, ch.guild_id);
    // Forum / media channels open their post list (forum.rs).
    let supported = ch.is_text_chat() || matches!(kind, ct::GUILD_FORUM | ct::GUILD_MEDIA);
    let voice = matches!(kind, ct::GUILD_VOICE | ct::GUILD_STAGE_VOICE);
    let name = ch.name.clone().unwrap_or_else(|| cx.state.channel_name(id));
    let selected = cx.state.selected_channel == Some(id);
    let connected = voice && cx.state.voice.channel_id == Some(id);
    let muted = cx.state.is_channel_muted(id);
    let unread = supported && !muted && !selected && cx.state.is_unread(id);
    let mentions = if supported { cx.state.mention_count(id) } else { 0 };

    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let bg_slot = ui.painter().add(Shape::Noop);
    let resp = ui.interact(row, ui.make_persistent_id(("channel", id)), if supported { Sense::click() } else { Sense::hover() });
    // Also while the pointer is on the chat button (a widget on top of the row).
    let hovered = supported && ui.rect_contains_pointer(row);
    row_chrome(ui, cx, bg_slot, row, selected, hovered, unread);

    let c = &cx.theme.colors;
    let size = cx.theme.font.size;
    if connected {
        let bar = Rect::from_min_size(pos2(row.left() + ROW_INSET, row.top() + 6.0), vec2(2.0, ROW_H - 12.0));
        ui.painter().rect_filled(bar, 1, presence_color(Presence::Online, c));
    }
    let color = row_text_color(c, !supported || muted, selected || unread || hovered);
    // The open channel's glyph carries the accent (selection indicator).
    let glyph_color = if selected { c.accent.to_color32() } else { color };
    // 20 px icon slot after 8 px of row padding, then the name.
    let glyph = pos2(row.left() + ROW_INSET + 18.0, row.center().y);
    if matches!(kind, ct::GUILD_ANNOUNCEMENT | ct::ANNOUNCEMENT_THREAD) {
        paint_megaphone(ui.painter(), glyph, 18.0, glyph_color);
    } else {
        ui.painter().text(glyph, Align2::CENTER_CENTER, channel_glyph(kind), FontId::new(size, crate::theme::icon_family()), glyph_color);
    }
    let mut right = row.right() - ROW_INSET - 8.0;
    let mut open_chat = false;
    if voice && hovered {
        let pal = Pal::new(cx.theme);
        let r = Rect::from_center_size(pos2(right - 10.0, row.center().y), Vec2::splat(20.0));
        // Not allocated: the row already holds the space.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
        open_chat = icon_button(&mut child, &pal, "💬", "Open chat", 20.0, false, pal.text).clicked();
        right -= 24.0;
    } else if mentions > 0 {
        right -= badge(ui, c, pos2(right, row.center().y), mentions, None) + 8.0;
    }
    right -= crate::plugins::sidebar::typing_dots(ui, cx.state, id, pos2(right, row.center().y));
    let font = if unread { bold(size) } else { FontId::proportional(size) };
    let text_left = row.left() + ROW_INSET + 34.0;
    let g = one_line(ui, name.as_str(), font, color, right - text_left);
    ui.painter().galley(pos2(text_left, row.center().y - g.size().y / 2.0), g, color);

    resp.widget_info(|| WidgetInfo::selected(Role::Button, supported, selected || connected, &name));
    resp.context_menu(|ui| channel_menu(ui, cx, id, guild_id, voice.then_some(connected)));
    if !supported {
        resp.on_hover_text("Not supported yet");
    } else if voice && !connected && !open_chat && resp.clicked() {
        cx.cmd.send(Command::JoinVoice { guild_id, channel_id: id });
    } else if open_chat || resp.clicked() {
        cx.state.select_channel(id);
        cx.view.settings_open = false;
        // The call view opens instead of the chat: show its chat column.
        cx.view.call_ui.chat |= open_chat && connected;
    }
}

/// Channel menu. `voice` = Some(connected there) for voice channels: join / disconnect and
/// their text chat.
fn channel_menu(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id, guild_id: Option<Id>, voice: Option<bool>) {
    menu_setup(ui);
    let c = &cx.theme.colors;
    let (text, danger) = (c.text.to_color32(), c.danger.to_color32());
    if ui.add_enabled_ui(cx.state.is_unread(id), |ui| menu_item(ui, "Mark As Read", MenuIcon::Glyph("✔"), text)).inner {
        mark_read(cx, [id]);
    }
    if let Some(connected) = voice {
        ui.separator();
        if connected {
            if menu_item(ui, "Disconnect", MenuIcon::Glyph("📞"), danger) {
                cx.cmd.send(Command::LeaveVoice);
            }
        } else if menu_item(ui, "Join Voice", MenuIcon::Glyph("🔊"), text) {
            cx.cmd.send(Command::JoinVoice { guild_id, channel_id: id });
        }
        if menu_item(ui, "Open Chat", MenuIcon::Glyph("💬"), text) {
            cx.state.select_channel(id);
            cx.view.settings_open = false;
            cx.view.call_ui.chat |= connected;
        }
    }
    server_menu::channel_menu_items(ui, cx, id);
    ui.separator();
    if menu_item(ui, "Copy Link", MenuIcon::Link, text) {
        let place = guild_id.map_or_else(|| "@me".to_owned(), |g| g.0.to_string());
        ui.ctx().copy_text(format!("https://discord.com/channels/{place}/{}", id.0));
    }
    if cx.state.account.settings.developer_mode() && menu_item(ui, "Copy Channel ID", MenuIcon::Glyph("🆔"), text) {
        ui.ctx().copy_text(id.0.to_string());
    }
}

/// Someone in a voice channel, as listed under it.
struct VoiceUser {
    id: Id,
    name: String,
    url: Option<String>,
    muted: bool,
    deaf: bool,
    /// Muted/deafened by a moderator (marks in the danger color).
    by_server: bool,
    /// Go Live / camera on.
    live: bool,
    video: bool,
}

/// The people in a voice channel, sorted by name, as 24 px avatar rows under its row.
fn voice_users(ui: &mut egui::Ui, cx: &mut UiCx<'_>, guild_id: Id, channel_id: Id) {
    let Some(states) = cx.state.voice_states.get(&channel_id) else { return };
    let mut people: Vec<VoiceUser> = states
        .values()
        .map(|v| {
            let user = cx.state.users.get(&v.user_id);
            VoiceUser {
                id: v.user_id,
                name: user.map_or_else(|| "Unknown user".into(), |u| cx.state.member_display_name(Some(guild_id), u, v.member.as_ref())),
                url: user.map(|u| super::images::user_avatar_url(Some(guild_id), u, v.member.as_ref(), 48, false)),
                muted: v.self_mute || v.mute || v.self_deaf || v.deaf,
                deaf: v.self_deaf || v.deaf,
                by_server: v.mute || v.deaf,
                live: v.self_stream,
                video: v.self_video,
            }
        })
        .collect();
    people.sort_by_cached_key(|p| (p.name.to_lowercase(), p.id));
    for p in &people {
        voice_user_row(ui, cx, guild_id, p);
    }
}

fn voice_user_row(ui: &mut egui::Ui, cx: &mut UiCx<'_>, guild_id: Id, u: &VoiceUser) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), VOICE_ROW_H), Sense::hover());
    let bg_slot = ui.painter().add(Shape::Noop);
    let avatar = Rect::from_min_size(pos2(row.left() + 36.0, row.center().y - 12.0), Vec2::splat(24.0));
    match &u.url {
        Some(url) => user_avatar_at(ui, cx.images, url, avatar, false, 1.0, (u.id, &u.name)),
        None => paint_initial(ui.painter(), avatar, 12.0, u.id.0, &u.name, ui.visuals().dark_mode),
    }
    let who = cx.state.users.get(&u.id).or(cx.state.me.as_ref().filter(|m| m.id == u.id));
    let deco = who.and_then(|w| crate::overrides::effective_for(w, None).decoration);
    super::images::decoration_at(ui, cx.images, avatar, deco.as_ref(), false, 1.0);
    // Registered after the avatar widget so the whole row stays clickable.
    let resp = ui.interact(row, ui.make_persistent_id(("voice_user", u.id)), Sense::click());
    let hovered = resp.hovered();
    // The hover fill starts 8 px left of the avatar.
    let fill = row_chrome(ui, cx, bg_slot, row.with_min_x(row.left() + 20.0), false, hovered, false);

    let c = &cx.theme.colors;
    let size = cx.theme.font.size;
    if cx.state.voice.speaking.contains(&u.id) {
        ui.painter().circle_stroke(avatar.center(), 13.5, Stroke::new(2.0, presence_color(Presence::Online, c)));
    }
    let bg = if fill == Color32::TRANSPARENT { c.sidebar.to_color32() } else { fill };
    let (muted, mark) = (c.text_muted.to_color32(), if u.by_server { c.danger } else { c.text_muted }.to_color32());
    let (mut right, cy) = (row.right() - ROW_INSET - 6.0, row.center().y);
    // Right to left, 4 px apart: LIVE pill, camera, deafened, muted.
    if u.live {
        let g = ui.painter().layout_no_wrap("LIVE".into(), bold(11.0), Color32::WHITE);
        let pill = Rect::from_min_max(pos2(right - g.size().x - 12.0, cy - 8.0), pos2(right, cy + 8.0));
        ui.painter().rect_filled(pill, 8, c.danger.to_color32());
        ui.painter().galley(pill.center() - g.size() / 2.0, g, Color32::WHITE);
        right = pill.left() - 4.0;
    }
    for (on, icon, color, slashed) in [(u.video, Icon::Camera, muted, false), (u.deaf, Icon::Headphones, mark, true), (u.muted, Icon::Mic, mark, true)] {
        if on {
            let r = Rect::from_min_max(pos2(right - 16.0, cy - 8.0), pos2(right, cy + 8.0));
            paint_icon(ui.painter(), icon, r, color, slashed, bg);
            right -= 20.0;
        }
    }
    let color = if hovered { c.text } else { c.text_muted }.to_color32();
    let text_left = avatar.right() + 8.0;
    let g = one_line(ui, u.name.as_str(), FontId::proportional(size - 1.0), color, right - 4.0 - text_left);
    ui.painter().galley(pos2(text_left, cy - g.size().y / 2.0), g, color);

    let state = match (u.deaf, u.muted) {
        (true, _) => ", deafened",
        (false, true) => ", muted",
        _ => "",
    };
    let (live, video) = (if u.live { ", live" } else { "" }, if u.video { ", camera on" } else { "" });
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, format!("{}{state}{live}{video}", u.name)));
    let anchor = pos2(row.right() + 8.0, row.top());
    resp.context_menu(|ui| user_menu(ui, cx, u.id, Some(guild_id), anchor));
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        cx.view.open_profile(u.id, Some(guild_id), anchor);
    }
}

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Mic,
    Headphones,
    Gear,
    /// A handset lying down (leave the call).
    Hangup,
    /// Video camera (camera on).
    Camera,
    /// A monitor with an up arrow (screen share).
    Screen,
}

/// Icon in the square `rect` (vector-painted, so no font has to carry it); `slashed` crosses
/// it out like Discord's "off" icons: a diagonal stroke with a gap in `bg` on both sides.
pub(super) fn paint_icon(p: &egui::Painter, icon: Icon, rect: Rect, color: Color32, slashed: bool, bg: Color32) {
    let (c, s) = (rect.center(), rect.width());
    let stroke = Stroke::new((0.09 * s).max(1.4), color);
    // Half circle (`from` = 0: lower half, PI: upper half, y pointing down).
    let arc = |center: egui::Pos2, r: f32, from: f32| {
        let pts = (0..=12).map(|i| center + r * egui::Vec2::angled(from + std::f32::consts::PI * i as f32 / 12.0)).collect();
        p.add(Shape::line(pts, stroke));
    };
    match icon {
        Icon::Mic => {
            p.rect_filled(Rect::from_center_size(c + vec2(0.0, -0.15 * s), vec2(0.32 * s, 0.5 * s)), 0.16 * s, color);
            arc(c + vec2(0.0, -0.08 * s), 0.3 * s, 0.0);
            p.line_segment([c + vec2(0.0, 0.22 * s), c + vec2(0.0, 0.42 * s)], stroke);
            p.line_segment([c + vec2(-0.16 * s, 0.42 * s), c + vec2(0.16 * s, 0.42 * s)], stroke);
        }
        Icon::Headphones => {
            arc(c + vec2(0.0, 0.06 * s), 0.38 * s, std::f32::consts::PI);
            for x in [-0.44, 0.24] {
                p.rect_filled(Rect::from_min_size(c + vec2(x * s, 0.02 * s), vec2(0.2 * s, 0.36 * s)), 0.07 * s, color);
            }
        }
        Icon::Gear => {
            p.text(c, Align2::CENTER_CENTER, "⚙", FontId::new(s, crate::theme::icon_family()), color);
        }
        Icon::Hangup => {
            let band = (0..=12).map(|i| c + vec2(0.0, 0.3 * s) + 0.45 * s * egui::Vec2::angled(PI * (1.2 + 0.6 * i as f32 / 12.0))).collect();
            p.add(Shape::line(band, Stroke::new(0.18 * s, color)));
            for x in [-0.36, 0.36] {
                p.rect_filled(Rect::from_center_size(c + vec2(x * s, 0.1 * s), vec2(0.26 * s, 0.2 * s)), 0.08 * s, color);
            }
        }
        Icon::Camera => {
            p.rect_filled(Rect::from_min_size(c + vec2(-0.46 * s, -0.24 * s), vec2(0.62 * s, 0.48 * s)), 0.1 * s, color);
            p.add(Shape::convex_polygon(vec![c + vec2(0.2 * s, 0.0), c + vec2(0.46 * s, -0.2 * s), c + vec2(0.46 * s, 0.2 * s)], color, Stroke::NONE));
        }
        Icon::Screen => {
            let screen = Rect::from_center_size(c + vec2(0.0, -0.1 * s), vec2(0.86 * s, 0.6 * s));
            p.rect_stroke(screen, 0.08 * s, stroke, egui::StrokeKind::Middle);
            p.line_segment([c + vec2(-0.2 * s, 0.4 * s), c + vec2(0.2 * s, 0.4 * s)], stroke);
            p.line_segment([c + vec2(0.0, 0.2 * s), c + vec2(0.0, 0.4 * s)], stroke);
            let tip = c + vec2(0.0, -0.24 * s);
            p.line_segment([c + vec2(0.0, 0.06 * s), tip], stroke);
            p.add(Shape::line(vec![tip + vec2(-0.12 * s, 0.12 * s), tip, tip + vec2(0.12 * s, 0.12 * s)], stroke));
        }
    }
    if slashed {
        let d = vec2(s, -s) * 0.42;
        let (a, b) = (c + d, c - d);
        // A narrow gap: a wide one erases a whole headphone cup.
        p.line_segment([a, b], Stroke::new(stroke.width + 1.5, bg));
        p.line_segment([a, b], stroke);
    }
}

/// DM rows under a "Direct Messages" header, filtered by the header field; closed DMs are
/// left out until a new message arrives in them.
fn dm_list(ui: &mut egui::Ui, cx: &mut UiCx<'_>, area: egui::ScrollArea) {
    // Opening a closed DM (notification, quick restore of the last channel) lists it again.
    if let Some(open) = cx.state.selected_channel {
        cx.view.hidden_dms.remove(&open);
    }
    let query = cx.view.dm_filter.trim().to_lowercase();
    let ids: Vec<Id> = cx
        .state
        .dm_order
        .iter()
        .copied()
        .filter(|&id| dm_listed(cx.state, &cx.view.hidden_dms, id))
        .filter(|&id| query.is_empty() || cx.state.channel_name(id).to_lowercase().contains(&query))
        .collect();
    if ids.is_empty() {
        friends_row(ui, cx);
        dm_header(ui, cx);
        empty_state(ui, cx, 24.0, if query.is_empty() { "No direct messages yet" } else { "No matches" });
        return;
    }
    // Rows are uniform (Friends is row 0, the header row 1): only the visible ones are laid out.
    area.show_rows(ui, DM_ROW_H, ids.len() + 2, |ui, range| {
        for i in range {
            match i {
                0 => friends_row(ui, cx),
                1 => dm_header(ui, cx),
                i => dm_row(ui, cx, ids[i - 2]),
            }
        }
    });
}

/// "Friends" on top of the DM list: opens the Friends page (friends.rs); pending requests as a
/// badge.
fn friends_row(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), DM_ROW_H), Sense::hover());
    let bg_slot = ui.painter().add(Shape::Noop);
    let resp = ui.interact(row, ui.make_persistent_id("friends_row"), Sense::click());
    let selected = super::friends::is_open(cx.state);
    row_chrome(ui, cx, bg_slot, row, selected, resp.hovered(), false);
    let c = &cx.theme.colors;
    let color = row_text_color(c, false, selected || resp.hovered());
    let size = cx.theme.font.size;
    super::friends::paint_people(ui.painter(), Rect::from_center_size(pos2(row.left() + ROW_INSET + 24.0, row.center().y), Vec2::splat(24.0)), color);
    let mut right = row.right() - ROW_INSET - 8.0;
    let pending = cx.state.pending_count() as u32;
    if pending > 0 {
        right -= badge(ui, c, pos2(right, row.center().y), pending, None) + 8.0;
    }
    let left = row.left() + ROW_INSET + 52.0;
    let g = one_line(ui, "Friends", FontId::proportional(size), color, right - left);
    ui.painter().galley(pos2(left, row.center().y - g.size().y / 2.0), g, color);
    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, "Friends"));
    if resp.clicked() {
        super::friends::open(cx.state);
        cx.view.settings_open = false;
    }
}

/// Whether a DM is listed: never closed here, or a message arrived after it was closed.
fn dm_listed(state: &AppState, hidden: &HashMap<Id, Option<Id>>, id: Id) -> bool {
    hidden.get(&id).is_none_or(|&closed_at| state.channels.get(&id).and_then(|c| c.last_message_id) > closed_at)
}

/// "Direct Messages" with a "+" (Create DM: the group picker in friends.rs).
fn dm_header(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), DM_ROW_H), Sense::hover());
    let color = cx.theme.colors.text_muted.to_color32();
    let g = one_line(ui, "Direct Messages", bold(cx.theme.font.size - 2.0), color, row.width() - 2.0 * PAD - 24.0);
    let text_y = row.bottom() - g.size().y - 6.0;
    let center_y = text_y + g.size().y / 2.0;
    ui.painter().galley(pos2(row.left() + PAD + 2.0, text_y), g, color);
    let pal = Pal::new(cx.theme);
    let r = Rect::from_center_size(pos2(row.right() - PAD - 4.0, center_y), Vec2::splat(24.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
    if icon_button(&mut child, &pal, "+", "Create DM", 24.0, false, pal.text).clicked() {
        super::friends::open_group_dm(cx);
    }
}

/// A DM's picture in `rect`: the other user's avatar, or a group DM's icon (or its initial).
/// Returns the other user's presence (1:1 DMs).
pub(super) fn dm_avatar_at(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id, rect: Rect) -> Option<Presence> {
    let ch = cx.state.channels.get(&id)?;
    let group = ch.kind == ct::GROUP_DM;
    let first = ch.recipient_ids.first().and_then(|u| cx.state.users.get(u));
    let (url, who) = match (group, first) {
        (true, _) => (ch.icon.as_deref().map(|h| cdn::group_dm_icon_url(id, h, 64)), (id, cx.state.channel_name(id))),
        (false, Some(u)) => (Some(super::images::user_avatar_url(None, u, None, 64, false)), (u.id, u.display_name().to_owned())),
        (false, None) => (None, (id, cx.state.channel_name(id))),
    };
    let presence = ch.recipient_ids.first().filter(|_| !group).map(|&u| user_presence(cx.state, u));
    let dark = ui.visuals().dark_mode;
    match &url {
        Some(url) => user_avatar_at(ui, cx.images, url, rect, false, 1.0, (who.0, &who.1)),
        None => paint_initial(ui.painter(), rect, rect.width() / 2.0, who.0.0, &who.1, dark),
    }
    // Placeholders under 20 px are bare dots; the 16 px title-bar icon still shows the letter.
    let px = (rect.width() * ui.ctx().pixels_per_point()).ceil() as u32;
    if rect.height() < 20.0 && url.is_none_or(|u| cx.images.get(ui.ctx(), &u, px, false).is_none()) {
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, initial(&who.1), bold(9.0), initial_colors(who.0.0, dark).1);
    }
    presence
}

/// Paints `name` (in `color`) at x `left_center.x`, centred on its y; with a status line (meta
/// size, muted) under it, the pair is centred instead. `max_w` bounds the status line.
/// Returns the name's rect (guild tag chips go after it) and the right end of the text.
pub(super) fn name_and_status(ui: &egui::Ui, cx: &mut UiCx<'_>, name: Arc<egui::Galley>, color: Color32, status: Option<&StatusText>, left_center: egui::Pos2, max_w: f32) -> (Rect, f32) {
    let (x, cy) = (left_center.x, left_center.y);
    let Some(st) = status else {
        let at = Rect::from_min_size(pos2(x, cy - name.size().y / 2.0), name.size());
        ui.painter().galley(at.min, name, color);
        return (at, at.right());
    };
    let font = FontId::proportional(cx.theme.font.size - 2.0);
    let sub_h = ui.ctx().fonts_mut(|f| f.row_height(&font));
    let (name_h, muted) = (name.size().y, cx.theme.colors.text_muted.to_color32());
    let top = cy - (name_h + sub_h) / 2.0;
    let at = Rect::from_min_size(pos2(x, top), name.size());
    ui.painter().galley(at.min, name, color);
    let sub = paint_status_text(ui, cx.images, pos2(x, top + name_h), st, egui::TextFormat::simple(font, muted), 16.0, max_w, 1);
    (at, at.right().max(sub.right()))
}

fn dm_row(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), DM_ROW_H), Sense::hover());
    let Some(ch) = cx.state.channels.get(&id) else { return };
    let group = ch.kind == ct::GROUP_DM;
    let name = cx.state.channel_name(id);
    let members = ch.recipient_ids.len() + 1;
    let other = ch.recipient_ids.first().copied().filter(|_| !group);
    let selected = cx.state.selected_channel == Some(id);
    let muted = cx.state.is_channel_muted(id);
    let unread = !muted && !selected && cx.state.is_unread(id);
    let mentions = cx.state.mention_count(id);

    let bg_slot = ui.painter().add(Shape::Noop);
    // Also while the pointer is on the close button (a widget on top of the row).
    let hot = ui.rect_contains_pointer(row);
    // 1:1 DMs: the other user's cosmetics (ui/cosmetics.rs), the nameplate under the row.
    let peer = other.and_then(|u| cx.state.users.get(&u));
    let (fx, tag) = peer.map(|u| (crate::overrides::effective_for(u, None), super::cosmetics::guild_tag(u, None).cloned())).unwrap_or_default();
    let fx_anim = super::cosmetics::animates(cx, hot);
    // Filled once the text's extent is known (light themes fade the art in after it).
    let plate = ui.painter().add(Shape::Noop);
    let avatar = Rect::from_min_size(pos2(row.left() + ROW_INSET + 8.0, row.center().y - 16.0), Vec2::splat(32.0));
    dm_avatar_at(ui, cx, id, avatar);
    super::images::decoration_at(ui, cx.images, avatar, fx.decoration.as_ref(), fx_anim, 1.0);
    // Registered after the avatar widget so the whole row stays clickable.
    let resp = ui.interact(row, ui.make_persistent_id(("dm", id)), Sense::click());
    let fill = row_chrome(ui, cx, bg_slot, row, selected, hot, unread);

    let c = &cx.theme.colors;
    let size = cx.theme.font.size;
    if let Some(u) = other {
        user_dot(ui.painter(), avatar, cx.state, u, c, if fill == Color32::TRANSPARENT { c.sidebar.to_color32() } else { fill });
    }
    let color = row_text_color(c, muted, selected || unread || hot);
    let chip_under = if fill == Color32::TRANSPARENT { c.sidebar.to_color32() } else { fill };
    let mut right = row.right() - ROW_INSET - 8.0;
    let mut close = false;
    if hot {
        let x_rect = Rect::from_min_max(pos2(right - 20.0, row.center().y - 10.0), pos2(right, row.center().y + 10.0));
        let x = ui.interact(x_rect, ui.make_persistent_id(("dm_close", id)), Sense::click());
        let x_color = if x.hovered() { c.text.to_color32() } else { c.text_muted.to_color32().gamma_multiply(0.8) };
        ui.painter().text(x_rect.center(), Align2::CENTER_CENTER, "×", FontId::proportional(size + 4.0), x_color);
        x.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Close DM"));
        close = x.on_hover_text("Close DM").clicked();
        right -= 24.0;
    } else if mentions > 0 {
        right -= badge(ui, c, pos2(right, row.center().y), mentions, None) + 8.0;
    }
    right -= super::dm_call::row_mark(ui, cx, id, pos2(right, row.center().y));
    right -= crate::plugins::sidebar::typing_dots(ui, cx.state, id, pos2(right, row.center().y));
    let text_left = avatar.right() + 12.0;
    let font = if unread { bold(size) } else { FontId::proportional(size) };
    let chip_w = tag.as_ref().map_or(0.0, |pg| super::cosmetics::tag_chip_width(ui, pg, size) + 4.0);
    let style = fx.name_style.as_ref().filter(|_| !muted);
    let title = super::cosmetics::name_galley(ui, name.as_str(), font, style, color, right - text_left - chip_w);
    // Group: its size; 1:1: the other user's custom status or activity.
    let sub = if group { Some((None, format!("{members} members"))) } else { other.and_then(|u| user_status_text(cx.state, u)) };
    let under = ui.painter().add(Shape::Noop);
    let (name_rect, text_end) = name_and_status(ui, cx, title.clone(), color, sub.as_ref(), pos2(text_left, row.center().y), right - text_left);
    ui.painter().set(under, super::cosmetics::name_underlay(name_rect.min, &title, style, ui.visuals().dark_mode));
    if let Some(np) = &fx.nameplate {
        let (radius, plate_rect, end) = (cx.theme.layout.corner_radius.round() as u8, row.shrink2(vec2(ROW_INSET, 0.0)), text_end.max(name_rect.right() + chip_w));
        let shape = super::cosmetics::nameplate(ui, cx.images, plate_rect, np, fx_anim, muted, radius, end);
        // Dark themes: a scrim keeps the status line off the art's bright spots.
        let scrim = (ui.visuals().dark_mode && !muted).then(|| super::cosmetics::plate_scrim(plate_rect, radius, chip_under, end, crate::theme::is_high_contrast(&cx.theme.colors)));
        ui.painter().set(plate, Shape::Vec(std::iter::once(shape).chain(scrim).collect()));
    }
    if let Some(pg) = &tag {
        let at = pos2(name_rect.right() + 4.0, name_rect.center().y);
        match fx.nameplate {
            // Opaque over the nameplate art.
            Some(_) => super::cosmetics::tag_chip_on(ui, cx.images, pg, at, size, color, chip_under),
            None => super::cosmetics::tag_chip(ui, cx.images, pg, at, size, color),
        };
    }

    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, &name));
    resp.context_menu(|ui| dm_menu(ui, cx, id, pos2(row.right() + 8.0, row.top())));
    if close {
        close_dm(cx, id);
    } else if resp.clicked() {
        cx.state.select_channel(id);
        cx.view.settings_open = false;
    }
}

/// Closes a DM here only: it comes back with its next message.
fn close_dm(cx: &mut UiCx<'_>, id: Id) {
    let last = cx.state.channels.get(&id).and_then(|c| c.last_message_id);
    cx.view.hidden_dms.insert(id, last);
    if cx.state.selected_channel == Some(id) {
        cx.state.selected_channel = None;
        cx.state.dirty = true;
    }
}

/// DM menu: mark read, the other user's profile, close (1:1 DMs), and their id (a group's
/// channel id).
fn dm_menu(ui: &mut egui::Ui, cx: &mut UiCx<'_>, id: Id, anchor: egui::Pos2) {
    menu_setup(ui);
    let text = cx.theme.colors.text.to_color32();
    let user = cx.state.channels.get(&id).filter(|c| c.kind == ct::DM).and_then(|c| c.recipient_ids.first().copied());
    if ui.add_enabled_ui(cx.state.is_unread(id), |ui| menu_item(ui, "Mark As Read", MenuIcon::Glyph("✔"), text)).inner {
        mark_read(cx, [id]);
    }
    ui.separator();
    if let Some(u) = user
        && menu_item(ui, "Profile", MenuIcon::Glyph("👤"), text)
    {
        cx.view.open_profile(u, None, anchor);
    }
    // Groups have Leave Group instead (friends.rs): one way out each.
    if user.is_some() && menu_item(ui, "Close DM", MenuIcon::Glyph("✖"), text) {
        close_dm(cx, id);
    }
    super::friends::dm_menu_items(ui, cx, id);
    let dev = cx.state.account.settings.developer_mode();
    if dev {
        ui.separator();
    }
    let (label, copy) = match user {
        Some(u) => ("Copy User ID", u),
        None => ("Copy Channel ID", id),
    };
    if dev && menu_item(ui, label, MenuIcon::Glyph("🆔"), text) {
        ui.ctx().copy_text(copy.0.to_string());
    }
}

// ---------------------------------------------------------------------------
// User panel
// ---------------------------------------------------------------------------

/// Background of the user panel: between the channel list and the server bar.
fn user_panel_fill(c: &ThemeColors) -> Color32 {
    c.sidebar.to_color32().lerp_to_gamma(c.guild_bar.to_color32(), 0.7)
}

/// Height of the floating user panel: the account row (two text lines grow with the font
/// size), plus the voice section and a hairline while in a call.
fn user_panel_h(cx: &UiCx<'_>) -> f32 {
    user_panel_height(cx.state, cx.theme)
}

/// `user_panel_h` without a `UiCx` (startup hooks).
pub(super) fn user_panel_height(state: &AppState, theme: &Theme) -> f32 {
    let buttons = if state.voice.connected { VOICE_BUTTONS_H } else { 0.0 };
    let voice = if state.voice.channel_id.is_some() { VOICE_PANEL_H + buttons + 1.0 } else { 0.0 };
    account_h(theme) + voice
}

fn account_h(theme: &Theme) -> f32 {
    ACCOUNT_H.max(3.0 * theme.font.size + 12.0)
}

/// Empty bottom panel that keeps a column's content above the floating user panel.
fn user_panel_space(ui: &mut egui::Ui, cx: &UiCx<'_>, salt: &str) {
    egui::Panel::bottom(salt)
        .exact_size(user_panel_h(cx) + 2.0 * PANEL_MARGIN)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new())
        .show(ui, |_| ());
}

/// The floating user panel, 8 px from the window's bottom-left corner over the server bar and
/// the channel list (`width` = both): the voice connection while in a call, then my account.
pub fn user_panel(ctx: &egui::Context, cx: &mut UiCx<'_>, width: f32) {
    let c = cx.theme.colors.clone();
    let h = user_panel_h(cx);
    let screen = ctx.content_rect();
    let pos = pos2(screen.left() + PANEL_MARGIN, screen.bottom() - PANEL_MARGIN - h);
    egui::Area::new(egui::Id::unique("user_panel")).order(egui::Order::Middle).fixed_pos(pos).show(ctx, |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(width - 2.0 * PANEL_MARGIN, h), Sense::hover());
        ui.painter().rect_filled(rect, PANEL_R, user_panel_fill(&c));
        let voice_h = h - account_h(cx.theme);
        if voice_h > 0.0 {
            let split = rect.top() + voice_h;
            voice_panel(ui, cx, rect.with_max_y(split - 1.0));
            ui.painter().hline(rect.x_range(), split - 0.5, line(&c));
        }
        account_row(ui, cx, rect.with_min_y(rect.top() + voice_h), rect.left_top());
        // Last: the outline stays over the nameplate's edge.
        ui.painter().rect_stroke(rect, PANEL_R, line(&c), egui::StrokeKind::Inside);
    });
}

/// "Voice Connected" (or "Connecting…", LIVE while I share) over "channel / server" (opens the
/// call view), and the disconnect button; once connected, Camera and Screen Share buttons under
/// them (lit while on: a click stops it).
fn voice_panel(ui: &mut egui::Ui, cx: &mut UiCx<'_>, rect: Rect) {
    let Some(ch) = cx.state.voice.channel_id else { return };
    let c = cx.theme.colors.clone();
    let row = Rect::from_min_size(rect.min + Vec2::splat(12.0), vec2(rect.width() - 24.0, 32.0));
    let sharing = cx.state.voice.video.screen_share.is_some();
    // Connected in status green, connecting in idle yellow; both ~2:1 on light panels, so
    // darkened there (~6:1 / ~5.6:1 on Paper Light).
    let (word, presence) = if cx.state.voice.connected { ("Voice Connected", Presence::Online) } else { ("Connecting…", Presence::Idle) };
    let word = &crate::plugins::sidebar::call_timer(ui.ctx(), cx.state, word);
    let color = presence_color(presence, &c);
    let color = if ui.visuals().dark_mode { color } else { color.lerp_to_gamma(Color32::BLACK, 0.5) };
    let guild = cx.state.channels.get(&ch).and_then(|x| x.guild_id).and_then(|g| cx.state.guild(g)).map(|g| g.name.clone());
    let place = match guild {
        Some(g) => format!("{} / {g}", cx.state.channel_name(ch)),
        None => cx.state.channel_name(ch),
    };
    let button = Rect::from_min_size(pos2(row.right() - 32.0, row.center().y - 16.0), Vec2::splat(32.0));
    let max_w = button.left() - 8.0 - row.left();
    let link_rect = Rect::from_min_max(pos2(row.left(), row.center().y), pos2(row.left() + max_w, row.bottom()));
    let link = ui.interact(link_rect, ui.make_persistent_id("voice_place"), Sense::click());
    let link_color = if link.hovered() { c.text } else { c.text_muted }.to_color32();
    let top_g = one_line(ui, word, bold(cx.theme.font.size - 1.0), color, max_w - if sharing { 44.0 } else { 0.0 });
    let sub_g = one_line(ui, place.as_str(), FontId::proportional(12.0), link_color, max_w);
    let top = row.center().y - (top_g.size().y + sub_g.size().y) / 2.0;
    let sub_y = top + top_g.size().y;
    if sharing {
        super::call::live_pill(ui.painter(), pos2(row.left() + top_g.size().x + 6.0, top + top_g.size().y / 2.0), c.danger.to_color32());
    }
    ui.painter().galley(pos2(row.left(), top), top_g, color);
    ui.painter().galley(pos2(row.left(), sub_y), sub_g, link_color);
    link.widget_info(|| WidgetInfo::labeled(Role::Link, true, &place));
    if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        cx.state.select_channel(ch);
        cx.view.settings_open = false;
    }

    let resp = ui.interact(button, ui.make_persistent_id("voice_disconnect"), Sense::click());
    let hot = resp.hovered() || resp.has_focus();
    let base = user_panel_fill(&c);
    let bg = if hot { base.blend(c.text.to_color32().gamma_multiply(0.08)) } else { base };
    ui.painter().rect_filled(button, CornerRadius::from(cx.theme.layout.corner_radius), bg);
    let glyph = if hot { c.danger } else { c.text_muted }.to_color32();
    paint_icon(ui.painter(), Icon::Hangup, button.shrink(7.0), glyph, false, bg);
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Disconnect"));
    if resp.on_hover_text("Disconnect").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        cx.cmd.send(Command::LeaveVoice);
    }

    if !cx.state.voice.connected {
        return;
    }
    // Two equal buttons, 8 px apart, on a resting tint so they read as buttons.
    let w = (row.width() - 8.0) / 2.0;
    let colors = (base.blend(c.text.to_color32().gamma_multiply(0.06)), c.text.to_color32(), c.danger.to_color32());
    let cam = cx.state.voice.video.camera_on;
    let camera = super::call::MediaButton { icon: Icon::Camera, tip: if cam { "Turn Off Camera" } else { "Turn On Camera" }, on: cam, red: false, slashed: false, enabled: true };
    // Lit from the Go Live click on (still connecting): a click cancels it.
    let share = cx.state.voice.video.sharing();
    let screen = super::call::MediaButton { icon: Icon::Screen, tip: if share { "Stop Streaming" } else { "Share Your Screen" }, on: share, red: false, slashed: false, enabled: true };
    let radius = cx.theme.layout.corner_radius;
    if camera.show(ui, Rect::from_min_size(pos2(row.left(), row.bottom() + 8.0), vec2(w, 32.0)), radius, "voice_camera", colors).clicked() {
        super::call::toggle_camera(cx);
    }
    if screen.show(ui, Rect::from_min_size(pos2(row.right() - w, row.bottom() + 8.0), vec2(w, 32.0)), radius, "voice_screen", colors).clicked() {
        super::call::toggle_share(cx);
    }
}

/// Corner radius of the floating user panel.
const PANEL_R: u8 = 8;
/// Microphone / headphones: the 32 px button and the ∨ of its device menu.
const GROUP_W: f32 = 50.0;

/// Me, like Discord's panel: my nameplate behind the whole row, avatar (decoration, status) over
/// name and a second line (an activity's icon • my custom status, else my status or "Playing X";
/// the connection while not connected; my username while hovered). A click opens the account
/// popout (my card with the status picker, profile.rs) above the panel, whose top-left is
/// `panel`. Then microphone and headphones, each with its device menu, and settings.
fn account_row(ui: &mut egui::Ui, cx: &mut UiCx<'_>, rect: Rect, panel: egui::Pos2) {
    let c = cx.theme.colors.clone();
    let size = cx.theme.font.size;
    let me = cx.state.me.clone();
    let cy = rect.center().y;
    // From the right, 8 px in and 4 px apart: settings, headphones, microphone.
    let gear = Rect::from_min_size(pos2(rect.right() - 8.0 - 32.0, cy - 16.0), Vec2::splat(32.0));
    let phones = Rect::from_min_size(pos2(gear.left() - 4.0 - GROUP_W, cy - 16.0), vec2(GROUP_W, 32.0));
    let mic = phones.translate(vec2(-GROUP_W - 4.0, 0.0));
    let avatar = Rect::from_min_size(pos2(rect.left() + 12.0, cy - 16.0), Vec2::splat(32.0));
    // Avatar and name: one button with a hover tint.
    let area = Rect::from_min_max(avatar.min - Vec2::splat(4.0), pos2(mic.left() - 4.0, avatar.bottom() + 4.0));
    let hot = me.is_some() && ui.rect_contains_pointer(area);
    let base = user_panel_fill(&c);
    let tint = c.text.to_color32().gamma_multiply(0.08);
    // My cosmetics (real or my local picks; ui/cosmetics.rs): nameplate, decoration, name style.
    let fx = me.as_ref().map(|u| crate::overrides::effective_for(u, None)).unwrap_or_default();
    let fx_anim = super::cosmetics::animates(cx, ui.rect_contains_pointer(rect));
    // My nameplate: filled once the text's extent is known (light themes fade the art in after it).
    let cover = ui.painter().add(Shape::Noop);
    if hot {
        ui.painter().rect_filled(area, CornerRadius::from(cx.theme.layout.corner_radius), tint);
    }
    let fill = if hot { base.blend(tint) } else { base };
    match &me {
        Some(me) => {
            let url = super::images::user_avatar_url(None, me, None, 64, false);
            let animate = crate::overrides::my_avatar_animates(me.id, ui.rect_contains_pointer(avatar)).unwrap_or(false);
            user_avatar_at(ui, cx.images, &url, avatar, animate, 1.0, (me.id, me.display_name()));
            super::images::decoration_at(ui, cx.images, avatar, fx.decoration.as_ref(), fx_anim, 1.0);
        }
        None => {
            ui.painter().circle_filled(avatar.center(), 16.0, c.surface.to_color32());
        }
    }
    let (line, detail) = my_status_line(ui, cx);
    status_dot(ui.painter(), avatar, my_presence(cx.state), &c, fill);

    let (mute, deaf) = (cx.view.self_mute, cx.view.self_deaf);
    let mic = voice_button(ui, cx, mic, false, if mute || deaf { "Unmute" } else { "Mute" }, mute || deaf);
    let phones = voice_button(ui, cx, phones, true, if deaf { "Undeafen" } else { "Deafen" }, deaf);
    let open = cx.view.settings_open;
    if panel_button(ui, cx.theme, gear, Icon::Gear, "User Settings", false, open).clicked() {
        cx.view.settings_open = !open;
    }
    if mic || phones {
        let (m, d) = toggle_voice(mute, deaf, mic);
        (cx.view.self_mute, cx.view.self_deaf) = (m, d);
        if d != deaf {
            cx.cmd.send(Command::SetSelfDeaf(d));
        }
        if m != mute {
            cx.cmd.send(Command::SetSelfMute(m));
        }
    }

    let text_left = avatar.right() + 8.0;
    let max_w = area.right() - 4.0 - text_left;
    let name = me.as_ref().map_or("Rustcord", |u| u.display_name()).to_owned();
    let label = format!("{name}, {}", line.1);
    let status = line.clone();
    // My game etc. as my member-list presence carries it.
    let activity = me.as_ref().and_then(|u| cx.state.presence_of(u.id)).and_then(|p| p.activity.clone());
    // An activity's icon leads my custom status ("🎮 • text"); without one it is the line.
    let (lead, line) = match (&me, &activity) {
        (Some(u), _) if hot && !cx.settings.streamer.hides_personal() => (None, (None, u.username.clone())),
        (_, Some(a)) if cx.state.chat.my_custom_status.is_some() => (Some(a.kind), line),
        (_, Some(a)) if detail.is_none() => (None, status_text(None, Some(a)).unwrap_or(line)),
        _ => (None, line),
    };
    let name_g = super::cosmetics::name_galley(ui, &name, bold(size - 1.0), fx.name_style.as_ref(), c.text.to_color32(), max_w);
    let sub_font = FontId::proportional(size - 2.0);
    let sub_h = ui.fonts_mut(|f| f.row_height(&sub_font));
    let top = cy - (name_g.size().y + sub_h) / 2.0;
    let name_at = pos2(text_left, top);
    ui.painter().add(super::cosmetics::name_underlay(name_at, &name_g, fx.name_style.as_ref(), ui.visuals().dark_mode));
    let sub_y = top + name_g.size().y;
    let name_end = name_at.x + name_g.size().x;
    ui.painter().galley(name_at, name_g, c.text.to_color32());
    let muted = c.text_muted.to_color32();
    let mut x = text_left;
    if let Some(kind) = lead {
        let side = (sub_h * 0.85).round();
        let icon = Rect::from_min_size(pos2(x, sub_y + (sub_h - side) / 2.0), Vec2::splat(side));
        paint_activity_icon(ui.painter(), kind, icon, presence_color(Presence::Online, &c));
        let dot = one_line(ui, "•", sub_font.clone(), muted, 16.0);
        x = icon.right() + 4.0 + dot.size().x + 4.0;
        ui.painter().galley(pos2(icon.right() + 4.0, sub_y), dot, muted);
    }
    let sub = paint_status_text(ui, cx.images, pos2(x, sub_y), &line, egui::TextFormat::simple(sub_font, muted), 16.0, text_left + max_w - x, 1);
    if let Some(np) = &fx.nameplate {
        // Under the voice section only the lower corners are the panel's.
        let corners = if rect.top() > panel.y + 0.5 { CornerRadius { sw: PANEL_R, se: PANEL_R, ..CornerRadius::ZERO } } else { CornerRadius::same(PANEL_R) };
        let end = name_end.max(sub.right());
        let shape = super::cosmetics::nameplate_cover(ui, cx.images, rect, np, fx_anim, corners, base, end);
        // High Contrast: the panel colour stays opaque behind the text.
        let strong = (crate::theme::is_high_contrast(&c) && !matches!(shape, Shape::Noop)).then(|| super::cosmetics::plate_scrim(rect, PANEL_R, base, end, true));
        ui.painter().set(cover, Shape::Vec(std::iter::once(shape).chain(strong).collect()));
    }

    let Some(me) = &me else { return };
    // Registered after the avatar widget so the whole area stays clickable.
    let resp = ui.interact(area, ui.make_persistent_id("my_account"), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
    let tip = [activity.map(|a| a.label()), status_tooltip(ui, &status), detail].into_iter().flatten().collect::<Vec<_>>().join("\n");
    let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        cx.view.open_profile_above(me.id, None, panel - vec2(0.0, 8.0));
    }
}

/// (mute, deaf) after a click on the microphone (`mic`) or on the headphones, like Discord:
/// unmuting while deafened also undeafens; deafening implies mute (shown, and the voice module
/// sends both), and undeafening restores the mute switch as it was.
pub(super) fn toggle_voice(mute: bool, deaf: bool, mic: bool) -> (bool, bool) {
    match (mic, deaf) {
        (true, true) => (false, false),
        (true, false) => (!mute, false),
        (false, _) => (mute, !deaf),
    }
}

/// Resting glyph colour of the user panel's buttons: near the text colour like Discord's, so the
/// icons stay readable on a bright nameplate.
fn panel_icon_rest(c: &ThemeColors) -> Color32 {
    c.text.to_color32().gamma_multiply(0.82)
}

/// 32 px icon button of the user panel: soft glyph on the panel (its nameplate shows through),
/// tinted on hover, pressed while `on`; `slashed` is the danger-colored "off" variant. The
/// tooltip is also the accessible name.
fn panel_button(ui: &mut egui::Ui, theme: &Theme, rect: Rect, icon: Icon, tip: &str, slashed: bool, on: bool) -> egui::Response {
    let c = &theme.colors;
    let resp = ui.interact(rect, ui.make_persistent_id(("panel_button", icon as u8)), Sense::click());
    let hot = on || resp.hovered() || resp.has_focus();
    if hot {
        ui.painter().rect_filled(rect, CornerRadius::from(theme.layout.corner_radius), c.text.to_color32().gamma_multiply(0.08));
    }
    let color = if slashed {
        c.danger.to_color32()
    } else if hot {
        c.text.to_color32()
    } else {
        panel_icon_rest(c)
    };
    paint_icon(ui.painter(), icon, rect.shrink(7.0), color, slashed, user_panel_fill(c));
    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, on || slashed, tip));
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Microphone or headphones (`output`) in `group`: the 32 px button, then the ∨ of its device
/// menu, one pill on hover of either, the hovered half (the ∨ also while its menu is open)
/// brighter; `slashed` (muted / deafened): the red "off" glyph on a red tint. True when the
/// button was clicked.
fn voice_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>, group: Rect, output: bool, tip: &str, slashed: bool) -> bool {
    let c = cx.theme.colors.clone();
    let icon = if output { Icon::Headphones } else { Icon::Mic };
    let (button, chevron) = (group.with_max_x(group.left() + 32.0), group.with_min_x(group.left() + 32.0));
    let resp = ui.interact(button, ui.make_persistent_id(("panel_button", icon as u8)), Sense::click());
    let more = ui.interact(chevron, ui.make_persistent_id(("panel_devices", icon as u8)), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&more));
    let (hot, more_hot) = (resp.hovered() || resp.has_focus(), open || more.hovered() || more.has_focus());
    let r = cx.theme.layout.corner_radius.clamp(0.0, 16.0).round() as u8;
    let p = ui.painter();
    if slashed {
        p.rect_filled(group, r, c.danger.to_color32().gamma_multiply(0.15));
    }
    // Discord: one pill around icon and ∨ while either is hovered, the hovered half brighter.
    if hot || more_hot {
        p.rect_filled(group, r, c.text.to_color32().gamma_multiply(0.06));
        let (half, corners) = if hot { (button, CornerRadius { nw: r, sw: r, ..CornerRadius::ZERO }) } else { (chevron, CornerRadius { ne: r, se: r, ..CornerRadius::ZERO }) };
        p.rect_filled(half, corners, c.text.to_color32().gamma_multiply(0.06));
    }
    let color = if slashed { c.danger.to_color32() } else if hot { c.text.to_color32() } else { panel_icon_rest(&c) };
    paint_icon(p, icon, button.shrink(7.0), color, slashed, user_panel_fill(&c));
    let k = chevron.center() - vec2(2.0, 0.0);
    let v = if more_hot { c.text.to_color32() } else { panel_icon_rest(&c) };
    p.add(Shape::line(vec![k + vec2(-3.5, -1.75), k + vec2(0.0, 1.75), k + vec2(3.5, -1.75)], Stroke::new(1.5, v)));

    let label = if output { "Output Options" } else { "Input Options" };
    more.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    if more.clicked() && !open {
        // Local: the voice task lists the audio devices (no request).
        cx.cmd.send(Command::ListVoiceDevices);
    }
    let more = if open { more } else { more.on_hover_text(label) };
    device_menu(cx, &more.on_hover_cursor(egui::CursorIcon::PointingHand), output);
    resp.widget_info(|| WidgetInfo::selected(Role::Button, true, slashed, tip));
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// The ∨ menu of the microphone / headphones (`output`): the devices of the last
/// `ListVoiceDevices`, Default first and the pick ticked (a pick goes to the voice task and is
/// saved, like on the Voice & Video page), then a link to that page.
fn device_menu(cx: &mut UiCx<'_>, chevron: &egui::Response, output: bool) {
    let (text, muted) = (cx.theme.colors.text.to_color32(), cx.theme.colors.text_muted.to_color32());
    let (list, current) = if output { (&cx.state.voice.outputs, &cx.settings.voice_output) } else { (&cx.state.voice.inputs, &cx.settings.voice_input) };
    let (mut pick, mut page) = (None, false);
    egui::Popup::menu(chevron).align(egui::RectAlign::TOP_START).gap(8.0).show(|ui| {
        menu_setup(ui);
        ui.set_max_width(280.0);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.label(egui::RichText::new(if output { "Output Device" } else { "Input Device" }).size(12.0).color(muted));
        for name in std::iter::once(None).chain(list.iter().map(Some)) {
            let glyph = if current.as_ref() == name { "✔" } else { "" };
            if menu_item(ui, name.map_or("Default", String::as_str), MenuIcon::Glyph(glyph), text) {
                pick = Some(name.cloned());
            }
        }
        ui.separator();
        page = menu_item(ui, "Voice Settings", MenuIcon::Glyph("⚙"), text);
    });
    if let Some(name) = pick {
        *(if output { &mut cx.settings.voice_output } else { &mut cx.settings.voice_input }) = name;
        cx.cmd.send(Command::SetVoiceDevices { input: cx.settings.voice_input.clone(), output: cx.settings.voice_output.clone() });
        if let Err(e) = cx.settings.save() {
            cx.view.toast(format!("Could not save settings: {e}"), true);
        }
    }
    if page {
        cx.view.settings_page = super::user_settings::Page::VoiceVideo;
        cx.view.settings_open = true;
    }
}

/// Test hook `RUSTCORD_OPEN=status:<online|idle|dnd|invisible>`: my status as the gateway would
/// report it, with a sample custom status, and a game as my member-list presence carries it.
pub(super) fn demo_status(state: &mut AppState, status: &str) {
    let Some(status) = UserStatus::parse(status) else { return };
    let emoji = crate::model::Emoji { name: Some("🦀".into()), ..Default::default() };
    let custom_status = Some(CustomStatus { text: Some("Rewriting it in Rust".into()), emoji: Some(emoji) });
    state.apply_gateway(crate::model::GatewayEvent::MyStatus { status, custom_status }, None);
    if let Some(me) = state.me.as_ref().map(|u| u.id) {
        let p = crate::model::UserPresence { status: status.presence(), activity: Some(super::members::demo_activity(0)), ..Default::default() };
        state.apply_gateway(crate::model::GatewayEvent::PresenceUpdate(vec![(me, p)]), None);
    }
}

/// (status line, connection details for the tooltip unless connected) of the user panel.
fn my_status_line(ui: &egui::Ui, cx: &UiCx<'_>) -> (StatusText, Option<String>) {
    let (conn, offline_mode) = (&cx.state.conn, crate::config::offline_mode());
    let elapsed = cx.view.conn_since.elapsed().as_secs();
    if matches!(conn, ConnState::Reconnecting { .. }) && !offline_mode {
        // Keep the countdown (tooltip) ticking.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    }
    let detail = (*conn != ConnState::Connected || offline_mode).then(|| connection_label(conn, elapsed, offline_mode));
    (my_line(cx.state, offline_mode), detail)
}

/// The text of `my_status_line`: while connecting the connection, else my custom status or
/// my status once the gateway reported it.
fn my_line(state: &AppState, offline_mode: bool) -> StatusText {
    let connecting = matches!(state.conn, ConnState::Connecting | ConnState::Reconnecting { .. });
    match state.chat.my_status {
        Some(s) if !connecting => status_text(state.chat.my_custom_status.as_ref(), None).unwrap_or_else(|| (None, status_label(s).into())),
        _ => (None, connection_word(&state.conn, offline_mode).into()),
    }
}

/// My status dot: my chosen status once known, else the connection state (idle while
/// connecting).
pub(super) fn my_presence(state: &AppState) -> Presence {
    match (state.chat.my_status, &state.conn, crate::config::offline_mode()) {
        (Some(s), ..) => s.presence(),
        (None, _, true) | (None, ConnState::Offline, _) => Presence::Offline,
        (None, ConnState::Connected, _) => Presence::Online,
        _ => Presence::Idle,
    }
}

/// The one word under my name.
fn connection_word(conn: &ConnState, offline_mode: bool) -> &'static str {
    match (conn, offline_mode) {
        (_, true) | (ConnState::Offline, _) => "Offline",
        (ConnState::Connected, _) => "Online",
        (ConnState::Connecting, _) => "Connecting…",
        (ConnState::Reconnecting { .. }, _) => "Reconnecting…",
    }
}

/// Details of the connection state (tooltip of the user panel's status line).
fn connection_label(conn: &ConnState, elapsed_secs: u64, offline_mode: bool) -> String {
    if offline_mode {
        return "Offline mode".into();
    }
    match conn {
        ConnState::Connected => "Connected".into(),
        ConnState::Connecting => "Connecting…".into(),
        ConnState::Reconnecting { in_secs, .. } => {
            let left = u64::from(*in_secs).saturating_sub(elapsed_secs);
            if left == 0 { "Reconnecting…".into() } else { format!("Reconnecting in {left}s") }
        }
        ConnState::Offline => "Offline".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_of_names() {
        assert_eq!(initials("Rust Workshop"), "RW");
        assert_eq!(initials("  egui  fans club  extra "), "efc");
        assert_eq!(initials("Ünïcode"), "Ü");
        assert_eq!(initials("   "), "?");
    }

    #[test]
    fn connection_labels() {
        assert_eq!(connection_label(&ConnState::Connected, 0, false), "Connected");
        assert_eq!(connection_label(&ConnState::Connected, 0, true), "Offline mode");
        assert_eq!(connection_label(&ConnState::Reconnecting { attempt: 2, in_secs: 5 }, 2, false), "Reconnecting in 3s");
        assert_eq!(connection_label(&ConnState::Reconnecting { attempt: 2, in_secs: 5 }, 9, false), "Reconnecting…");
        assert_eq!(connection_label(&ConnState::Offline, 0, false), "Offline");
    }

    #[test]
    fn connection_words() {
        assert_eq!(connection_word(&ConnState::Connected, false), "Online");
        assert_eq!(connection_word(&ConnState::Connected, true), "Offline");
        assert_eq!(connection_word(&ConnState::Reconnecting { attempt: 1, in_secs: 3 }, false), "Reconnecting…");
        assert_eq!(connection_word(&ConnState::Offline, false), "Offline");
    }

    #[test]
    fn glyphs() {
        assert_eq!(channel_glyph(ct::GUILD_TEXT), "#");
        assert_eq!(channel_glyph(ct::GUILD_VOICE), "🔊");
    }

    #[test]
    fn mute_and_deafen_toggles() {
        // Mic: plain toggle; while deafened it undeafens and unmutes.
        assert_eq!(toggle_voice(false, false, true), (true, false));
        assert_eq!(toggle_voice(true, false, true), (false, false));
        assert_eq!(toggle_voice(true, true, true), (false, false));
        assert_eq!(toggle_voice(false, true, true), (false, false));
        // Headphones: toggle deafen, keep the mute switch for when it ends.
        assert_eq!(toggle_voice(false, false, false), (false, true));
        assert_eq!(toggle_voice(false, true, false), (false, false));
        assert_eq!(toggle_voice(true, false, false), (true, true));
        assert_eq!(toggle_voice(true, true, false), (true, false));
    }

    #[test]
    fn hide_muted_keeps_the_open_channel() {
        let snap = serde_json::from_str(include_str!("../../tests/fixtures/home/cache/snapshot.json")).unwrap();
        let mut s = AppState::from_snapshot(snap);
        let g = s.guilds[0].id;
        let ids = |s: &AppState| -> Vec<Id> {
            let mut items = s.sidebar_channels(g);
            drop_muted(s, &mut items);
            items.iter().map(|(SidebarItem::Category { id, .. } | SidebarItem::Channel { id })| *id).collect()
        };
        let all = ids(&s);
        // Mute the "Voice" category (its channel follows) and the open channel.
        let (voice_cat, open) = (Id(1565443437363200013), s.selected_channel.unwrap());
        let overrides = [voice_cat, open].map(|channel_id| crate::model::ChannelOverride { channel_id, muted: true, ..Default::default() });
        s.guild_settings.insert(Some(g), crate::model::GuildSettings { guild_id: Some(g), channel_overrides: overrides.to_vec(), ..Default::default() });
        let kept = ids(&s);
        assert!(kept.contains(&open));
        assert!(!kept.contains(&voice_cat) && !kept.contains(&Id(1565443437363200014)));
        assert_eq!(kept.len(), all.len() - 2);
    }

    #[test]
    fn drop_zones_around_an_icon() {
        let icon = Rect::from_min_size(pos2(16.0, 100.0), Vec2::splat(ICON));
        let at = |y: f32, slot, merge| zone(Some(pos2(30.0, y)), icon, slot, merge).map(|t| (t.to, t.line, t.ring.is_some()));
        let merge = Some(GuildDrop::NewFolder { with: Id(9) });
        // Top-level: edges insert before / after (the line in the gap), the middle merges.
        assert_eq!(at(97.0, Slot::Top(2), merge), Some((GuildDrop::Top(2), Some(96.0), false)));
        assert_eq!(at(138.0, Slot::Top(2), merge), Some((GuildDrop::Top(3), Some(144.0), false)));
        assert_eq!(at(120.0, Slot::Top(2), merge), Some((GuildDrop::NewFolder { with: Id(9) }, None, true)));
        assert_eq!(at(120.0, Slot::Top(2), None), None);
        assert_eq!(at(150.0, Slot::Top(2), merge), None);
        // In a folder: halves.
        assert_eq!(at(110.0, Slot::Child(7, 1), None), Some((GuildDrop::Folder { id: 7, index: 1 }, Some(96.0), false)));
        assert_eq!(at(130.0, Slot::Child(7, 1), None), Some((GuildDrop::Folder { id: 7, index: 2 }, Some(144.0), false)));
        assert_eq!(zone(None, icon, Slot::Top(0), merge).map(|t| t.to), None);
    }

    #[test]
    fn closed_dms_return_with_new_messages() {
        let mut s = AppState::default();
        let dm = crate::model::Channel { id: Id(5), kind: ct::DM, last_message_id: Some(Id(10)), ..Default::default() };
        s.channels.insert(dm.id, dm);
        let mut hidden = HashMap::new();
        assert!(dm_listed(&s, &hidden, Id(5)));
        hidden.insert(Id(5), Some(Id(10)));
        assert!(!dm_listed(&s, &hidden, Id(5)));
        s.channels.get_mut(&Id(5)).unwrap().last_message_id = Some(Id(11));
        assert!(dm_listed(&s, &hidden, Id(5)));
    }
}
