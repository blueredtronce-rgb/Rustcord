//! The content of a pane's window (an egui deferred viewport callback): place the window,
//! show it, draw the scene. Runs on the UI thread in the overlay's own pass, so the root UI
//! does not re-run for it. docs/OVERLAY_DESIGN.md §5.3 and §6.

use super::live::{Colors, GAP, NAME_MAX, PAD, PaneState, RING, Row, SHOTS_PENDING, Scene, TOAST_H, ToastRow, VoiceScene, Want, dims, place_want, slots, visible};
use super::win32;
use crate::config::{Corner, Show};
use crate::ui::icons::{self, Icon};
use crate::ui::images::paint_initial;
use egui::load::SizedTexture;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, FontId, Rect, Stroke, StrokeKind, TextureId, Vec2, pos2, vec2};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A demo screenshot waits this long after the window was created, so avatars and fonts are in.
const SHOT_AFTER: Duration = Duration::from_secs(1);

pub(super) fn show(ui: &mut egui::Ui, shared: &Arc<Mutex<PaneState>>) {
    let ctx = ui.ctx().clone();
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hwnd) = win32::own_window(s.title) else { return };
    let new = win32::style(hwnd, s.want.hide_from_capture, &mut s.styled);
    // The overlay's own scale (the game monitor's DPI and the zoom): the content in points fits exactly.
    let target = place_want(&s.want, ctx.pixels_per_point());
    if new || (win32::client_rect(hwnd) != target && s.tries < 4) {
        // A monitor with another DPI makes winit resize once more; the next pass corrects it, and the cap ends any loop.
        s.tries += 1;
        win32::set_client_rect(hwnd, target);
        ctx.request_repaint();
    } else if !s.shown {
        // The extra pass after showing re-adds the styles winit's show rewrites.
        s.shown = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }
    s.last_paint = Some(Instant::now());
    draw(ui, &s.want);
    if s.shown && s.shot.is_some() {
        let wait = SHOT_AFTER.saturating_sub(s.created.elapsed());
        if wait.is_zero() {
            let path = s.shot.take().unwrap_or_default();
            let ctx2 = ctx.clone();
            ctx.request_screenshot(move |img| {
                save(&path, &img);
                SHOTS_PENDING.fetch_sub(1, Ordering::Relaxed);
                ctx2.request_repaint_of(egui::ViewportId::ROOT);
            });
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(wait);
        }
    }
}

/// The PNG over mid grey (premultiplied pixels), so white text and dark pills both read.
fn save(path: &std::path::Path, img: &egui::ColorImage) {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|c| {
            let [r, g, b, a] = c.to_array();
            let under = |v: u8| (u16::from(v) + 128 * (255 - u16::from(a)) / 255) as u8;
            [under(r), under(g), under(b), 255]
        })
        .collect();
    if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        && let Err(e) = buf.save(path)
    {
        eprintln!("rustcord: overlay screenshot {}: {e}", path.display());
    }
}

fn draw(ui: &egui::Ui, w: &Want) {
    let rect = ui.max_rect();
    match &w.scene {
        Scene::Voice(v) => draw_voice(ui, rect, w.corner, v, &w.colors),
        Scene::Toasts(rows) => draw_toasts(ui, rect, rows, &w.colors),
    }
}

fn avatar(ui: &egui::Ui, r: Rect, tex: Option<TextureId>, seed: u64, name: &str) {
    let p = ui.painter();
    match tex {
        Some(tex) => egui::Image::from_texture(SizedTexture::new(tex, Vec2::splat(r.width()))).corner_radius(r.width() / 2.0).paint_at(ui, r),
        None => paint_initial(p, r, r.width() / 2.0, seed, name, true),
    }
}

fn draw_voice(ui: &egui::Ui, rect: Rect, corner: Corner, v: &VoiceScene, pal: &Colors) {
    let p = ui.painter();
    let (a, row_h, pill_h, font) = dims(v.large);
    let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
    let bottom = matches!(corner, Corner::BottomLeft | Corner::BottomRight);
    let (rows, more) = visible(v);
    // Rows pack toward the corner's edge: from the top, or so the last one touches the bottom.
    let used = rows.len() + usize::from(more > 0);
    let base = if bottom { slots(v.rows.len(), v.users).saturating_sub(used) } else { 0 };
    let slot_w = PAD + a + 2.0 * RING;
    let avatar_x = if right { rect.right() - PAD - RING - a } else { rect.left() + PAD + RING };
    // A pill hugs its text; right corners mirror everything.
    let pill_at = |w: f32, cy: f32| {
        let x = if right { rect.right() - slot_w - GAP - w } else { rect.left() + slot_w + GAP };
        Rect::from_min_size(pos2(x, cy - pill_h / 2.0), vec2(w, pill_h))
    };
    let pill = |text: String, color: Color32, cy: f32| {
        let galley = crate::ui::one_line(ui, text, FontId::new(font, crate::theme::medium_family()), color, NAME_MAX - 16.0);
        let r = pill_at((galley.size().x + 16.0).min(NAME_MAX), cy);
        p.rect_filled(r, 4, pal.pill);
        p.galley(pos2(r.left() + 8.0, r.center().y - galley.size().y / 2.0), galley, color);
    };
    let row_cy = |slot: usize| rect.top() + PAD + (slot as f32 + 0.5) * row_h;
    for (i, row) in rows.iter().enumerate() {
        let cy = row_cy(base + i);
        let r = Rect::from_center_size(pos2(avatar_x + a / 2.0, cy), Vec2::splat(a));
        avatar(ui, r, row.tex, row.id.0, &row.name);
        if row.speaking {
            p.circle_stroke(r.center(), a / 2.0 + 2.0, Stroke::new(2.0, pal.positive));
        }
        if row.muted || row.deaf {
            mark(p, r, row, a, pal);
        }
        if v.names == Show::Always || (v.names == Show::Speaking && row.speaking) {
            pill(row.name.clone(), pal.on_media, cy);
        }
    }
    if more > 0 && v.names != Show::Never {
        pill(format!("{more} more"), pal.on_media_muted, row_cy(base + rows.len()));
    }
}

/// The muted or deafened mark on an avatar's lower right corner; red when a moderator did it.
fn mark(p: &egui::Painter, avatar: Rect, row: &Row, a: f32, pal: &Colors) {
    let c = avatar.max - Vec2::splat(a / 4.0 - 2.0);
    p.circle_filled(c, a / 4.0, pal.pill);
    let icon = if row.deaf { Icon::Deafened } else { Icon::MicOff };
    let color = if row.by_server { pal.danger } else { pal.on_media };
    icons::paint(p, icon, Rect::from_center_size(c, Vec2::splat(a / 2.0 - 4.0)), color, Color32::from_gray(30));
}

fn draw_toasts(ui: &egui::Ui, rect: Rect, rows: &[ToastRow], pal: &Colors) {
    let p = ui.painter();
    for (i, t) in rows.iter().enumerate() {
        let card = Rect::from_min_size(pos2(rect.left(), rect.top() + i as f32 * (TOAST_H + GAP)), vec2(rect.width(), TOAST_H));
        p.rect_filled(card, pal.card_r, pal.card);
        // High Contrast outlines every surface a little stronger (docs/DESIGN.md §6).
        let stroke = if pal.hc { Stroke::new(1.5, pal.muted) } else { Stroke::new(1.0, pal.rule) };
        p.rect_stroke(card, pal.card_r, stroke, StrokeKind::Inside);
        // Streamer Mode passes no author: the card then shows the app's "R".
        avatar(ui, Rect::from_min_size(card.min + vec2(12.0, 12.0), Vec2::splat(32.0)), t.tex, t.seed, &t.title);
        let title = crate::ui::one_line(ui, t.title.clone(), crate::ui::semibold(14.0), pal.text, 252.0);
        p.galley(card.min + vec2(56.0, 12.0), title, pal.text);
        let mut job = LayoutJob { break_on_newline: false, ..Default::default() };
        crate::theme::append_boxed(&mut job, &t.body, &TextFormat::simple(FontId::proportional(14.0), pal.muted));
        job.wrap = TextWrapping { max_width: 252.0, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
        p.galley(card.min + vec2(56.0, 34.0), p.layout_job(job), pal.muted);
    }
}
