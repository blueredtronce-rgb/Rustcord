//! Plugin dialogs (one at a time, opened from a menu or profile link): PermissionsViewer,
//! ServerInfo and ViewRaw. Everything shown is cached state; nothing is fetched.

use super::people::member_perms;
use crate::model::{Guild, Id, perm};
use crate::ui::UiCx;
use egui::RichText;

#[derive(Debug, Clone, PartialEq)]
pub enum Window {
    /// A member's server permissions and the roles they come from.
    Permissions { guild_id: Id, user_id: Id },
    ServerInfo { guild_id: Id },
    /// ViewRaw: pretty JSON of a message.
    Raw { title: String, text: String },
}

fn slot() -> egui::Id {
    egui::Id::unique("rc_plugin_window")
}

pub fn open(ctx: &egui::Context, w: Window) {
    ctx.data_mut(|d| d.insert_temp(slot(), Some(w)));
}

/// Every Discord permission bit with its settings name (PermissionsViewer).
pub const PERMISSIONS: &[(u32, &str)] = &[
    (3, "Administrator"),
    (10, "View Channels"),
    (4, "Manage Channels"),
    (28, "Manage Roles"),
    (30, "Manage Expressions"),
    (43, "Create Expressions"),
    (7, "View Audit Log"),
    (29, "Manage Webhooks"),
    (5, "Manage Server"),
    (0, "Create Invite"),
    (26, "Change Nickname"),
    (27, "Manage Nicknames"),
    (1, "Kick Members"),
    (2, "Ban Members"),
    (40, "Timeout Members"),
    (11, "Send Messages"),
    (38, "Send Messages in Threads"),
    (35, "Create Public Threads"),
    (36, "Create Private Threads"),
    (14, "Embed Links"),
    (15, "Attach Files"),
    (6, "Add Reactions"),
    (18, "Use External Emoji"),
    (37, "Use External Stickers"),
    (17, "Mention Everyone"),
    (13, "Manage Messages"),
    (51, "Pin Messages"),
    (34, "Manage Threads"),
    (16, "Read Message History"),
    (12, "Send TTS Messages"),
    (46, "Send Voice Messages"),
    (49, "Create Polls"),
    (20, "Connect"),
    (21, "Speak"),
    (9, "Video"),
    (39, "Use Activities"),
    (42, "Use Soundboard"),
    (45, "Use External Sounds"),
    (25, "Use Voice Activity"),
    (8, "Priority Speaker"),
    (22, "Mute Members"),
    (23, "Deafen Members"),
    (24, "Move Members"),
    (31, "Use Application Commands"),
    (50, "Use External Apps"),
    (32, "Request to Speak"),
    (33, "Create Events"),
    (44, "Manage Events"),
    (41, "View Server Insights"),
    (47, "View Creator Monetization Analytics"),
];

/// Per permission: granted, and the highest role (or "Server Owner") that grants it.
pub fn permission_rows(g: &Guild, user_id: Id, roles: &[Id]) -> Vec<(&'static str, bool, String)> {
    let total = member_perms(g, user_id, roles);
    let mut own: Vec<&crate::model::Role> = g.roles.iter().filter(|r| r.id == g.id || roles.contains(&r.id)).collect();
    own.sort_by_key(|r| std::cmp::Reverse((r.position, r.id)));
    PERMISSIONS
        .iter()
        .map(|&(bit, name)| {
            let mask = 1u64 << bit;
            let from = if g.owner_id == user_id {
                "Server Owner".to_owned()
            } else {
                own.iter()
                    .find(|r| r.permissions.0 & (mask | perm::ADMINISTRATOR) != 0)
                    .map(|r| if r.id == g.id { "@everyone".to_owned() } else { r.name.clone() })
                    .unwrap_or_default()
            };
            (name, total & mask != 0, from)
        })
        .collect()
}

/// Draws the open plugin dialog (from `plugins::frame`).
pub fn show(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    let Some(w) = ctx.data(|d| d.get_temp::<Option<Window>>(slot())).flatten() else { return };
    let theme = cx.theme.clone();
    let (text, muted) = (theme.colors.text.to_color32(), theme.colors.text_muted.to_color32());
    let (title, width) = match &w {
        Window::Permissions { .. } => ("Permissions".to_owned(), 500.0),
        Window::ServerInfo { guild_id } => (cx.state.guild(*guild_id).map_or_else(|| "Server Info".to_owned(), |g| g.name.clone()), 480.0),
        Window::Raw { title, .. } => (title.clone(), 620.0),
    };
    let resp = crate::ui::dialog(ctx, &theme, "rc_plugin_window", width, &title, |ui| {
        match &w {
            Window::Permissions { guild_id, user_id } => permissions(ui, cx, *guild_id, *user_id, text, muted),
            Window::ServerInfo { guild_id } => server_info(ui, cx, *guild_id, text, muted),
            Window::Raw { text: raw, .. } => {
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(raw.clone());
                }
                let mut shown = raw.as_str();
                ui.add(egui::TextEdit::multiline(&mut shown).code_editor().desired_width(f32::INFINITY));
            }
        }
        ui.add_space(8.0);
        let close = egui::Button::new("Close").min_size(egui::vec2(96.0, 36.0));
        crate::ui::dialog_footer(ui, |ui| ui.add(close).clicked())
    });
    if resp.inner || resp.should_close() {
        ctx.data_mut(|d| d.remove::<Option<Window>>(slot()));
    }
}

fn row(ui: &mut egui::Ui, key: &str, value: &str, text: egui::Color32, muted: egui::Color32) {
    ui.label(RichText::new(key).color(muted));
    ui.add(egui::Label::new(RichText::new(value).color(text)).truncate());
    ui.end_row();
}

fn permissions(ui: &mut egui::Ui, cx: &mut UiCx<'_>, guild_id: Id, user_id: Id, text: egui::Color32, muted: egui::Color32) {
    let Some(g) = cx.state.guild(guild_id) else { return };
    let roles = cx.state.find_member(guild_id, user_id).map(|m| m.roles.clone()).or_else(|| cx.state.my_members.get(&guild_id).filter(|_| cx.state.me.as_ref().is_some_and(|m| m.id == user_id)).map(|m| m.roles.clone())).unwrap_or_default();
    let name = cx.state.users.get(&user_id).or(cx.state.me.as_ref().filter(|m| m.id == user_id)).map_or_else(|| user_id.0.to_string(), |u| u.display_name().to_owned());
    ui.label(RichText::new(format!("{name} in {}", g.name)).color(muted));
    let (green, red) = (crate::ui::presence_color(crate::model::Presence::Online, &cx.theme.colors), cx.theme.colors.danger.to_color32());
    egui::Grid::new("rc_permissions").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
        for (perm_name, on, from) in permission_rows(g, user_id, &roles) {
            ui.label(RichText::new(if on { "✔" } else { "✖" }).family(crate::theme::icon_family()).color(if on { green } else { red }));
            ui.label(RichText::new(perm_name).color(text));
            ui.add(egui::Label::new(RichText::new(if on { from } else { String::new() }).color(muted)).truncate());
            ui.end_row();
        }
    });
}

fn server_info(ui: &mut egui::Ui, cx: &mut UiCx<'_>, guild_id: Id, text: egui::Color32, muted: egui::Color32) {
    let s = &*cx.state;
    let Some(g) = s.guild(guild_id) else { return };
    let owner = s.users.get(&g.owner_id).or(s.me.as_ref().filter(|m| m.id == g.owner_id)).map_or_else(|| g.owner_id.0.to_string(), |u| format!("{} ({})", u.display_name(), u.username));
    let created = snowflake_date(guild_id);
    let channels = s.channels.values().filter(|c| c.guild_id == Some(guild_id) && c.kind != crate::model::channel_type::GUILD_CATEGORY).count();
    let members = s.member_lists.get(&guild_id).filter(|l| l.member_count > 0).map(|l| format!("{} ({} online)", l.member_count, l.online_count));
    egui::Grid::new("rc_server_info").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
    row(ui, "Owner", &owner, text, muted);
    row(ui, "Created", &created, text, muted);
    if let Some(m) = members {
        row(ui, "Members", &m, text, muted);
    }
    row(ui, "Channels", &channels.to_string(), text, muted);
    row(ui, "Roles", &g.roles.len().saturating_sub(1).to_string(), text, muted);
    row(ui, "Emoji", &g.emojis.len().to_string(), text, muted);
    row(ui, "Boost level", &g.premium_tier.to_string(), text, muted);
    row(ui, "2FA for moderation", if g.mfa_level == 1 { "Required" } else { "Off" }, text, muted);
    row(ui, "Server ID", &guild_id.0.to_string(), text, muted);
    });
}

/// The creation date in a snowflake ("Mar 4, 2021").
pub fn snowflake_date(id: Id) -> String {
    let ms = (id.0 >> 22) + 1_420_070_400_000;
    chrono::DateTime::from_timestamp_millis(ms as i64).map_or_else(String::new, |t| t.with_timezone(&chrono::Local).format("%b %-d, %Y").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Perms, Role};

    #[test]
    fn permission_rows_name_the_granting_role() {
        let role = |id, pos, p, name: &str| Role { id: Id(id), position: pos, permissions: Perms(p), name: name.into(), ..Default::default() };
        let g = Guild { id: Id(1), owner_id: Id(9), roles: vec![role(1, 0, perm::VIEW_CHANNEL, "@everyone"), role(2, 1, perm::KICK_MEMBERS, "Mod")], ..Default::default() };
        let rows = permission_rows(&g, Id(5), &[Id(2)]);
        let get = |n: &str| rows.iter().find(|r| r.0 == n).cloned().unwrap();
        assert_eq!(get("Kick Members"), ("Kick Members", true, "Mod".into()));
        assert_eq!(get("View Channels"), ("View Channels", true, "@everyone".into()));
        assert!(!get("Ban Members").1);
        assert_eq!(get("Ban Members").2, "");
        assert!(permission_rows(&g, Id(9), &[]).iter().all(|r| r.1 && r.2 == "Server Owner"));
        // Every bit appears once.
        let mut bits: Vec<u32> = PERMISSIONS.iter().map(|p| p.0).collect();
        bits.sort_unstable();
        bits.dedup();
        assert_eq!(bits.len(), PERMISSIONS.len());
        assert!(!snowflake_date(Id(0)).is_empty());
    }
}
