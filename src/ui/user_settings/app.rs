//! App settings kept on this device (`config::Settings`, docs/SETTINGS_NOTES.md §3 "L"):
//! Accessibility, Windows Settings, Streamer Mode; and `frame`, the per-frame
//! hook that applies them: zoom (Ctrl +/−/0 included, kept in `Settings::zoom`), reduced motion,
//! the global keybinds (mute, deafen, Streamer Mode), start minimized, close button minimizes.
//! OWNERSHIP: user-settings engineer.

use super::super::messages::Pal;
use super::super::{UiCx, hint, section};
use super::keybinds::{self, Bind};
use super::switch_row;
use crate::backend::Command;
use crate::config::AnimateMode;
use egui::{RichText, Slider};

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

/// Windows Settings: open on startup (the HKCU Run value, written only when toggled), start
/// minimized, close button minimizes.
pub(super) fn windows(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    if !cfg!(windows) {
        hint(ui, cx.theme, "Only on Windows.");
        return false;
    }
    let s = &mut *cx.settings;
    let mut changed = false;
    section(ui, cx.theme, "System startup");
    let open = switch_row(ui, cx.theme, "Open Rustcord when I sign in", None, &mut s.open_on_startup);
    let theme = &*cx.theme;
    let minimized = ui
        .add_enabled_ui(s.open_on_startup, |ui| switch_row(ui, theme, "Start minimized", None, &mut s.start_minimized))
        .inner;
    if open || minimized {
        match run_at_sign_in(s.open_on_startup, s.start_minimized) {
            Ok(()) => changed = true,
            Err(e) => {
                // Not written: show the switch as it is.
                s.open_on_startup ^= open;
                s.start_minimized ^= minimized;
                cx.view.toast(format!("Could not change the startup entry: {e}"), true);
            }
        }
    }
    section(ui, cx.theme, "Window");
    changed |= switch_row(ui, cx.theme, "Close button minimizes", Some("Quit from the taskbar's right-click menu then."), &mut s.close_minimizes);
    changed
}

/// The HKCU `…\CurrentVersion\Run` value "Rustcord" (via reg.exe: no registry feature needed for
/// one write on a click): this exe, with `--minimized` when `minimized`; removed when `!on`.
fn run_at_sign_in(on: bool, minimized: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new("reg");
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let data = format!("\"{}\"{}", exe.display(), if minimized { " --minimized" } else { "" });
            cmd.args(["add", KEY, "/v", crate::config::APP_NAME, "/t", "REG_SZ", "/d", &data, "/f"]);
        } else {
            cmd.args(["delete", KEY, "/v", crate::config::APP_NAME, "/f"]);
        }
        let out = cmd.creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
        // Removing a value that is already gone is fine.
        if !out.status.success() && on {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        Ok(())
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
    // Started at sign-in (`--minimized` in the Run value): to the taskbar on the first frame.
    let started_id = egui::Id::unique("rc_started");
    if !ctx.data(|d| d.get_temp::<bool>(started_id).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(started_id, true));
        if std::env::args().any(|a| a == "--minimized") {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }
    exclude_from_capture(ctx, cx.settings.streamer.enabled && cx.settings.streamer.hide_from_capture);
    if cx.settings.close_minimizes && ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
    if save
        && let Err(e) = cx.settings.save()
    {
        cx.view.toast(format!("Could not save settings: {e}"), true);
    }
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
        let (mute, deaf) = (cx.view.self_mute, cx.view.self_deaf);
        let (m, d) = super::super::sidebar::toggle_voice(mute, deaf, mic);
        (cx.view.self_mute, cx.view.self_deaf) = (m, d);
        if d != deaf {
            cx.cmd.send(Command::SetSelfDeaf(d));
        }
        if m != mute {
            cx.cmd.send(Command::SetSelfMute(m));
        }
    }
    if streamer {
        let s = &mut cx.settings.streamer;
        s.enabled = !s.enabled;
        cx.view.toast(if s.enabled { "Streamer Mode on" } else { "Streamer Mode off" }.to_owned(), false);
    }
    streamer
}
