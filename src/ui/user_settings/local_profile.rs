//! Settings > Local profile: my banner and (animated) avatar, only visible in this client on
//! this device; reset buttons; avatar animation mode.
//! OWNERSHIP: user-settings engineer (docs/SOCIAL_UI_DESIGN.md).

use crate::ui::{UiCx, avatar_at, profile::cover_uv};
use crate::config::AnimateMode;
use crate::overrides::{self, Kind};
use egui::{Button, Image, RichText, Sense, Vec2, vec2};

/// Same size as the banner on the profile card.
const BANNER: Vec2 = vec2(300.0, 106.0);
const AVATAR: f32 = 80.0;
const EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "apng"];

pub fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let (size, muted) = (cx.theme.font.size - 1.5, cx.theme.colors.text_muted.to_color32());
    let hint = move |ui: &mut egui::Ui, text: &str| ui.label(RichText::new(text).size(size).color(muted));
    ui.horizontal_wrapped(|ui| {
        hint(ui, "Only changes how you see yourself in Rustcord on this device. Nothing is uploaded.");
        super::folder_button(ui, cx, &crate::config::paths().overrides_dir(), "Your images are copied here");
    });
    let always = cx.overrides.avatar_animate == AnimateMode::Always;
    let radius = cx.theme.layout.corner_radius;
    let placeholder = cx.theme.colors.surface.to_color32();

    ui.add_space(8.0);
    ui.label("Profile banner");
    if let Some(url) = cx.overrides.url(Kind::Banner) {
        let (rect, _) = ui.allocate_exact_size(BANNER, Sense::hover());
        let animate = always || ui.rect_contains_pointer(rect);
        match cx.images.get(ui.ctx(), &url, 600, animate) {
            Some(tex) => Image::from_texture(tex).uv(cover_uv(tex.size, rect.size())).corner_radius(radius).paint_at(ui, rect),
            None => {
                ui.painter().rect_filled(rect, radius, placeholder);
            }
        }
    }
    buttons(ui, cx, Kind::Banner);

    ui.add_space(8.0);
    ui.label("Avatar");
    ui.horizontal(|ui| {
        if let Some(url) = cx.overrides.url(Kind::Avatar) {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(AVATAR), Sense::hover());
            let animate = always || ui.rect_contains_pointer(rect);
            avatar_at(ui, cx.images, &overrides::large(&url), rect, animate, 1.0);
        }
        ui.vertical(|ui| {
            buttons(ui, cx, Kind::Avatar);
            hint(ui, "GIF, APNG and animated WebP play; any still image works too.");
            ui.horizontal_wrapped(|ui| {
                ui.label("Animate");
                let a = ui.radio_value(&mut cx.overrides.avatar_animate, AnimateMode::Always, "Always");
                let h = ui.radio_value(&mut cx.overrides.avatar_animate, AnimateMode::Hover, "Only on hover (saves CPU)");
                if a.changed() || h.changed() {
                    commit(cx);
                }
            });
        });
    });
}

fn buttons(ui: &mut egui::Ui, cx: &mut UiCx<'_>, kind: Kind) {
    ui.horizontal(|ui| {
        if ui.button("Choose image…").clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("Images", &EXTENSIONS).pick_file()
        {
            // Blocking copy + decode check: a short pause on an explicit user action.
            match overrides::import_image(&path, kind) {
                Ok(name) => set(cx, kind, Some(name)),
                Err(e) => cx.view.toast(format!("Could not use this image: {e}"), true),
            }
        }
        if ui.add_enabled(cx.overrides.url(kind).is_some(), Button::new("Reset")).clicked() {
            set(cx, kind, None);
        }
    });
}

/// Replace or clear an override; the old file goes with the next successful save (`commit`).
fn set(cx: &mut UiCx<'_>, kind: Kind, name: Option<String>) {
    let old_url = cx.overrides.url(kind);
    let old = cx.overrides.replace(kind, name);
    commit(cx);
    if let (Some(_), Some(url)) = (old, old_url) {
        cx.images.remove(&overrides::large(&url));
        cx.images.remove(&url);
    }
}

/// Publish the overrides to the rest of the UI and save them (toast if saving failed); once
/// saved, delete image files nothing references any more.
fn commit(cx: &mut UiCx<'_>) {
    overrides::publish(cx.state.me.as_ref(), cx.overrides);
    match cx.overrides.save() {
        Ok(()) => overrides::prune(cx.overrides),
        Err(e) => cx.view.toast(format!("Could not save local profile: {e}"), true),
    }
}
