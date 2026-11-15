//! The in-game overlay (Windows, v1): Rustcord's own small windows on top of the game, never
//! inside it: borderless, transparent, always on top, click-through, never focused, not in the
//! taskbar. No injection, no graphics hooks, nothing read from the game; only user32 queries on
//! its window (foreground, client rect, monitor). Exclusive fullscreen bypasses the compositor,
//! so nothing shows there. docs/OVERLAY_DESIGN.md is the spec and the measurements behind it.
//! - `live.rs`: what each pane (voice widget, notifications) shows and where; the pure helpers.
//! - `win32.rs`: the foreground watcher thread, window styling and placement.
//! - `paint.rs`: the deferred viewports' content and the demo screenshots.
//!
//! On Linux and macOS this is a no-op stub (the settings page is hidden there).
//! OWNERSHIP: overlay (`src/ui/overlay/*`, `src/ui/user_settings/overlay.rs`).

#[cfg(windows)]
mod live;
#[cfg(windows)]
mod paint;
#[cfg(windows)]
mod win32;

#[cfg(windows)]
pub use live::Overlay;

/// `RUSTCORD_OVERLAY_DEMO` is set: a screenshot run waits for the panes' own screenshots.
#[cfg(windows)]
pub fn demo_active() -> bool {
    live::demo().is_some()
}

#[cfg(not(windows))]
pub fn demo_active() -> bool {
    false
}

#[cfg(not(windows))]
#[derive(Default)]
pub struct Overlay;

#[cfg(not(windows))]
impl Overlay {
    pub fn logic(&mut self, _ctx: &egui::Context, _state: &crate::state::AppState, _view: &super::ViewState, _settings: &crate::config::Settings, _theme: &crate::theme::Theme, _images: &mut super::images::ImageCache) {}

    pub fn notify(&mut self, _state: &crate::state::AppState, _settings: &crate::config::Settings, _title: &str, _body: &str, _author: Option<crate::model::Id>) -> bool {
        false
    }
}
