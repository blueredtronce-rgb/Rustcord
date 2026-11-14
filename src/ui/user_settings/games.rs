//! Registered Games (docs/SETTINGS_NOTES.md §4): the game Rustcord sees me playing, "Add it!"
//! (pick an open window's program as a game) and my added games with Remove. The list stays on
//! this device (`crate::activity`); others see the game only while Share detected activity
//! (Activity Privacy, settings 11.3) is on, through op 3.
//! OWNERSHIP: activities engineer.

use super::super::messages::Pal;
use super::super::{UiCx, one_line, section};
use super::Page;
use crate::activity::{ActivityCommand, AddedGame, GameActivity, Window};
use crate::backend::Command;
use egui::{FontId, Rect, RichText, Sense, Vec2, pos2, vec2};

const ICON: f32 = 40.0;

/// The page. `RUSTCORD_NAV_DEMO=games` opens the picker with sample windows (no OS call).
pub(super) fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let pal = Pal::new(cx.theme);
    let picking_id = ui.make_persistent_id("rc_games_picker");
    let demo = super::super::sidebar::nav_demo("games");
    let mut picking = demo || ui.data(|d| d.get_temp::<bool>(picking_id).unwrap_or(false));
    if demo && cx.state.activity.windows.is_none() {
        let w = |title: &str, exe: &str| Window { title: title.into(), exe: exe.into() };
        cx.state.activity.windows = Some(vec![
            w("Crab Rave Racers", "d:/games/crab rave racers/crr.exe"),
            w("Ferris Farm", "c:/program files/ferris farm/ferrisfarm.exe"),
            w("Untitled - Notepad", "c:/windows/notepad.exe"),
        ]);
    }

    now_playing(ui, cx, &pal, &mut picking);
    if picking {
        picker(ui, cx, &pal, &mut picking);
    }
    ui.data_mut(|d| d.insert_temp(picking_id, picking));

    section(ui, cx.theme, "Added games");
    let added = cx.state.activity.added.clone();
    if added.is_empty() {
        super::super::hint(ui, cx.theme, "Games you add show here. Rustcord looks for them like the games it knows.");
    }
    for g in &added {
        let remove = ui
            .horizontal(|ui| {
                game_icon(ui, cx, &pal, None, 32.0, pal.bg);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&g.name).color(pal.text));
                    ui.label(RichText::new(&g.exe).size(pal.meta()).color(pal.muted));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| crate::ui::messages::vector_button(ui, &pal, crate::ui::icons::Icon::Trash, "Remove", 32.0, false, pal.danger).clicked()).inner
            })
            .inner;
        if remove {
            set_added(cx, added.iter().filter(|x| x.exe != g.exe).cloned().collect());
        }
    }

    section(ui, cx.theme, "Activity sharing");
    let on = crate::activity::sharing(cx.state);
    let text = if on { "Others see the game you play while Share detected activity is on." } else { "Share detected activity is off: nobody sees your game." };
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).size(pal.size - 1.0).color(pal.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if secondary(ui, &pal, "Activity privacy").clicked() {
                cx.view.settings_page = Page::ActivityPrivacy;
            }
        });
    });
}

/// The detected game (its icon, name, whether others see it), or "No game detected" with
/// "Add it!".
fn now_playing(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, picking: &mut bool) {
    let game = cx.state.activity.detected.clone();
    let shared = crate::activity::sharing(cx.state);
    let radius = pal.radius + pal.radius / 2;
    egui::Frame::new().fill(pal.surface).corner_radius(radius).inner_margin(16).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            game_icon(ui, cx, pal, game.as_ref(), ICON, pal.surface);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                let (title, line) = match &game {
                    Some(g) => (g.name.as_str(), if shared { "Now playing. Others see it as your activity." } else { "Now playing. Hidden: Share detected activity is off." }),
                    None => ("No game detected", "What are you playing?"),
                };
                ui.label(RichText::new(title).font(FontId::new(pal.size + 1.0, crate::theme::semibold_family())).color(pal.text));
                ui.label(RichText::new(line).size(pal.meta()).color(pal.muted));
            });
            if game.is_none() && !*picking {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary(ui, pal, "Add it!").clicked() {
                        *picking = true;
                        cx.state.activity.windows = None;
                        cx.cmd.send(Command::Activity(ActivityCommand::ListWindows));
                    }
                });
            }
        });
    });
}

/// The open windows' programs; a click adds one as a game (named after its window).
fn picker(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, picking: &mut bool) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Pick the program of your game").font(FontId::new(pal.size - 2.0, crate::theme::semibold_family())).color(pal.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new("Cancel").color(pal.text)).frame(false)).clicked() {
                *picking = false;
            }
        });
    });
    let Some(windows) = cx.state.activity.windows.clone() else {
        ui.add(egui::Spinner::new().size(16.0).color(pal.muted));
        return;
    };
    let fresh: Vec<&Window> = windows.iter().filter(|w| !cx.state.activity.added.iter().any(|g| g.exe == w.exe)).collect();
    if fresh.is_empty() {
        super::super::hint(ui, cx.theme, "No other program has a window open. Start your game, then select Add it! again.");
        return;
    }
    // One group of rows with hairlines between them (docs/DESIGN.md §9 option lists).
    let frame = egui::Frame::new().fill(pal.surface).corner_radius(pal.radius + pal.radius / 2).inner_margin(4);
    frame.show(ui, |ui| ui.vertical(|ui| picker_rows(ui, cx, pal, &fresh, picking)));
}

fn picker_rows(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, windows: &[&Window], picking: &mut bool) {
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, w) in windows.iter().enumerate() {
        let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
        if i > 0 {
            ui.painter().hline(row.x_range().shrink(8.0), row.top(), egui::Stroke::new(1.0, pal.text.gamma_multiply(0.1)));
        }
        if resp.hovered() || resp.has_focus() {
            ui.painter().rect_filled(row, pal.radius, pal.tint());
        }
        let x = row.left() + 12.0;
        let title = one_line(ui, w.title.as_str(), FontId::proportional(pal.size), pal.text, row.width() - 24.0);
        let exe = one_line(ui, crate::activity::file_name(&w.exe), FontId::proportional(pal.meta()), pal.muted, row.width() - 24.0);
        let top = row.center().y - (title.size().y + exe.size().y) / 2.0;
        let h = title.size().y;
        ui.painter().galley(pos2(x, top), title, pal.text);
        ui.painter().galley(pos2(x, top + h), exe, pal.muted);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Add {}", w.title)));
        if resp.on_hover_text(&w.exe).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            let mut list = cx.state.activity.added.clone();
            list.push(AddedGame { name: w.title.clone(), exe: w.exe.clone() });
            set_added(cx, list);
            *picking = false;
        }
    }
}

/// Shown at once; saved (and scanned for) by the activity thread. Nothing leaves the device.
fn set_added(cx: &mut UiCx<'_>, list: Vec<AddedGame>) {
    cx.state.activity.added = list.clone();
    cx.cmd.send(Command::Activity(ActivityCommand::SetAdded(list)));
}

/// A game's icon (`side` px): its application's icon, else a controller on a tile over `under`.
fn game_icon(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, game: Option<&GameActivity>, side: f32, under: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let px = (side * ui.ctx().pixels_per_point()).ceil() as u32;
    let url = game.and_then(|g| crate::activity::art_url(ui.ctx(), cx.state, &g.to_activity(), px.max(64)));
    match url.and_then(|u| cx.images.get(ui.ctx(), &u, px.max(64), false)) {
        Some(tex) => egui::Image::from_texture(tex).corner_radius(pal.radius).paint_at(ui, rect),
        None => {
            let tile = pal.text.gamma_multiply(0.06);
            ui.painter().rect_filled(rect, pal.radius, tile);
            let color = if game.is_some() { super::super::presence_color(crate::model::Presence::Online, &cx.theme.colors) } else { pal.muted };
            // The icon set's sizes (docs/DESIGN.md §8.2): 24 on the large tile, 20 on rows.
            let icon = Rect::from_center_size(rect.center(), Vec2::splat(if side >= ICON { 24.0 } else { 20.0 }));
            super::super::icons::paint(ui.painter(), super::super::icons::Icon::Playing, icon, color, under.blend(tile));
        }
    }
}

/// Secondary button (docs/DESIGN.md §9): `selected` fill, 32 tall.
fn secondary(ui: &mut egui::Ui, pal: &Pal, label: &str) -> egui::Response {
    let b = egui::Button::new(RichText::new(label).size(pal.size - 1.0).color(pal.text)).fill(pal.text.gamma_multiply(0.1)).corner_radius(pal.radius).min_size(vec2(64.0, 32.0));
    ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand)
}

