//! Settings > Plugins (Vencord's layout): search, All / Enabled / Disabled, a two-column grid (one
//! in narrow windows) of cards (name, one line of description, switch; a gear opens the plugin's
//! options in the card).
//! Values live in plugins.toml (`crate::plugins`), not in `config::Settings`.
//! OWNERSHIP: plugins engineer.

use super::super::messages::{Pal, icon_button};
use super::{switch, switch_row};
use crate::plugins::{self, META, Meta, Opt, OptKind};
use crate::ui::{UiCx, bold, hint};
use egui::{Align, ComboBox, Frame, Label, Layout, Margin, RichText, Slider, Stroke, TextEdit, vec2};

const FILTERS: [&str; 3] = ["All", "Enabled", "Disabled"];

fn query_id() -> egui::Id {
    egui::Id::unique("rc_plugins_query")
}

fn filter_id() -> egui::Id {
    egui::Id::unique("rc_plugins_filter")
}

fn open_id(m: &Meta) -> egui::Id {
    egui::Id::unique(("rc_plugin_options", m.name))
}

/// Whether a plugin matches the search text (name or description, any case).
fn matches(m: &Meta, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || m.name.to_lowercase().contains(&q) || m.description.to_lowercase().contains(&q)
}

pub fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let pal = Pal::new(cx.theme);
    hint(ui, cx.theme, "Vencord's plugins, rebuilt into Rustcord. They only change this app; none sends anything on its own.");
    ui.add_space(8.0);
    let mut q: String = ui.data(|d| d.get_temp(query_id())).unwrap_or_default();
    let mut filter: usize = ui.data(|d| d.get_temp(filter_id())).unwrap_or(0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let search_w = (ui.available_width() - 240.0).max(160.0);
        Frame::new().fill(pal.surface).corner_radius(pal.radius).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.add(TextEdit::singleline(&mut q).hint_text("Search plugins").frame(Frame::NONE).desired_width(search_w - 20.0));
        });
        for (i, label) in FILTERS.iter().enumerate() {
            let selected = filter == i;
            let text = RichText::new(*label).color(if selected { pal.text } else { pal.muted });
            if ui.add(egui::Button::new(text).selected(selected).frame_when_inactive(false)).clicked() {
                filter = i;
            }
        }
    });
    ui.data_mut(|d| {
        d.insert_temp(query_id(), q.clone());
        d.insert_temp(filter_id(), filter);
    });
    let on_count = META.iter().filter(|m| plugins::enabled(m.id)).count();
    let list: Vec<&Meta> = META
        .iter()
        .filter(|m| matches(m, &q))
        .filter(|m| match filter {
            1 => plugins::enabled(m.id),
            2 => !plugins::enabled(m.id),
            _ => true,
        })
        .collect();
    ui.add_space(4.0);
    hint(ui, cx.theme, &format!("{on_count} of {} on", META.len()));
    ui.add_space(8.0);
    if list.is_empty() {
        ui.label(RichText::new("No plugin matches.").color(pal.muted));
        return;
    }
    let gap = 12.0;
    // One column once two would make cards narrower than ~270 px.
    let cols = if ui.available_width() < 560.0 { 1 } else { 2 };
    let col_w = ((ui.available_width() - gap * (cols - 1) as f32) / cols as f32).floor();
    for pair in list.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for m in pair {
                ui.allocate_ui_with_layout(vec2(col_w, 0.0), Layout::top_down(Align::Min), |ui| card(ui, cx, &pal, m));
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
}

fn card(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, m: &Meta) {
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id(m))).unwrap_or(false);
    let mut on = plugins::enabled(m.id);
    Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0, if on { pal.accent.gamma_multiply(0.45) } else { pal.line() }))
        .corner_radius(pal.card_radius())
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(72.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            // Switch and gear first (right to left), the name gets what is left (elided, the full
            // name on hover), so narrow cards never draw controls over it.
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if switch(ui, pal, &mut on, m.name).changed() {
                        if !on && m.id == plugins::Plugin::MessageLogger {
                            plugins::message_logger::purge(cx.state);
                        }
                        save(cx, plugins::set(m.id, "enabled", toml::Value::Boolean(on)));
                    }
                    if !m.opts.is_empty() && icon_button(ui, pal, "⚙", "Options", 24.0, open, pal.text).clicked() {
                        open = !open;
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(Label::new(RichText::new(m.name).font(bold(pal.size)).color(pal.text)).truncate());
                    });
                });
            });
            ui.add(Label::new(RichText::new(m.description).size(pal.meta()).color(pal.muted)).wrap());
            if open {
                ui.separator();
                for o in m.opts {
                    option(ui, cx, pal, m, o);
                }
            }
        });
    ui.data_mut(|d| d.insert_temp(open_id(m), open));
}

fn save(cx: &mut UiCx<'_>, r: Result<(), String>) {
    if let Err(e) = r {
        cx.view.toast(format!("Could not save plugin settings: {e}"), true);
    }
}

fn option(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, m: &Meta, o: &Opt) {
    let p = m.id;
    match o.kind {
        OptKind::Bool(_) => {
            let mut v = plugins::opt_bool(p, o.key);
            if switch_row(ui, cx.theme, o.label, None, &mut v) {
                save(cx, plugins::set(p, o.key, toml::Value::Boolean(v)));
            }
        }
        OptKind::Choice(list, _) => {
            let cur = plugins::opt_choice(p, o.key);
            ui.horizontal(|ui| {
                ui.label(RichText::new(o.label).color(pal.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ComboBox::from_id_salt((m.name, o.key)).selected_text(list[cur]).show_ui(ui, |ui| {
                        for (i, c) in list.iter().enumerate() {
                            if ui.selectable_label(cur == i, *c).clicked() && cur != i {
                                save(cx, plugins::set(p, o.key, toml::Value::Integer(i as i64)));
                            }
                        }
                    });
                });
            });
        }
        OptKind::Int(lo, hi, _) => {
            let mut v = plugins::opt_int(p, o.key);
            ui.label(RichText::new(o.label).color(pal.text));
            let resp = ui.add(Slider::new(&mut v, lo..=hi));
            if resp.changed() {
                plugins::put(p, o.key, toml::Value::Integer(v));
            }
            // Written once the drag ends, not on every frame of it.
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                save(cx, plugins::save());
            }
        }
        OptKind::Text(_, hint_text) => {
            let mut v = plugins::opt_text(p, o.key);
            ui.label(RichText::new(o.label).color(pal.text));
            let resp = ui.add(TextEdit::singleline(&mut v).hint_text(hint_text).desired_width(f32::INFINITY));
            if resp.changed() {
                plugins::put(p, o.key, toml::Value::String(v));
            }
            if resp.lost_focus() {
                save(cx, plugins::save());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_name_or_description() {
        let m = &META[0];
        assert!(matches(m, ""));
        assert!(matches(m, &m.name.to_uppercase()));
        assert!(!matches(m, "zzzz-no-plugin"));
        assert!(META.iter().filter(|m| matches(m, "url")).count() >= 2);
    }
}
