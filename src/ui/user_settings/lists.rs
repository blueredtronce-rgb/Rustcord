//! Settings answered by REST lists (docs/SETTINGS_NOTES.md §1): Devices (`GET /auth/sessions`;
//! log out one device or all others, each after a confirmation; this login is never offered),
//! Authorized Apps (`GET /oauth2/tokens`; Deauthorize after a confirmation) and Data & Privacy's
//! two consents (`GET/POST /users/@me/consent`). A page sends its one fetch when it is first
//! shown in an opening of the layer; offline mode answers "Not connected" (sample lists with
//! `RUSTCORD_NAV_DEMO=settings`, sending nothing more).
//! OWNERSHIP: user-settings engineer.

use super::super::messages::{Pal, small_button};
use super::super::{UiCx, bold, hint, section};
use super::{Page, switch_row};
use crate::backend::{AccountCommand, Command};
use crate::state::account::AccountState;
use egui::{Align, Layout, RichText};

/// The page's fetch, once per opening of the layer (`UserSettingsUi::fetched`).
pub(super) fn fetch_once(cx: &mut UiCx<'_>, page: Page, cmd: AccountCommand, clear: impl FnOnce(&mut AccountState)) {
    let fetched = &mut cx.view.user_settings_ui.fetched;
    if fetched.contains(&page) {
        return;
    }
    fetched.push(page);
    clear(&mut cx.state.account);
    cx.cmd.send(Command::Account(cmd));
}

/// Test hook `RUSTCORD_NAV_DEMO=settings`: sample devices, apps and consents in place of the
/// offline "Not connected" answers.
pub(super) fn demo(a: &mut AccountState) {
    use crate::model::{AuthSession, AuthorizedApp, Consent, Consents, Id, OAuthApplication, SessionClientInfo};
    if !super::super::sidebar::nav_demo("settings") {
        return;
    }
    let session = |hash: &str, os: &str, platform: &str, location: &str, used: &str| AuthSession {
        id_hash: hash.into(),
        approx_last_used_time: Some(used.into()),
        client_info: SessionClientInfo { os: Some(os.into()), platform: Some(platform.into()), location: Some(location.into()) },
    };
    a.session_hash = "me".into();
    if !a.sessions.as_ref().is_some_and(Result::is_ok) {
        a.sessions = Some(Ok(vec![
        session("me", "Windows", "Rustcord", "Berlin, Germany", "2026-11-13T12:00:00Z"),
        session("p", "Android", "Discord Android", "Berlin, Germany", "2026-11-12T08:30:00Z"),
        session("w", "Linux", "Firefox", "Hamburg, Germany", "2026-10-02T19:10:00Z"),
        ]));
    }
    let app = |id: u64, name: &str, description: &str, scopes: &[&str]| AuthorizedApp {
        id: Id(id),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        application: OAuthApplication { id: Id(id), name: name.into(), icon: None, description: description.into() },
    };
    if !a.authorized_apps.as_ref().is_some_and(Result::is_ok) {
        a.authorized_apps = Some(Ok(vec![
            app(1, "Ferris Stats", "Server statistics and leaderboards.", &["identify", "guilds"]),
            app(2, "Crab Music", "", &["identify"]),
        ]));
    }
    if !a.consents.as_ref().is_some_and(Result::is_ok) {
        a.consents = Some(Ok(Consents { usage_statistics: Consent { consented: true }, personalization: Consent { consented: false } }));
    }
    if !a.standing.as_ref().is_some_and(Result::is_ok) {
        a.standing = Some(Ok(100));
    }
}

/// Spinner while `None`, the error text for `Err`; the list for `Ok`.
fn loaded<'a, T>(ui: &mut egui::Ui, pal: &Pal, v: Option<&'a Result<T, String>>) -> Option<&'a T> {
    match v {
        None => {
            ui.add(egui::Spinner::new().size(18.0).color(pal.muted));
            None
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap().selectable(true));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

/// "2026-11-13" from an ISO time (empty when unparsable).
fn day(iso: Option<&str>) -> String {
    iso.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// One card row: title, muted detail line, and an optional button on the right (true when clicked).
fn card(ui: &mut egui::Ui, pal: &Pal, title: &str, detail: &str, button: Option<&str>) -> bool {
    let mut clicked = false;
    egui::Frame::new().fill(pal.surface).corner_radius(pal.card_radius()).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - 120.0);
                ui.add(egui::Label::new(RichText::new(title).font(bold(pal.size)).color(pal.text)).truncate());
                if !detail.is_empty() {
                    ui.add(egui::Label::new(RichText::new(detail).size(pal.meta()).color(pal.muted)).wrap());
                }
            });
            if let Some(b) = button {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| clicked = small_button(ui, pal, b, pal.danger).clicked());
            }
        });
    });
    ui.add_space(6.0);
    clicked
}

/// Settings → Devices.
pub(super) fn devices(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    fetch_once(cx, Page::Devices, AccountCommand::FetchSessions, |a| a.sessions = None);
    demo(&mut cx.state.account);
    let pal = Pal::new(cx.theme);
    hint(ui, cx.theme, "Devices logged in to your account. Log out of any you don't recognise.");
    let me = cx.state.account.session_hash.clone();
    let Some(list) = loaded(ui, &pal, cx.state.account.sessions.as_ref()) else { return };
    let line = |s: &crate::model::AuthSession| {
        let c = &s.client_info;
        let title = [c.os.as_deref(), c.platform.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let when = day(s.approx_last_used_time.as_deref());
        let detail = [c.location.as_deref(), Some(when.as_str())].into_iter().flatten().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" · ");
        (if title.is_empty() { "Unknown device".to_owned() } else { title }, detail, s.id_hash.clone())
    };
    let (current, others): (Vec<_>, Vec<_>) = list.iter().map(line).partition(|(_, _, h)| !me.is_empty() && *h == me);
    let mut ask = None;
    section(ui, cx.theme, "Current device");
    match current.first() {
        Some((title, detail, _)) => _ = card(ui, &pal, title, detail, None),
        None => hint(ui, cx.theme, "This app"),
    }
    section(ui, cx.theme, "Other devices");
    if others.is_empty() {
        hint(ui, cx.theme, "No other devices.");
    }
    // This login is unknown (no session hash in READY): any card could be it, so none offers Log Out.
    let known = !me.is_empty();
    if !known && !others.is_empty() {
        hint(ui, cx.theme, "This device could not be told apart, so logging out here is off.");
    }
    for (title, detail, hash) in &others {
        if card(ui, &pal, title, detail, known.then_some("Log Out")) {
            ask = Some((format!("Log out of {title}?"), vec![hash.clone()]));
        }
    }
    if known && others.len() > 1 {
        ui.add_space(8.0);
        if small_button(ui, &pal, "Log Out of All Other Devices", pal.danger).clicked() {
            // The endpoint takes at most 64 hashes per call.
            ask = Some(("Log out of all other devices?".into(), others.iter().take(64).map(|o| o.2.clone()).collect()));
        }
    }
    if let Some((title, id_hashes)) = ask {
        let body = "Those sessions end at once; logging in again there needs your password.";
        cx.view.confirm(title, body, "Log Out", Command::Account(AccountCommand::LogoutSessions { id_hashes }));
    }
}

/// Settings → Authorized Apps.
pub(super) fn authorized_apps(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    fetch_once(cx, Page::AuthorizedApps, AccountCommand::FetchAuthorizedApps, |a| a.authorized_apps = None);
    demo(&mut cx.state.account);
    let pal = Pal::new(cx.theme);
    hint(ui, cx.theme, "Apps you allowed to use your account.");
    let Some(list) = loaded(ui, &pal, cx.state.account.authorized_apps.as_ref()) else { return };
    if list.is_empty() {
        hint(ui, cx.theme, "No authorized apps.");
    }
    let mut ask = None;
    for app in list {
        let mut detail = app.application.description.clone();
        if !app.scopes.is_empty() {
            let scopes = format!("Permissions: {}", app.scopes.join(", "));
            detail = if detail.is_empty() { scopes } else { format!("{detail}\n{scopes}") };
        }
        if card(ui, &pal, &app.application.name, &detail, Some("Deauthorize")) {
            ask = Some((app.application.name.clone(), app.id));
        }
    }
    if let Some((name, token_id)) = ask {
        let body = format!("{name} loses access to your account until you authorize it again.");
        cx.view.confirm(format!("Deauthorize {name}?"), body, "Deauthorize", Command::Account(AccountCommand::RevokeAuthorizedApp { token_id }));
    }
}

/// Data & Privacy's consents ("how we use your data").
pub(super) fn consents(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    fetch_once(cx, Page::Privacy, AccountCommand::FetchConsents, |a| a.consents = None);
    demo(&mut cx.state.account);
    let pal = Pal::new(cx.theme);
    section(ui, cx.theme, "How we use your data");
    let Some(c) = loaded(ui, &pal, cx.state.account.consents.as_ref()) else { return };
    let (mut usage, mut personal) = (c.usage_statistics.consented, c.personalization.consented);
    let theme = &*cx.theme;
    let change = if switch_row(ui, theme, "Use data to improve Discord", None, &mut usage) {
        Some(("usage_statistics", usage))
    } else if switch_row(ui, theme, "Use data to personalize my experience", None, &mut personal) {
        Some(("personalization", personal))
    } else {
        None
    };
    if let Some((name, on)) = change {
        let (grant, revoke) = if on { (vec![name.to_owned()], vec![]) } else { (vec![], vec![name.to_owned()]) };
        // The answer is the refetched pair; a spinner until then.
        cx.state.account.consents = None;
        cx.cmd.send(Command::Account(AccountCommand::SetConsents { grant, revoke }));
    }
}
