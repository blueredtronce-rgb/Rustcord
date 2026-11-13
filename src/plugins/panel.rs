//! User panel plugins: GameActivityToggle (PLUGINS_NOTES S10, a controller button flipping
//! "Share detected activity", settings proto 11.3, one write per click) and
//! AccountPanelServerProfile (S1: in a server the panel shows my server name and avatar, and a
//! click opens that server profile).

use super::{Plugin, enabled};
use crate::api::settings_proto::SettingValue;
use crate::backend::{AccountCommand, Command};
use crate::model::Id;
use crate::state::AppState;
use crate::ui::UiCx;
use egui::{Color32, CornerRadius, Rect, Sense, Stroke, pos2, vec2};

/// Settings proto 11.3 `show_current_game` (BoolValue, default on).
const SHOW_GAME: (u32, &[u32]) = (11, &[3]);

/// Width the plugin buttons take left of the microphone (button and gap), from the `room` my
/// name has without them: none when the name would keep less than 72 px (narrow windows).
pub fn buttons_width(room: f32) -> f32 {
    if enabled(Plugin::GameActivityToggle) && room - 36.0 >= 72.0 { 36.0 } else { 0.0 }
}

/// The controller button in the 32 px `rect` (left of the microphone; empty when there is no
/// room): lit while my game is shared, red and slashed while hidden. A click sends one settings
/// write (none while the last one is on its way).
pub fn game_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>, rect: Rect, panel_fill: Color32) {
    if !enabled(Plugin::GameActivityToggle) || rect.width() < 1.0 {
        return;
    }
    let on = cx.state.account.settings.proto_varint(SHOW_GAME.0, SHOW_GAME.1, true).is_none_or(|v| v != 0);
    let resp = ui.interact(rect, ui.make_persistent_id("rc_game_activity"), Sense::click());
    let c = &cx.theme.colors;
    let hot = resp.hovered() || resp.has_focus();
    if hot {
        // The pill of the panel's own buttons (`sidebar.rs` panel_button).
        let r = CornerRadius::from(cx.theme.layout.corner_radius.clamp(0.0, 16.0));
        ui.painter().rect_filled(rect, r, crate::ui::sidebar::pill_fill(c));
        ui.painter().rect_filled(rect, r, c.text.to_color32().gamma_multiply(0.06));
    }
    let color = if !on {
        c.danger.to_color32()
    } else if hot {
        c.text.to_color32()
    } else {
        c.text.to_color32().gamma_multiply(0.82)
    };
    paint_controller(ui.painter(), rect.shrink(7.0), color, !on, panel_fill);
    let tip = if on { "Hide Game Activity" } else { "Share Game Activity" };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, !on, tip));
    if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        let values = vec![(SHOW_GAME.1.to_vec(), SettingValue::Wrapped(u64::from(!on)))];
        let cmd = AccountCommand::SetSetting { top: SHOW_GAME.0, values: values.clone() };
        // Same path as Settings > Activity Privacy (shown at once, one PATCH).
        if cx.state.account.begin(cmd.op()) {
            cx.state.account.settings.set_local(SHOW_GAME.0, &values);
            cx.cmd.send(Command::Account(cmd));
        }
    }
}

/// A game controller in `r`; `slashed`: crossed out with a gap in `bg` (like the panel's icons).
fn paint_controller(p: &egui::Painter, r: Rect, color: Color32, slashed: bool, bg: Color32) {
    let (c, s) = (r.center(), r.width());
    let body = Rect::from_center_size(c, vec2(s, s * 0.56));
    p.rect_filled(body, s * 0.28, color);
    // D-pad on the left, two buttons on the right, cut out of the body.
    let pad = pos2(body.left() + s * 0.27, c.y);
    p.rect_filled(Rect::from_center_size(pad, vec2(s * 0.26, s * 0.08)), 0.0, bg);
    p.rect_filled(Rect::from_center_size(pad, vec2(s * 0.08, s * 0.26)), 0.0, bg);
    p.circle_filled(pos2(body.right() - s * 0.3, c.y - s * 0.05), s * 0.055, bg);
    p.circle_filled(pos2(body.right() - s * 0.18, c.y + s * 0.07), s * 0.055, bg);
    if slashed {
        let (a, b) = (r.left_top() + vec2(s * 0.08, s * 0.08), r.right_bottom() - vec2(s * 0.08, s * 0.08));
        p.line_segment([a, b], Stroke::new(s * 0.22, bg));
        p.line_segment([a, b], Stroke::new(s * 0.09, color));
    }
}

/// AccountPanelServerProfile: the server whose profile the panel shows (the open one).
pub fn profile_guild(state: &AppState) -> Option<Id> {
    state.selected_guild.filter(|_| enabled(Plugin::AccountPanelServerProfile))
}
