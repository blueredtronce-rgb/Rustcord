//! Message search (FEATURES_NOTES §1.5, layout §4.5 / §4.9): the chat header's search box
//! (Ctrl+F, Enter searches the server or the DM) and the results panel that replaces the member
//! list, or the chat in narrow windows: hits grouped by channel, 25 a page, newest / oldest /
//! relevant, the "still indexing" state with a retry, click = jump.
//! OWNERSHIP: ui-messages implementer.

use super::messages::{channel_prefix, icon_button, jump_to, menu_item, menu_setup, message_card, small_button, MenuIcon, Pal};
use super::{UiCx, MEMBER_LIST_MIN_WINDOW_W};
use crate::backend::{ChatCommand, Command, SearchOutcome, SearchQuery, SearchScope, SearchSort};
use crate::model::{Id, Message, SearchPage};
use crate::state::chat::SearchView;
use crate::state::AppState;
use crate::theme::bold_family;
use egui::{pos2, vec2, Align, Align2, FontId, Frame, Label, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, UiBuilder, Vec2};

/// Hits per page (the API's `limit` maximum).
const PAGE: u32 = 25;
/// Largest `offset` the API takes.
const MAX_OFFSET: u32 = 9975;
const PANEL_W: f32 = 418.0;

pub fn is_open(state: &AppState) -> bool {
    state.chat.search.is_some()
}

/// Too narrow for the panel beside the chat (the member list's threshold): it covers the chat.
pub(super) fn narrow(ctx: &egui::Context) -> bool {
    ctx.content_rect().width() < MEMBER_LIST_MIN_WINDOW_W
}

/// Where a search from the open channel runs: its server, or the DM itself.
pub(super) fn scope(state: &AppState) -> Option<SearchScope> {
    let ch = state.selected_channel?;
    Some(match state.channels.get(&ch)?.guild_id {
        Some(g) => SearchScope::Guild(g),
        None => SearchScope::Dm(ch),
    })
}

fn run(cx: &mut UiCx<'_>, scope: SearchScope, query: SearchQuery) {
    cx.state.chat.search = Some(SearchView { scope, query: query.clone(), outcome: None });
    cx.cmd.send(Command::Chat(ChatCommand::Search { scope, query }));
}

fn text_id() -> egui::Id {
    egui::Id::unique("rc_search_text")
}

/// The header's search box (`width` px); `focus` = Ctrl+F was pressed. Enter searches, × clears
/// the text and closes the results.
pub(super) fn search_box(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, width: f32, focus: bool) {
    let mut text: String = ui.data(|d| d.get_temp(text_id())).unwrap_or_default();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
    let edit_id = text_id().with("edit");
    let focused = ui.memory(|m| m.has_focus(edit_id));
    let stroke = Stroke::new(1.0, if focused { pal.accent.gamma_multiply(0.6) } else { pal.line() });
    ui.painter().rect(rect, pal.radius, pal.surface, stroke, egui::StrokeKind::Inside);
    let edit = TextEdit::singleline(&mut text)
        .id(edit_id)
        .hint_text(RichText::new("Search").color(pal.muted))
        .font(FontId::proportional(pal.meta()))
        .frame(Frame::NONE)
        .align(Align2::LEFT_CENTER);
    let resp = ui.put(Rect::from_min_max(rect.min + vec2(8.0, 0.0), rect.max - vec2(26.0, 0.0)), edit);
    if focus {
        resp.request_focus();
    }
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let icon = Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), Vec2::splat(22.0));
    if !text.is_empty() || is_open(cx.state) {
        // A child ui: the row's cursor must stay left of the whole box.
        let mut child = ui.new_child(UiBuilder::new().max_rect(icon));
        if icon_button(&mut child, pal, "×", "Clear search", 22.0, false, pal.text).clicked() {
            text.clear();
            cx.state.chat.search = None;
        }
    } else {
        ui.painter().text(icon.center(), Align2::CENTER_CENTER, "🔍", FontId::new(13.0, crate::theme::icon_family()), pal.muted);
    }
    let content = text.trim();
    if enter && !content.is_empty() && let Some(scope) = scope(cx.state) {
        let sort = cx.state.chat.search.as_ref().map_or(SearchSort::Newest, |v| v.query.sort);
        let content = Some(content.chars().take(1024).collect());
        run(cx, scope, SearchQuery { content, sort, ..Default::default() });
    }
    ui.data_mut(|d| d.insert_temp(text_id(), text));
}

/// The results as a right panel replacing the member list (wide windows only; narrow ones show
/// `results` in place of the chat).
pub fn side_panel(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    if narrow(ui.ctx()) {
        return;
    }
    let c = cx.theme.colors.clone();
    let r = egui::Panel::right("rc_search_results")
        .exact_size(PANEL_W)
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().fill(c.sidebar.to_color32()))
        .show(ui, |ui| results(ui, cx))
        .response
        .rect;
    ui.painter().vline(r.left() + 0.5, r.y_range(), super::sidebar::line(&c));
}

/// Header (count, sort, close) and the hits, or the loading / indexing / error state.
pub fn results(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let pal = Pal::new(cx.theme);
    let Some(v) = cx.state.chat.search.as_ref() else { return };
    let (scope, query) = (v.scope, v.query.clone());
    let page: Option<SearchPage> = match &v.outcome {
        Some(SearchOutcome::Page(p)) => Some(p.clone()),
        _ => None,
    };
    let indexing = matches!(v.outcome, Some(SearchOutcome::Indexing { .. }));
    let state = match &v.outcome {
        None => None,
        Some(SearchOutcome::Page(_)) => Some(Ok(())),
        Some(SearchOutcome::Indexing { .. }) => Some(Err(String::new())),
        Some(SearchOutcome::Failed(e)) => Some(Err(e.clone())),
    };
    // The side panel's fill also in place of the chat (narrow windows): the cards stay raised.
    ui.painter().rect_filled(ui.max_rect(), 0, cx.theme.colors.sidebar.to_color32());
    ui.spacing_mut().item_spacing.y = 0.0;
    let title = match (&v.outcome, page.as_ref().map(|p| p.total_results)) {
        (_, Some(1)) => "1 Result".to_owned(),
        (_, Some(n)) => format!("{n} Results"),
        (Some(SearchOutcome::Indexing { .. }), _) => "Indexing".to_owned(),
        (Some(SearchOutcome::Failed(_)), _) => "Search failed".to_owned(),
        _ => "Searching…".to_owned(),
    };
    header(ui, cx, &pal, scope, &query, title);
    match (state, page) {
        (None, _) => centered(ui, |ui| {
            ui.add(egui::Spinner::new().size(24.0).color(pal.text));
        }),
        (Some(Err(e)), _) => centered(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            // Indexing waits for a manual retry: a still clock, not a spinner (that shows only
            // while the retry is in flight, as `None`).
            if indexing {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
                super::pickers::paint_clock(ui.painter(), r.center(), 12.0, pal.muted);
            }
            let text = if indexing { "Still indexing, try again soon" } else { e.as_str() };
            ui.add(Label::new(RichText::new(text).color(pal.muted)).selectable(false).wrap());
            if small_button(ui, &pal, "Try again", pal.text).clicked() {
                run(cx, scope, query.clone());
            }
        }),
        (Some(Ok(())), Some(page)) if page.messages.is_empty() => centered(ui, |ui| {
            ui.add(Label::new(RichText::new("No results").color(pal.muted)).selectable(false));
        }),
        (Some(Ok(())), Some(page)) => hits(ui, cx, &pal, scope, &query, &page),
        _ => {}
    }
}

/// A state message a third down the panel, centred.
fn centered(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(ui.available_height() / 3.0);
    Frame::new().inner_margin(Margin::symmetric(24, 0)).show(ui, |ui| ui.vertical_centered(add));
}

fn header(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, scope: SearchScope, query: &SearchQuery, title: String) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, pal.line()));
    let mut row = ui.new_child(UiBuilder::new().max_rect(rect.shrink2(vec2(16.0, 0.0)).with_max_x(rect.right() - 8.0)).layout(Layout::left_to_right(Align::Center)));
    row.add(Label::new(RichText::new(title).family(bold_family()).color(pal.text)).selectable(false));
    row.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if icon_button(ui, pal, "×", "Close", 28.0, false, pal.text).clicked() {
            cx.state.chat.search = None;
        }
        let label = match query.sort {
            SearchSort::Newest => "Newest",
            SearchSort::Oldest => "Oldest",
            SearchSort::Relevance => "Most relevant",
        };
        let resp = small_button(ui, pal, label, pal.text).on_hover_text("Sort");
        let mut picked = None;
        egui::Popup::menu(&resp).align(egui::RectAlign::BOTTOM_END).show(|ui| {
            menu_setup(ui);
            for (sort, name) in [(SearchSort::Newest, "Newest"), (SearchSort::Oldest, "Oldest"), (SearchSort::Relevance, "Most relevant")] {
                if menu_item(ui, name, MenuIcon::Check(sort == query.sort), pal.text) {
                    picked = Some(sort);
                }
            }
        });
        if let Some(sort) = picked.filter(|s| *s != query.sort) {
            run(cx, scope, SearchQuery { sort, offset: 0, ..query.clone() });
        }
    });
}

fn hits(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, scope: SearchScope, query: &SearchQuery, page: &SearchPage) {
    let guild_id = match scope {
        SearchScope::Guild(g) => Some(g),
        SearchScope::Dm(_) => None,
    };
    let mut jump = None;
    let terms: Vec<String> = query.content.as_deref().unwrap_or_default().split_whitespace().map(str::to_lowercase).collect();
    // A scroll position per page: a new page starts at its top.
    ScrollArea::vertical().id_salt(("rc_search_hits", query.offset, query.sort as u8)).auto_shrink(false).show(ui, |ui| {
        Frame::new().inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            let mut last = None;
            for m in &page.messages {
                if guild_id.is_some() && last != Some(m.channel_id) {
                    last = Some(m.channel_id);
                    ui.add_space(4.0);
                    let name = channel_label(cx.state, page, m.channel_id);
                    ui.add(Label::new(RichText::new(name).size(pal.meta()).family(bold_family()).color(pal.muted)).selectable(false).truncate());
                }
                let card = message_card(ui, cx, pal, guild_id, m, &terms);
                if card.clicked() {
                    jump = Some(m);
                }
                if ui.rect_contains_pointer(card.rect) {
                    let bar = Rect::from_min_max(card.rect.left_top() + vec2(0.0, 8.0), card.rect.right_top() + vec2(-8.0, 32.0));
                    let mut bar = ui.new_child(UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Min)));
                    if small_button(&mut bar, pal, "Jump", pal.text).clicked() {
                        jump = Some(m);
                    }
                }
            }
            pager(ui, cx, pal, scope, query, page.total_results);
        });
    });
    if let Some(m) = jump {
        let narrow = narrow(ui.ctx());
        jump_to(ui.ctx(), cx, m.channel_id, m.id);
        // In narrow windows the results cover the chat the jump lands in.
        if narrow {
            cx.state.chat.search = None;
        }
    }
}

/// "#channel" (or the thread's name) over a group of hits.
fn channel_label(state: &AppState, page: &SearchPage, id: Id) -> String {
    match state.channels.get(&id) {
        Some(c) => format!("{}{}", channel_prefix(c.kind), state.channel_name(id)),
        None => page.threads.iter().find(|t| t.id == id).and_then(|t| t.name.clone()).unwrap_or_else(|| "unknown".to_owned()),
    }
}

/// ‹ page / pages › under the hits.
fn pager(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, scope: SearchScope, query: &SearchQuery, total: u32) {
    let pages = total.div_ceil(PAGE).min(MAX_OFFSET / PAGE + 1);
    if pages <= 1 {
        return;
    }
    let current = query.offset / PAGE + 1;
    let mut to = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let label = format!("{current} / {pages}");
        let w = 28.0 * 2.0 + 16.0 + ui.painter().layout_no_wrap(label.clone(), FontId::proportional(pal.meta()), pal.muted).size().x;
        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
        ui.spacing_mut().item_spacing.x = 8.0;
        if ui.add_enabled_ui(current > 1, |ui| icon_button(ui, pal, "‹", "Previous page", 28.0, false, pal.text)).inner.clicked() {
            to = Some(current - 2);
        }
        ui.add(Label::new(RichText::new(label).size(pal.meta()).color(pal.muted)).selectable(false));
        if ui.add_enabled_ui(current < pages, |ui| icon_button(ui, pal, "›", "Next page", 28.0, false, pal.text)).inner.clicked() {
            to = Some(current);
        }
    });
    if let Some(p) = to {
        run(cx, scope, SearchQuery { offset: p * PAGE, ..query.clone() });
    }
}

/// `RUSTCORD_MSG_DEMO=search` / `indexing`: results for "rust" from the cached messages, or the
/// indexing state (no request).
pub(super) fn demo(ctx: &egui::Context, cx: &mut UiCx<'_>, indexing: bool) {
    let (Some(scope), Some(ch)) = (scope(cx.state), cx.state.selected_channel) else { return };
    let q = "rust";
    ctx.data_mut(|d| d.insert_temp(text_id(), q.to_owned()));
    let messages: Vec<Message> = cx.state.messages.get(&ch).map(|c| c.msgs.iter().rev().filter(|m| m.content.to_lowercase().contains(q)).cloned().collect()).unwrap_or_default();
    let outcome = if indexing {
        SearchOutcome::Indexing { retry_after_secs: 2.0 }
    } else {
        SearchOutcome::Page(SearchPage { total_results: 57, messages, ..Default::default() })
    };
    let query = SearchQuery { content: Some(q.into()), ..Default::default() };
    cx.state.chat.search = Some(SearchView { scope, query, outcome: Some(outcome) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_follows_the_open_channel() {
        let snapshot = serde_json::from_str(include_str!("../../tests/fixtures/home/cache/snapshot.json")).unwrap();
        let mut state = AppState::from_snapshot(snapshot);
        let g = state.selected_guild.unwrap();
        assert_eq!(scope(&state), Some(SearchScope::Guild(g)));
        let dm = state.dm_order[0];
        state.select_channel(dm);
        assert_eq!(scope(&state), Some(SearchScope::Dm(dm)));
    }
}
