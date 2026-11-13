//! Settings → Connections (docs/COSMETICS_NOTES.md §10): my connected accounts, loaded with one
//! `GET /users/@me/connections` each time the page opens. Each card has "Display on profile" /
//! "Display as status" switches (`PATCH`; the answer reloads the list) and a remove button
//! behind a confirmation (`DELETE`, which also leaves servers joined through the account).
//! Adding an account happens on the platform's site, not here.
//! OWNERSHIP: cosmetics-display engineer.

use super::super::messages::{Pal, icon_button};
use super::super::{UiCx, bold, cosmetics, hint, one_line};
use crate::backend::{Command, SocialCommand};
use crate::model::ConnectedAccount;
use egui::{Frame, Margin, Rect, RichText, Sense, Vec2, pos2, vec2};

pub(super) fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let pal = Pal::new(cx.theme);
    demo(cx);
    // Shown in the previous pass too = still the same opening; otherwise load the list once.
    let seen = egui::Id::unique("rc_connections_page_pass");
    let pass = ui.ctx().cumulative_pass_nr();
    let last = ui.ctx().data(|d| d.get_temp::<u64>(seen));
    ui.ctx().data_mut(|d| d.insert_temp(seen, pass));
    if last.is_none_or(|p| p + 1 < pass) && !super::super::sidebar::nav_demo("profile") {
        cx.cmd.send(Command::Social(SocialCommand::FetchConnections));
    }
    hint(ui, cx.theme, "Accounts linked to your profile. Link new ones on the platform's site.");
    let list = match &cx.state.social.connections {
        None => {
            ui.spinner();
            return;
        }
        Some(Err(e)) => {
            // Devices' and Authorized Apps' empty state (lists.rs `loaded`); Retry sends the fetch again.
            let reason = e.split_once(": ").map_or(e.as_str(), |(_, r)| r);
            super::empty_state(ui, cx, "🔌", "Couldn't load connections", reason, None);
            if ui.vertical_centered(|ui| ui.button("Retry").clicked()).inner {
                cx.state.social.connections = None;
                cx.cmd.send(Command::Social(SocialCommand::FetchConnections));
            }
            return;
        }
        Some(Ok(list)) => list.iter().filter(|a| cosmetics::shown_connection(&a.kind)).cloned().collect::<Vec<_>>(),
    };
    // Only above the cards: the spinner and the empty state start where Devices' do.
    ui.add_space(12.0);
    if list.is_empty() {
        hint(ui, cx.theme, "No connected accounts.");
    }
    for a in &list {
        card(ui, cx, &pal, a);
        ui.add_space(8.0);
    }
}

/// One account: icon, name, platform, ✔; the remove button; the switches.
fn card(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, a: &ConnectedAccount) {
    let (platform, _) = cosmetics::platform(&a.kind);
    Frame::new().fill(pal.tint()).corner_radius(pal.card_radius()).inner_margin(Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
        let icon = Rect::from_min_size(pos2(head.left(), head.center().y - 16.0), Vec2::splat(32.0));
        cosmetics::paint_platform_icon(ui.painter(), icon, &a.kind);
        let x = icon.right() + 12.0;
        let name = one_line(ui, a.name.as_str(), bold(pal.size), pal.text, head.right() - x - 64.0);
        let sub = one_line(ui, if a.verified { format!("{platform} · Verified") } else { platform.clone() }, egui::FontId::proportional(pal.meta()), pal.muted, head.right() - x - 64.0);
        let top = head.center().y - (name.size().y + sub.size().y) / 2.0;
        let name_h = name.size().y;
        ui.painter().galley(pos2(x, top), name, pal.text);
        ui.painter().galley(pos2(x, top + name_h), sub, pal.muted);
        let mut remove = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(pos2(head.right() - 32.0, head.top() + 2.0), head.max)));
        if icon_button(&mut remove, pal, "🗑", "Remove", 32.0, false, pal.danger).clicked() {
            let body = format!("Disconnect your {platform} account \"{}\"? Servers you joined through it may remove you.", a.name);
            cx.view.confirm("Remove Connection", body, "Remove", Command::Social(SocialCommand::RemoveConnection { kind: a.kind.clone(), id: a.id.clone() }));
        }
        ui.add_space(6.0);
        let mut visible = a.visibility.unwrap_or(0) == 1;
        if toggle(ui, pal, "Display on profile", &mut visible) {
            update(cx, a, Some(visible), None);
        }
        if let Some(mut on) = a.show_activity
            && toggle(ui, pal, "Display as status", &mut on)
        {
            update(cx, a, None, Some(on));
        }
    });
}

fn toggle(ui: &mut egui::Ui, pal: &Pal, label: &str, on: &mut bool) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(pal.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| super::switch(ui, pal, on, label).changed()).inner
    })
    .inner
}

/// Shows the switch at once, then sends the change (the answer reloads the list).
fn update(cx: &mut UiCx<'_>, a: &ConnectedAccount, visible: Option<bool>, show_activity: Option<bool>) {
    if let Some(Ok(list)) = &mut cx.state.social.connections
        && let Some(c) = list.iter_mut().find(|c| c.kind == a.kind && c.id == a.id)
    {
        if let Some(v) = visible {
            c.visibility = Some(u8::from(v));
        }
        if show_activity.is_some() {
            c.show_activity = show_activity;
        }
    }
    let cmd = SocialCommand::UpdateConnection { kind: a.kind.clone(), id: a.id.clone(), visibility: visible, show_activity };
    cx.cmd.send(Command::Social(cmd));
}

/// `RUSTCORD_NAV_DEMO=profile`: sample connections (no request).
fn demo(cx: &mut UiCx<'_>) {
    if cx.state.social.connections.is_some() || !super::super::sidebar::nav_demo("profile") {
        return;
    }
    let acc = |kind: &str, name: &str, verified: bool, visible: u8, activity: Option<bool>| ConnectedAccount {
        kind: kind.into(),
        id: format!("{name}-id"),
        name: name.into(),
        verified,
        visibility: Some(visible),
        show_activity: activity,
    };
    cx.state.social.connections = Some(Ok(vec![
        acc("github", "ferris-dev", true, 1, None),
        acc("spotify", "Ferris", true, 1, Some(true)),
        acc("steam", "crabby", false, 0, Some(false)),
    ]));
}
