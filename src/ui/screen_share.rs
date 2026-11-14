//! Screen share picker (`Modal::ScreenShare`, opened by a screen-share button while I don't
//! share): Screens / Windows tabs filled by the `ListScreenSources` its opening sends (⟳ lists
//! again), the sources by name (no thumbnails), quality (720p / 1080p, 15 / 30 fps) and Go Live
//! → `StartScreenShare` (no application audio). The user picks the source; nothing is
//! preselected.
//! OWNERSHIP: video-ui engineer.

use super::messages::{Pal, vector_button};
use super::icons::{self, Icon};
use super::{Modal, UiCx, dialog, dialog_buttons, one_line};
use crate::backend::Command;
use crate::voice::video::{ScreenSource, VideoCommand};
use egui::{Color32, FontId, Rect, Role, Sense, Stroke, StrokeKind, Vec2, WidgetInfo, pos2, vec2};

/// The picker's choices (`CallUi::share`), reset on every opening.
#[derive(Default)]
pub struct ShareForm {
    /// Windows tab (else Screens).
    windows: bool,
    pick: Option<ScreenSource>,
    /// 1080p (else 720p).
    hd: bool,
    /// 15 fps (else 30).
    low_fps: bool,
}

/// Opens the picker and lists the screens and windows (local, no request).
pub(super) fn open(cx: &mut UiCx<'_>) {
    cx.view.call_ui.share = ShareForm::default();
    cx.cmd.send(Command::Video(VideoCommand::ListScreenSources));
    cx.view.modal = Some(Modal::ScreenShare);
}

/// Test hook (call.rs `demo`): a source picked.
pub(super) fn demo_pick(f: &mut ShareForm, source: ScreenSource) {
    f.pick = Some(source);
}

/// `Modal::ScreenShare`.
pub fn modal(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    let theme = &*cx.theme;
    let pal = Pal::new(theme);
    let v = &cx.state.voice.video;
    let connected = cx.state.voice.connected;
    let f = &mut cx.view.call_ui.share;
    let mut refresh = false;
    let resp = dialog(ctx, theme, "rc_screen_share", 602.0, "Share your screen", |ui| {
        ui.horizontal(|ui| {
            // Screens | Windows: the shared segmented control (§9).
            let mut windows = f.windows;
            ui.allocate_ui(vec2(240.0, 36.0), |ui| crate::ui::login::segmented(ui, theme, &mut windows, &[(false, "Screens"), (true, "Windows")]));
            if windows != f.windows {
                (f.windows, f.pick) = (windows, None);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                refresh = vector_button(ui, &pal, Icon::Refresh, "Refresh the list", 32.0, false, pal.text).clicked();
            });
        });
        let rows: Vec<(ScreenSource, &str, String)> = if f.windows {
            v.windows.iter().map(|w| (w.source, w.title.as_str(), w.process.clone())).collect()
        } else {
            let detail = |w: u32, h: u32, primary: bool| format!("{w} × {h}{}", if primary { ", main screen" } else { "" });
            v.monitors.iter().map(|m| (m.source, m.name.as_str(), detail(m.width, m.height, m.primary))).collect()
        };
        egui::Frame::new().fill(pal.bg).corner_radius(pal.card_radius()).inner_margin(4).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if rows.is_empty() {
                    let text = if f.windows { "No windows to share. Open one, then refresh the list." } else { "No screens found. Refresh the list." };
                    ui.add_sized(vec2(ui.available_width(), 44.0), egui::Label::new(egui::RichText::new(text).color(pal.muted)));
                }
                for (source, title, detail) in &rows {
                    if source_row(ui, &pal, title, detail, f.pick == Some(*source)).clicked() {
                        f.pick = Some(*source);
                    }
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Chip height first, so the first label is centred like the later ones.
            ui.set_min_height(ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y);
            ui.label(egui::RichText::new("Resolution").color(pal.muted));
            ui.selectable_value(&mut f.hd, false, "720p");
            ui.selectable_value(&mut f.hd, true, "1080p");
            ui.add_space(24.0);
            ui.label(egui::RichText::new("Frame rate").color(pal.muted));
            ui.selectable_value(&mut f.low_fps, true, "15 FPS");
            ui.selectable_value(&mut f.low_fps, false, "30 FPS");
        });
        // A picked source must still be listed (⟳ may have dropped it).
        let ok = connected && f.pick.is_some_and(|p| rows.iter().any(|r| r.0 == p));
        dialog_buttons(ui, theme, "Go live", false, ok)
    });
    if refresh {
        cx.cmd.send(Command::Video(VideoCommand::ListScreenSources));
    }
    match resp.inner {
        Some(true) if let Some(source) = f.pick => {
            let (fps, max_height) = (if f.low_fps { 15 } else { 30 }, if f.hd { 1080 } else { 720 });
            cx.cmd.send(Command::Video(VideoCommand::StartScreenShare { source, fps, max_height, audio: false }));
            cx.state.voice.video.share_starting = true;
            cx.view.modal = None;
        }
        Some(_) => cx.view.modal = None,
        None if resp.should_close() => cx.view.modal = None,
        None => {}
    }
}

/// A 44 px source row: screen icon, title, muted detail; the pick has the `selected` fill and a
/// 2 px `text` ring (never the accent).
fn source_row(ui: &mut egui::Ui, pal: &Pal, title: &str, detail: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
    let fill = if selected {
        pal.tk.selected
    } else if resp.hovered() || resp.has_focus() {
        pal.tk.hover
    } else {
        Color32::TRANSPARENT
    };
    let p = ui.painter();
    let r = pal.radius.saturating_sub(4).max(2);
    p.rect_filled(rect, r, fill);
    if selected {
        p.rect_stroke(rect, r, Stroke::new(2.0, pal.text), StrokeKind::Inside);
    }
    let icon = Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(20.0));
    icons::paint(p, Icon::Screen, icon, if selected { pal.text } else { pal.muted }, fill);
    let max_w = rect.width() - 52.0;
    let t = one_line(ui, title, FontId::new(pal.body(), crate::theme::medium_family()), pal.text, max_w);
    let d = one_line(ui, detail, FontId::proportional(pal.meta()), pal.muted, max_w);
    let top = rect.center().y - (t.size().y + d.size().y) / 2.0;
    let y = top + t.size().y;
    p.galley(pos2(rect.left() + 44.0, top), t, pal.text);
    p.galley(pos2(rect.left() + 44.0, y), d, pal.muted);
    resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, title));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
