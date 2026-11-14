//! App settings kept on this device (`config::Settings`, docs/SETTINGS_NOTES.md §3 "L"):
//! Accessibility, Windows Settings, Streamer Mode; and `frame`, the per-frame
//! hook that applies them: zoom (Ctrl +/−/0 included, kept in `Settings::zoom`), reduced motion,
//! the global keybinds (mute, deafen, Streamer Mode); `logic`, the window's tray life (close to
//! the tray, tray menu, `--minimized`), which also runs while the window is hidden.
//! OWNERSHIP: user-settings engineer; Windows Settings and `logic`: desktop engineer.

use super::super::messages::Pal;
use super::super::{UiCx, ViewState, hint, section};
use super::keybinds::{self, Bind};
use super::switch_row;
use crate::backend::{Command, CommandSender};
use crate::config::{AnimateMode, Settings};
use crate::notify::TrayAction;
use egui::{RichText, Slider, ViewportCommand};

/// Accessibility (this device): saturation, role colours, chat text size, zoom, message
/// spacing, motion. True when the settings changed.
pub(super) fn accessibility(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    let pal = Pal::new(cx.theme);
    let theme_size = pal.size;
    let s = &mut *cx.settings;
    let mut changed = false;
    ui.spacing_mut().slider_width = 320.0;

    section(ui, cx.theme, "Saturation");
    let mut pct = (s.saturation * 100.0).round();
    let r = ui.add(Slider::new(&mut pct, 0.0..=100.0).step_by(5.0).fixed_decimals(0).suffix(" %"));
    if r.changed() {
        // Live while dragging; every colour goes through it, so the visuals are re-applied too.
        s.saturation = pct / 100.0;
        s.publish();
        cx.view.theme_dirty = true;
    }
    changed |= released(&r);
    section(ui, cx.theme, "Role colours");
    changed |= switch_row(ui, cx.theme, "Show role colours in names", None, &mut s.role_colors);

    section(ui, cx.theme, "Chat font scaling");
    let mut size = s.chat_font_size.unwrap_or(theme_size).round();
    ui.horizontal(|ui| {
        let r = ui.add(Slider::new(&mut size, 12.0..=24.0).step_by(1.0).fixed_decimals(0).suffix(" px"));
        if r.changed() {
            s.chat_font_size = Some(size);
        }
        changed |= released(&r);
        if s.chat_font_size.is_some() && ui.link("Theme size").on_hover_text("Use the theme's text size").clicked() {
            s.chat_font_size = None;
            changed = true;
        }
    });

    section(ui, cx.theme, "Zoom level");
    // Applied when let go (zooming while dragging would move the slider under the pointer):
    // the dragged value waits in temp memory meanwhile.
    let drag_id = egui::Id::unique("rc_zoom_slider");
    let mut zoom = ui.data(|d| d.get_temp(drag_id)).unwrap_or((s.zoom * 100.0).round());
    let r = ui.add(Slider::new(&mut zoom, 50.0..=200.0).step_by(10.0).fixed_decimals(0).suffix(" %"));
    if r.dragged() {
        ui.data_mut(|d| d.insert_temp(drag_id, zoom));
    } else {
        ui.data_mut(|d| d.remove::<f32>(drag_id));
        if released(&r) {
            s.zoom = zoom / 100.0;
            changed = true;
        }
    }
    hint(ui, cx.theme, "Ctrl + / Ctrl − zoom, Ctrl 0 resets.");

    section(ui, cx.theme, "Space between message groups");
    ui.horizontal(|ui| {
        for px in [0.0, 4.0, 8.0, 16.0, 24.0] {
            changed |= ui.selectable_value(&mut s.message_group_spacing, px, RichText::new(format!("{px} px")).color(pal.text)).changed();
        }
    });

    section(ui, cx.theme, "Motion");
    if switch_row(ui, cx.theme, "Reduced motion", Some("No UI animations; animated avatars, emoji and effects only while hovered."), &mut s.reduced_motion) {
        changed = true;
        if s.reduced_motion {
            s.animate_others = AnimateMode::Hover;
        }
    }
    let mut always = s.animate_others == AnimateMode::Always;
    let enabled = !s.reduced_motion;
    let theme = &*cx.theme;
    if ui.add_enabled_ui(enabled, |ui| switch_row(ui, theme, "Play animated avatars, emoji and GIFs", Some("Off: they play while hovered."), &mut always)).inner {
        s.animate_others = if always { AnimateMode::Always } else { AnimateMode::Hover };
        changed = true;
    }
    if changed {
        s.publish();
    }
    changed
}

/// A slider was let go, or changed without a drag (keys): the moment to save.
fn released(r: &egui::Response) -> bool {
    r.drag_stopped() || (r.changed() && !r.dragged())
}

/// Windows Settings: open at sign-in and start minimized (the HKCU Run value itself, read each
/// time the page draws: the installer or another copy may have changed it), minimize to tray.
pub(super) fn windows(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    if !cfg!(windows) {
        hint(ui, cx.theme, "Only on Windows.");
        return false;
    }
    section(ui, cx.theme, "System startup");
    let run = autostart();
    let mut open = run.is_some();
    let mut minimized = run.as_deref().is_none_or(|cmd| cmd.contains("--minimized"));
    let mut toggled = switch_row(ui, cx.theme, "Open Rustcord when I sign in to Windows", None, &mut open);
    let theme = &*cx.theme;
    toggled |= ui
        .add_enabled_ui(open, |ui| switch_row(ui, theme, "Start minimized", Some("It starts in the notification area."), &mut minimized))
        .inner;
    if toggled && let Err(e) = set_autostart(open, minimized) {
        cx.view.toast(format!("Could not change the startup entry: {e}"), true);
    }
    section(ui, cx.theme, "Window");
    switch_row(
        ui,
        cx.theme,
        "Minimize to tray when I close the window",
        Some("Rustcord keeps running in the notification area. Quit it from the tray icon."),
        &mut cx.settings.minimize_to_tray,
    )
}

/// The command line of the HKCU Run value "Rustcord", if there is one.
fn autostart() -> Option<String> {
    #[cfg(windows)]
    {
        crate::winreg::autostart(crate::config::APP_NAME)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Writes the Run value for this exe (`--minimized` when `minimized`), or removes it.
fn set_autostart(on: bool, minimized: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::winreg::set_autostart(crate::config::APP_NAME, on.then_some(exe.as_path()), minimized)
    }
    #[cfg(not(windows))]
    {
        let _ = (on, minimized);
        Err("only on Windows".into())
    }
}

/// Streamer Mode (no "Disable sounds": Rustcord plays none; no OBS auto-enable: no process scan).
pub(super) fn streamer(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    let s = &mut cx.settings.streamer;
    let mut changed = switch_row(ui, cx.theme, "Enable Streamer Mode", Some("Also on a keybind (Keybinds)."), &mut s.enabled);
    section(ui, cx.theme, "Options");
    let theme = &*cx.theme;
    ui.add_enabled_ui(s.enabled, |ui| {
        changed |= switch_row(ui, theme, "Hide personal information", Some("Your username, connections and who messaged you."), &mut s.hide_personal_info);
        changed |= switch_row(ui, theme, "Hide invite links", Some("Copy still works."), &mut s.hide_invite_links);
        changed |= switch_row(ui, theme, "Disable notifications", None, &mut s.disable_notifications);
        if cfg!(windows) {
            changed |= switch_row(ui, theme, "Hide window from screen capture", Some("Others see a blank area where Rustcord is."), &mut s.hide_from_capture);
        }
    });
    changed
}

/// Advanced (this device).
pub(super) fn advanced(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    section(ui, cx.theme, "Hardware acceleration");
    hint(ui, cx.theme, "Always on: Rustcord draws with the GPU (OpenGL).");
}

// ---------------------------------------------------------------------------
// Per frame (RustcordApp::ui, every screen)
// ---------------------------------------------------------------------------

/// egui's default `Style::animation_time`.
const ANIMATION_TIME: f32 = 0.2;

/// Applies the settings that act every frame. Saves when a key changed one.
pub(in crate::ui) fn frame(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    let mut save = sync_zoom(ctx, cx);
    let want = if cx.settings.reduced_motion { 0.0 } else { ANIMATION_TIME };
    if ctx.global_style().animation_time != want {
        ctx.all_styles_mut(|s| s.animation_time = want);
    }
    // Signed in only; recording a shortcut on the Keybinds page takes every key.
    if cx.view.screen == super::super::Screen::Main && !keybinds::recording(ctx) {
        save |= global_keys(ctx, cx);
    }
    exclude_from_capture(ctx, cx.settings.streamer.enabled && cx.settings.streamer.hide_from_capture);
    if save
        && let Err(e) = cx.settings.save()
    {
        cx.view.toast(format!("Could not save settings: {e}"), true);
    }
}

/// The window's tray life, every pass (`RustcordApp::logic`), also while it is hidden: tray
/// menu clicks, closing to the tray (× and Alt+F4) unless quitting, `--minimized` at startup;
/// and the mute state for the tray menu's check marks.
pub(in crate::ui) fn logic(ctx: &egui::Context, view: &mut ViewState, cmd: &CommandSender, settings: &mut Settings) {
    let quitting = egui::Id::unique("rc_quitting");
    let started = egui::Id::unique("rc_started");
    if !ctx.data(|d| d.get_temp::<bool>(started).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(started, true));
        // Started at sign-in (`--minimized` in the Run value): straight to the tray.
        if crate::notify::has_tray() && std::env::args().any(|a| a == "--minimized") {
            to_tray(ctx);
        }
    }
    for action in crate::notify::take_actions() {
        match action {
            TrayAction::Show => {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            TrayAction::ToggleMute | TrayAction::ToggleDeafen => toggle_voice(view, cmd, action == TrayAction::ToggleMute),
            TrayAction::Quit => {
                ctx.data_mut(|d| d.insert_temp(quitting, true));
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }
    let quit = ctx.data(|d| d.get_temp::<bool>(quitting).unwrap_or(false));
    // Without a tray icon (screenshot runs, a failed icon) closing quits as before.
    if !quit && settings.minimize_to_tray && crate::notify::has_tray() && ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        to_tray(ctx);
        if !settings.tray_hint_shown {
            settings.tray_hint_shown = true;
            if let Err(e) = settings.save() {
                view.toast(format!("Could not save settings: {e}"), true);
            }
            crate::notify::show(crate::config::APP_NAME, "Rustcord is still running in the tray. Quit it from the tray icon.");
        }
    }
    crate::notify::set_voice_state(view.self_mute, view.self_deaf);
}

/// Hides the window, taskbar button included. Minimized as well, so eframe runs only `logic`
/// (no ui pass, no painting) while it waits in the tray.
fn to_tray(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
}

/// Streamer Mode's "Hide window from screen capture": the window's display affinity follows
/// `on`, set when it changes (retried each frame until the window is active: the UI thread's
/// active window is ours then, no window handle needed).
fn exclude_from_capture(ctx: &egui::Context, on: bool) {
    let id = egui::Id::unique("rc_capture_hidden");
    if ctx.data(|d| d.get_temp::<bool>(id).unwrap_or(false)) == on {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};
        // SAFETY: plain Win32 calls on this thread's own active window (null: not active yet).
        let done = unsafe {
            let hwnd = GetActiveWindow();
            !hwnd.is_null() && SetWindowDisplayAffinity(hwnd, if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE }) != 0
        };
        if done {
            ctx.data_mut(|d| d.insert_temp(id, on));
        }
    }
}

/// `Settings::zoom` -> egui, and egui's own Ctrl +/−/0 -> `Settings::zoom` (true: save).
fn sync_zoom(ctx: &egui::Context, cx: &mut UiCx<'_>) -> bool {
    let id = egui::Id::unique("rc_zoom_applied");
    let applied: Option<f32> = ctx.data(|d| d.get_temp(id));
    let want = cx.settings.zoom.clamp(0.5, 2.0);
    let (set, adopt) = match applied {
        // Startup, or the slider moved: apply (egui takes it at the next pass).
        None => (true, false),
        Some(a) if a != want => (true, false),
        // A zoom key changed egui's factor since.
        Some(a) => (false, (ctx.zoom_factor() - a).abs() > 1e-3),
    };
    if set {
        ctx.set_zoom_factor(want);
        ctx.data_mut(|d| d.insert_temp(id, want));
    } else if adopt {
        let z = (ctx.zoom_factor() * 100.0).round() / 100.0;
        cx.settings.zoom = z;
        ctx.data_mut(|d| d.insert_temp(id, z));
        if z != ctx.zoom_factor() {
            ctx.set_zoom_factor(z);
        }
    }
    adopt
}

/// Toggle Mute / Deafen (like the user panel's buttons) and Toggle Streamer Mode. True when a
/// setting changed (save).
fn global_keys(ctx: &egui::Context, cx: &mut UiCx<'_>) -> bool {
    let pressed = |b| keybinds::shortcut(cx.settings, b).is_some_and(|k| ctx.input_mut(|i| i.consume_shortcut(&k)));
    let (mic, phones, streamer) = (pressed(Bind::ToggleMute), pressed(Bind::ToggleDeafen), pressed(Bind::ToggleStreamerMode));
    if mic || phones {
        toggle_voice(cx.view, cx.cmd, mic);
    }
    if streamer {
        let s = &mut cx.settings.streamer;
        s.enabled = !s.enabled;
        cx.view.toast(if s.enabled { "Streamer Mode on" } else { "Streamer Mode off" }.to_owned(), false);
    }
    streamer
}

/// Mute (`mic`) or Deafen toggled like the user panel's buttons (keybind, tray menu).
fn toggle_voice(view: &mut ViewState, cmd: &CommandSender, mic: bool) {
    let (mute, deaf) = (view.self_mute, view.self_deaf);
    let (m, d) = super::super::sidebar::toggle_voice(mute, deaf, mic);
    (view.self_mute, view.self_deaf) = (m, d);
    if d != deaf {
        cmd.send(Command::SetSelfDeaf(d));
    }
    if m != mute {
        cmd.send(Command::SetSelfMute(m));
    }
}
