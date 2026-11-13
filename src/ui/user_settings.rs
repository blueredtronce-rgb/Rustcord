//! User settings (FEATURES_NOTES §3.1, §3.2, layout §4.9; every page of
//! docs/SETTINGS_NOTES.md in Discord's order): a full-screen layer while
//! `ViewState::settings_open`, a searchable category list on the left
//! (`ViewState::settings_page`; the search matches page titles, option labels and keywords)
//! and one page on the right; Log Out at the bottom. Esc or the × closes it.
//! - User Settings: My Account, Profiles, Profile Cosmetics, Content & Social, Data & Privacy,
//!   Family Center, Authorized Apps, Devices, Connections, Friend Requests.
//! - Billing Settings: one page linking to discord.com. App Settings: Appearance, Accessibility,
//!   Voice & Video, Clips, Chat, Notifications, Keybinds, Language, Windows Settings, Streamer
//!   Mode, Advanced. Activity Settings: Activity Privacy, Registered Games, Game Overlay.
//!   Rustcord: Plugins, Local Profile, Theme Editor.
//! - What Rustcord can't do (billing, game detection, overlay, clip recording, Family Center
//!   linking) is a Discord-like empty state (`empty_state`) above the page's synced rows.
//!
//! Where options live: Discord's settings proto for synced ones (synced.rs, one table per page),
//! REST lists (lists.rs: devices, authorized apps, consents), `config::Settings` for this
//! device (app.rs, keybinds.rs, voice.rs). Also the Set Custom Status modal
//! (`Modal::CustomStatus`) and the lower part of the account popout (`account_popout`).
//!
//! Every account write is an `AccountCommand` sent on a click, with its progress read from
//! `AccountState::ops` (`begin`); errors (captcha, 2FA, "too fast") are shown as they arrive.
//! OWNERSHIP: user-settings engineer (docs/SOCIAL_UI_DESIGN.md); the layer chrome
//! (`settings_layer`, `nav_item`, `section`, `hint`, `dialog`) is shared in mod.rs.

mod account;
mod app;
mod connections;
mod cosmetics;
mod keybinds;
mod lists;
pub mod local_profile;
mod plugins;
mod status;
mod synced;
pub mod theme_editor;
mod voice;

pub(super) use app::frame;
pub(super) use keybinds::{Bind, shortcut};
pub(super) use status::account_popout;
pub use status::custom_status_modal;
pub(super) use voice::restore as restore_voice;

use super::messages::Pal;
use super::{UiCx, ViewState, hint, nav_heading, nav_item, page_title, section, settings_layer};
use crate::backend::{AccountCommand, AccountOp, Command, NotificationPatch, OpKind};
use crate::state::account::OpState;
use crate::theme::Theme;
use egui::{Align, Layout, RichText, Sense, vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    MyAccount,
    Profiles,
    Cosmetics,
    ContentSocial,
    Privacy,
    FamilyCenter,
    AuthorizedApps,
    Devices,
    Connections,
    FriendRequests,
    Billing,
    Appearance,
    Accessibility,
    VoiceVideo,
    /// Only the synced "record my voice" switch: Rustcord does not capture clips.
    Clips,
    Chat,
    Notifications,
    Keybinds,
    Language,
    Windows,
    StreamerMode,
    Advanced,
    ActivityPrivacy,
    RegisteredGames,
    GameOverlay,
    /// Rustcord: native Vencord-style plugins (plugins.rs).
    Plugins,
    LocalProfile,
    /// Rustcord: the live theme editor (theme_editor.rs).
    ThemeEditor,
}

impl Page {
    /// Nav groups, in Discord's order.
    const GROUPS: [(&str, &[Page]); 5] = [
        (
            "User Settings",
            &[
                Page::MyAccount,
                Page::Profiles,
                Page::Cosmetics,
                Page::ContentSocial,
                Page::Privacy,
                Page::FamilyCenter,
                Page::AuthorizedApps,
                Page::Devices,
                Page::Connections,
                Page::FriendRequests,
            ],
        ),
        ("Billing Settings", &[Page::Billing]),
        (
            "App Settings",
            &[
                Page::Appearance,
                Page::Accessibility,
                Page::VoiceVideo,
                Page::Clips,
                Page::Chat,
                Page::Notifications,
                Page::Keybinds,
                Page::Language,
                Page::Windows,
                Page::StreamerMode,
                Page::Advanced,
            ],
        ),
        ("Activity Settings", &[Page::ActivityPrivacy, Page::RegisteredGames, Page::GameOverlay]),
        ("Rustcord", &[Page::Plugins, Page::LocalProfile, Page::ThemeEditor]),
    ];

    /// Title, `RUSTCORD_OPEN=settings:<name>` name, extra search words (lowercase).
    fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Page::MyAccount => (
                "My Account",
                "my-account",
                "display name username email phone password two-factor 2fa authenticator sms backup codes security keys account standing disable delete",
            ),
            Page::Profiles => ("Profiles", "profiles", "pronouns about me bio avatar banner colour color"),
            Page::Cosmetics => ("Profile Cosmetics", "cosmetics", "avatar decoration profile effect nameplate frame"),
            Page::ContentSocial => ("Content & Social", "content", "messaging permissions dms spam sensitive explicit filter"),
            Page::Privacy => ("Data & Privacy", "privacy", "data consent personalize statistics read receipts"),
            Page::FamilyCenter => ("Family Center", "family", "teen parent"),
            Page::AuthorizedApps => ("Authorized Apps", "apps", "oauth applications deauthorize bots"),
            Page::Devices => ("Devices", "devices", "sessions log out logged in"),
            Page::Connections => ("Connections", "connections", "accounts spotify steam github twitch xbox"),
            Page::FriendRequests => ("Friend Requests", "friend-requests", "friends"),
            Page::Billing => ("Nitro & Billing", "billing", "nitro boost subscriptions gift payment"),
            Page::Appearance => ("Appearance", "appearance", "theme dark light gradient font member list colours colors density"),
            Page::Accessibility => {
                ("Accessibility", "accessibility", "saturation role colours colors font size scaling zoom spacing reduced motion animation gif tts")
            }
            Page::VoiceVideo => ("Voice & Video", "voice", "microphone speaker camera push to talk input output"),
            Page::Clips => ("Clips", "clips", "clips recording record voice"),
            Page::Chat => ("Chat", "chat", "text images embeds links media emoji reactions spoilers"),
            Page::Notifications => ("Notifications", "notifications", "desktop taskbar push mute servers"),
            Page::Keybinds => ("Keybinds", "keybinds", "shortcuts hotkeys keyboard"),
            Page::Language => ("Language", "language", "locale"),
            Page::Windows => ("Windows Settings", "windows", "startup minimize tray close"),
            Page::StreamerMode => ("Streamer Mode", "streamer", "streaming obs hide"),
            Page::Advanced => ("Advanced", "advanced", "developer mode copy id hardware acceleration"),
            Page::ActivityPrivacy => ("Activity Privacy", "activity", "activity status game join"),
            Page::RegisteredGames => ("Registered Games", "games", "games detection"),
            Page::GameOverlay => ("Game Overlay", "overlay", "overlay"),
            Page::Plugins => ("Plugins", "plugins", "vencord"),
            Page::LocalProfile => ("Local Profile", "local-profile", "local banner avatar this device"),
            Page::ThemeEditor => ("Theme Editor", "theme-editor", "customize theme colours colors import export"),
        }
    }

    /// The labels of the page's own (this device) rows, for the search.
    /// shortcut: kept by hand next to the pages' code; move the labels into shared consts if they drift.
    fn local_labels(self) -> &'static [&'static str] {
        match self {
            Page::Appearance => &["Theme", "Gradients", "Message Display", "Cozy", "Compact", "Use Discord font (gg sans)", "Show member list"],
            Page::Accessibility => &[
                "Saturation",
                "Show role colours in names",
                "Chat font scaling",
                "Zoom level",
                "Space between message groups",
                "Reduced motion",
                "Play animated avatars, emoji and GIFs",
            ],
            Page::Windows => &["Open Rustcord when I sign in", "Start minimized", "Close button minimizes"],
            Page::StreamerMode => &["Enable Streamer Mode", "Hide personal information", "Hide invite links", "Disable notifications", "Hide window from screen capture"],
            Page::Privacy => &["Send read receipts"],
            Page::Notifications => &["Desktop notifications", "Flash the taskbar"],
            Page::VoiceVideo => &["Input Mode", "Voice Activity", "Push to Talk", "Input Sensitivity", "Microphone & Sound", "Mute", "Deafen", "Video", "Camera"],
            Page::Advanced => &["Hardware acceleration"],
            Page::ThemeEditor => &[
                "Base", "Dark", "Light", "Gradient", "Background", "Sidebar", "Server bar", "Surface", "Text", "Muted text", "Accent", "Text on accent", "Message hover",
                "Mention highlight", "Links", "Danger", "Code background", "UI font", "Monospace font", "Font size", "Corner radius", "Gradient angle", "Panel opacity",
            ],
            _ => &[],
        }
    }

    pub fn title(self) -> &'static str {
        self.info().0
    }

    /// `RUSTCORD_OPEN=settings:<name>`: my-account, profiles, cosmetics, content, privacy,
    /// family, apps, devices, connections, friend-requests, billing, appearance, accessibility,
    /// voice, clips, chat, notifications, keybinds, language, windows, streamer, advanced, activity,
    /// games, overlay, plugins, local-profile, theme-editor.
    pub fn from_name(name: &str) -> Option<Page> {
        Self::GROUPS.iter().flat_map(|g| g.1.iter().copied()).find(|p| p.info().1 == name)
    }

    /// The sidebar search: `q` (lowercase, trimmed) in the title, the keywords, a local or synced
    /// option's label (Keybinds: the actions).
    fn matches(self, q: &str) -> bool {
        let (title, _, words) = self.info();
        let has = |s: &str| s.to_lowercase().contains(q);
        let binds = self == Page::Keybinds && Bind::ALL.iter().any(|b| has(b.label()));
        q.is_empty()
            || has(title)
            || words.contains(q)
            || binds
            || self.local_labels().iter().any(|l| has(l))
            || synced::groups(self).iter().any(|g| has(g.title) || g.rows.iter().any(|r| has(r.label)))
    }
}

/// Drafts of the user settings forms (in `ViewState::user_settings_ui`). Per-opening parts are
/// reset when the layer was not drawn in the previous pass; the password buffer is wiped then.
#[derive(Default)]
pub struct UserSettingsUi {
    /// Pass the layer was last drawn in.
    pass: u64,
    status: status::StatusForm,
    account: account::AccountForm,
    /// Profiles page; None until it is first shown (that sends the one `FetchProfile`).
    profile: Option<account::ProfileForm>,
    voice: voice::VoiceForm,
    /// Profile Cosmetics tab / search; lives as long as the login (like the catalog in state).
    cosmetics: cosmetics::Form,
    /// The sidebar's search text (per opening).
    search: String,
    /// The page open when the search moved away from it (it matched nothing): shown again once
    /// the search is cleared, unless a result was clicked meanwhile.
    before_search: Option<Page>,
    /// Pages whose one fetch went out in this opening (lists.rs).
    fetched: Vec<Page>,
    /// Profiles > Server Profiles: the server and its nickname draft (None until first shown);
    /// None = the User Profile tab. Cleared when the layer closes.
    server_tab: Option<(crate::model::Id, Option<String>)>,
    /// Log Out was clicked: the "Are you sure?" dialog is open.
    ask_logout: bool,
}

/// Server menu > Edit Per-server Profile: the settings layer on Profiles, that server's tab.
pub fn open_server_profile(view: &mut ViewState, guild_id: crate::model::Id) {
    view.profile_card = None;
    (view.settings_open, view.settings_page) = (true, Page::Profiles);
    view.user_settings_ui.server_tab = Some((guild_id, None));
}

/// The layer (main_layout calls it while `settings_open`).
pub fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    let pass = ui.ctx().cumulative_pass_nr();
    if cx.view.user_settings_ui.pass + 1 != pass {
        opened(cx);
    }
    cx.view.user_settings_ui.pass = pass;
    if let Some(q) = ui.ctx().data_mut(|d| d.remove_temp::<String>(search_demo_id())) {
        cx.view.user_settings_ui.search = q;
    }
    // Reading the themes folder is disk I/O: once per opening of the layer.
    if cx.view.settings_themes.is_none() {
        cx.view.settings_themes = Some(Theme::load_all());
    }
    let (mut changed, mut log_out) = (false, false);
    let before = cx.view.settings_page;
    // Esc while the Log Out question is open closes only the question.
    let asking = cx.view.user_settings_ui.ask_logout;
    // The 2FA re-check of a waiting action, on any page; before the layer so its Esc wins.
    account::security::mfa_dialog(ui, cx);
    let close = settings_layer(
        ui,
        cx,
        "user_settings",
        |ui, cx| log_out = nav(ui, cx),
        |ui, cx| {
            page_title(ui, cx.theme, cx.view.settings_page.title());
            changed = page(ui, cx);
        },
    ) && !asking;
    cx.view.user_settings_ui.ask_logout |= log_out;
    if cx.view.user_settings_ui.ask_logout {
        let theme = &*cx.theme;
        let resp = super::dialog(ui.ctx(), theme, "rc_logout", 440.0, "Log Out", |ui| {
            super::dialog_text(ui, theme, "Are you sure you want to log out?");
            super::dialog_buttons(ui, theme, "Log Out", true, true)
        });
        match resp.inner {
            Some(true) => {
                logout(cx);
                return;
            }
            Some(false) => cx.view.user_settings_ui.ask_logout = false,
            None if resp.should_close() => cx.view.user_settings_ui.ask_logout = false,
            None => {}
        }
    }
    let left = close || cx.view.settings_page != before;
    if left && before == Page::Cosmetics {
        cosmetics::leave(ui.ctx(), cx);
    }
    if left && before == Page::Keybinds {
        keybinds::stop_recording(ui.ctx());
    }
    if close {
        cx.view.settings_open = false;
        opened(cx);
        cx.view.user_settings_ui.server_tab = None;
    }
    if changed
        && let Err(e) = cx.settings.save()
    {
        cx.view.toast(format!("Could not save settings: {e}"), true);
    }
}

/// The nav column: search box, the groups (only matching pages while searching), Log Out.
/// True when Log Out was clicked (the caller asks before logging out).
fn nav(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    let search = egui::TextEdit::singleline(&mut cx.view.user_settings_ui.search).hint_text("Search").desired_width(f32::INFINITY);
    ui.add(search);
    ui.add_space(6.0);
    let q = cx.view.user_settings_ui.search.trim().to_lowercase();
    cx.view.settings_page = search_page(cx.view.settings_page, &mut cx.view.user_settings_ui.before_search, &q);
    let mut any = false;
    for (heading, pages) in Page::GROUPS {
        let mut shown = pages.iter().copied().filter(|p| p.matches(&q)).peekable();
        if shown.peek().is_none() {
            continue;
        }
        any = true;
        nav_heading(ui, cx.theme, heading);
        for p in shown {
            if nav_item(ui, cx.theme, p.title(), cx.view.settings_page == p, false) {
                cx.view.settings_page = p;
                cx.view.user_settings_ui.before_search = None;
            }
        }
        ui.separator();
    }
    if !any {
        hint(ui, cx.theme, "No results");
        ui.separator();
    }
    nav_item(ui, cx.theme, "Log Out", false, true)
}

/// The page to show for the search `q` (lowercase, trimmed): the open one while it matches,
/// else the first match (the nav highlights it), remembering the open one in `before`; once
/// `q` is empty again, the remembered page.
fn search_page(open: Page, before: &mut Option<Page>, q: &str) -> Page {
    if q.is_empty() {
        return before.take().unwrap_or(open);
    }
    if open.matches(q) {
        return open;
    }
    let Some(first) = Page::GROUPS.iter().flat_map(|g| g.1.iter().copied()).find(|p| p.matches(q)) else { return open };
    before.get_or_insert(open);
    first
}

/// `RUSTCORD_OPEN=settings-search:<q>`: the sidebar search filled in (applied when the layer
/// opens, after its per-opening reset).
pub fn demo_search(ctx: &egui::Context, q: &str) {
    ctx.data_mut(|d| d.insert_temp(search_demo_id(), q.to_owned()));
}

fn search_demo_id() -> egui::Id {
    egui::Id::unique("rc_settings_search_demo")
}

/// The layer opened or closed: drop per-opening drafts (unsaved edits, the password, finished
/// progress entries) so nothing stale shows next time.
fn opened(cx: &mut UiCx<'_>) {
    let u = &mut cx.view.user_settings_ui;
    u.account = Default::default();
    u.voice.opened();
    u.search.clear();
    u.before_search = None;
    u.fetched.clear();
    if let Some(p) = &mut u.profile {
        // A save answered while the layer was closed is kept, not reset to the old base.
        p.apply_saved(&mut cx.state.account);
        p.discard();
    }
    for kind in [
        OpKind::UpdateGlobalName,
        OpKind::UpdateUsername,
        OpKind::UpdateAvatar,
        OpKind::UpdateBanner,
        OpKind::UpdateBio,
        OpKind::UpdatePronouns,
        OpKind::UpdateAccentColor,
    ] {
        finish_op(cx, kind);
    }
}

/// The selected page: its own controls, then its synced options (synced.rs). True when
/// `cx.settings` changed (the caller saves them).
fn page(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    let mut changed = false;
    let p = cx.view.settings_page;
    match p {
        Page::MyAccount => account::my_account(ui, cx),
        Page::Profiles => account::profiles(ui, cx),
        Page::Cosmetics => cosmetics::show(ui, cx),
        Page::Connections if cx.settings.streamer.hides_personal() => hint(ui, cx.theme, "Hidden by Streamer Mode."),
        Page::Connections => connections::show(ui, cx),
        Page::VoiceVideo => changed |= voice::show(ui, cx),
        Page::Privacy => {
            lists::consents(ui, cx);
            section(ui, cx.theme, "Read state");
            changed |= switch_row(
                ui,
                cx.theme,
                "Send read receipts",
                Some("Off: channels are marked read only in this app; your other devices keep them unread."),
                &mut cx.settings.send_read_acks,
            );
        }
        Page::Devices => lists::devices(ui, cx),
        Page::AuthorizedApps => lists::authorized_apps(ui, cx),
        Page::FamilyCenter => empty_state(ui, cx, "👪", "Set up in the mobile app", "Linking a teen's account happens in the Discord mobile app.", None),
        Page::Billing => empty_state(ui, cx, "💳", "Managed on discord.com", "Nitro, boosts, subscriptions, gifts and payments.", Some("premium")),
        Page::RegisteredGames => empty_state(ui, cx, "🎮", "No game detection", "Rustcord doesn't scan for running games.", None),
        Page::GameOverlay => empty_state(ui, cx, "🖥", "No in-game overlay", "An overlay needs code injected into games.", None),
        Page::Clips => empty_state(ui, cx, "🎬", "No clip recording", "Rustcord doesn't record clips. The switch below is about clips others record.", None),
        Page::LocalProfile => local_profile::show(ui, cx),
        Page::Plugins => plugins::show(ui, cx),
        Page::ThemeEditor => changed |= theme_editor::show(ui, cx),
        Page::Appearance => {
            changed |= appearance(ui, cx);
            section(ui, cx.theme, "Message Display");
            synced::message_display(ui, cx);
            section(ui, cx.theme, "Font");
            if switch_row(
                ui,
                cx.theme,
                "Use Discord font (gg sans)",
                Some("Downloaded from discord.com into the cache, never bundled. Off or offline: Segoe UI."),
                &mut cx.settings.use_discord_font,
            ) {
                crate::theme::set_discord_font(ui.ctx(), cx.settings.use_discord_font);
                changed = true;
            }
            section(ui, cx.theme, "Layout");
            changed |= switch_row(
                ui,
                cx.theme,
                "Show member list",
                Some("Next to server channels and group DMs; hidden in narrow windows."),
                &mut cx.settings.show_member_list,
            );
        }
        Page::Accessibility => changed |= app::accessibility(ui, cx),
        Page::Notifications => changed |= notification_switches(ui, cx),
        Page::Keybinds => changed |= keybinds::show(ui, cx),
        Page::Windows => changed |= app::windows(ui, cx),
        Page::StreamerMode => changed |= app::streamer(ui, cx),
        Page::Advanced => app::advanced(ui, cx),
        Page::ContentSocial | Page::FriendRequests | Page::Chat | Page::Language | Page::ActivityPrivacy => {}
    }
    // Profiles > Server Profiles is about one server only.
    if !(p == Page::Profiles && cx.view.user_settings_ui.server_tab.is_some()) {
        synced::show(ui, cx, synced::groups(p));
    }
    if p == Page::Notifications {
        guild_notifications(ui, cx);
    }
    changed
}

/// Discord's settings in the browser (a link the user clicks; Rustcord sends nothing).
/// shortcut: `section` follows the web client's `/settings/<section>` route; unverified per page.
fn open_in_browser(ui: &mut egui::Ui, section: &str) {
    let url = format!("https://discord.com/settings/{section}");
    let resp = ui.add(egui::Button::new("Open in browser  ↗").min_size(vec2(64.0, 32.0)));
    if resp.on_hover_text(&url).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// What Rustcord can't offer on a page (Discord's empty state): a large muted icon in a disc,
/// a title, one line why, and the browser link if any. Synced rows may follow under it.
fn empty_state(ui: &mut egui::Ui, cx: &UiCx<'_>, icon: &str, title: &str, text: &str, section: Option<&str>) {
    let pal = Pal::new(cx.theme);
    ui.add_space(32.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 48.0, pal.surface);
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::new(44.0, crate::theme::icon_family()), pal.muted);
        ui.add_space(16.0);
        ui.label(RichText::new(title).font(super::bold(pal.size + 4.0)).color(pal.text));
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(text).color(pal.muted)).wrap());
        if let Some(section) = section {
            ui.add_space(16.0);
            open_in_browser(ui, section);
        }
    });
    ui.add_space(16.0);
}

/// Theme picker: round swatches (tooltip = name) of the flat themes and of the gradient themes
/// (the Nitro colour themes; free, only on this device). Returns true when the settings changed.
fn appearance(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    // Taken for the frame (put back below): `folder_button` needs `cx`.
    let themes = cx.view.settings_themes.take().unwrap_or_default();
    let mut picked: Option<Theme> = None;
    section(ui, cx.theme, "Theme");
    ui.horizontal_wrapped(|ui| {
        picked = theme_editor::swatches(ui, &themes, cx.theme, false);
        folder_button(ui, cx, &crate::config::paths().themes_dir(), "Themes folder: add your own as .toml files");
        let pal = Pal::new(cx.theme);
        if super::messages::icon_button(ui, &pal, "✏", "Customize theme", 28.0, false, pal.text).clicked() {
            cx.view.settings_page = Page::ThemeEditor;
        }
    });
    section(ui, cx.theme, "Gradients");
    let gradient = ui.horizontal_wrapped(|ui| theme_editor::swatches(ui, &themes, cx.theme, true)).inner;
    picked = picked.or(gradient);
    hint(ui, cx.theme, "Free, only on this device.");
    cx.view.settings_themes = Some(themes);

    let Some(theme) = picked.filter(|t| *t != *cx.theme) else { return false };
    cx.settings.theme = theme.name.clone();
    *cx.theme = theme;
    cx.view.theme_dirty = true;
    ui.ctx().request_repaint();
    true
}

/// Notifications on this device: desktop notifications and taskbar flashing.
fn notification_switches(ui: &mut egui::Ui, cx: &mut UiCx<'_>) -> bool {
    let s = &mut *cx.settings;
    let desktop = switch_row(
        ui,
        cx.theme,
        "Desktop notifications",
        Some("Direct messages and mentions, following each server's settings. Silenced while you're on Do Not Disturb."),
        &mut s.desktop_notifications,
    );
    desktop | switch_row(ui, cx.theme, "Flash the taskbar", Some("When a notification arrives while Rustcord is in the background."), &mut s.flash_taskbar)
}

/// Each server's level and mute (`UpdateGuildNotifications`, one request per pick; a server's
/// controls wait while its change is on the way).
fn guild_notifications(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    if cx.state.guilds.is_empty() {
        return;
    }
    section(ui, cx.theme, "Servers");
    const LEVELS: [&str; 3] = ["All Messages", "Only @mentions", "Nothing"];
    let pal = Pal::new(cx.theme);
    let mut send = None;
    for g in &cx.state.guilds {
        let s = cx.state.guild_settings.get(&Some(g.id));
        // 3 = inherit: the server's default.
        let level = s.map_or(3, |s| s.message_notifications);
        let level = if level == 3 { g.default_message_notifications.min(1) } else { level.min(2) };
        let muted = cx.state.is_guild_muted(g.id);
        let op = AccountOp { kind: OpKind::UpdateGuildNotifications, target: Some(g.id) };
        let busy = cx.state.account.ops.get(&op) == Some(&OpState::Pending);
        ui.horizontal(|ui| {
            super::button_row(ui);
            let (icon, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
            // The server bar's icons (same URL and size: no new requests), only for rows in view.
            const PX: u32 = super::sidebar::ICON_PX;
            if ui.is_rect_visible(icon) {
                match crate::api::cdn::guild_icon_url(g, PX, false).and_then(|url| cx.images.get(ui.ctx(), &url, PX, false)) {
                    Some(tex) => egui::Image::from_texture(tex).corner_radius(10).paint_at(ui, icon),
                    None => super::images::paint_initial(ui.painter(), icon, 10.0, g.id.0, &g.name, pal.dark),
                }
            }
            let w = ui.available_width() - 210.0;
            ui.allocate_ui_with_layout(vec2(w.max(60.0), 32.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(&g.name).color(pal.text)).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if bell_button(ui, &pal, muted).clicked() {
                        send = Some((g.id, NotificationPatch { muted: Some(!muted), ..Default::default() }));
                    }
                    let mut pick = level;
                    egui::ComboBox::from_id_salt(("rc_guild_notify", g.id)).width(150.0).selected_text(LEVELS[level as usize]).show_ui(ui, |ui| {
                        for (i, l) in LEVELS.iter().enumerate() {
                            ui.selectable_value(&mut pick, i as u8, *l);
                        }
                    });
                    if pick != level {
                        send = Some((g.id, NotificationPatch { level: Some(pick), ..Default::default() }));
                    }
                    if busy {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                });
            });
        });
    }
    if let Some((guild_id, patch)) = send {
        let cmd = AccountCommand::UpdateGuildNotifications { guild_id: Some(guild_id), patch };
        if cx.state.account.begin(cmd.op()) {
            cx.cmd.send(Command::Account(cmd));
        }
    }
    // Answers: the gateway update shows a success; a failure becomes a toast.
    let mut failed = Vec::new();
    cx.state.account.ops.retain(|op, s| match s {
        OpState::Failed(e) if op.kind == OpKind::UpdateGuildNotifications => {
            failed.push(std::mem::take(e));
            false
        }
        _ => op.kind != OpKind::UpdateGuildNotifications || *s == OpState::Pending,
    });
    for e in failed {
        cx.view.toast(e, true);
    }
}

fn logout(cx: &mut UiCx<'_>) {
    cx.cmd.send(Command::Logout);
    cx.state.reset();
    cx.images.clear();
    // Fresh view state: login screen, no drafts or other leftovers of this account.
    *cx.view = ViewState::default();
    std::thread::spawn(crate::cache::clear_all);
}

// ---------------------------------------------------------------------------
// Small shared pieces of the pages
// ---------------------------------------------------------------------------

/// Progress of one of my own account writes (`target: None`).
fn my_op(kind: OpKind) -> AccountOp {
    AccountOp { kind, target: None }
}

fn op_state(cx: &UiCx<'_>, kind: OpKind) -> Option<OpState> {
    cx.state.account.ops.get(&my_op(kind)).cloned()
}

/// Forget a finished (or failed) write; a pending one stays until its answer.
fn finish_op(cx: &mut UiCx<'_>, kind: OpKind) {
    let ops = &mut cx.state.account.ops;
    if ops.get(&my_op(kind)).is_some_and(|s| *s != OpState::Pending) {
        ops.remove(&my_op(kind));
    }
}

/// Spinner while a write is on its way, its error text (selectable: captcha texts carry a link).
fn op_line(ui: &mut egui::Ui, pal: &Pal, state: Option<&OpState>, pending: &str) {
    match state {
        Some(OpState::Pending) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(pal.muted));
                ui.label(RichText::new(pending).size(pal.meta()).color(pal.muted));
            });
        }
        Some(OpState::Failed(e)) => {
            ui.add(egui::Label::new(RichText::new(e).size(pal.meta()).color(pal.danger)).wrap().selectable(true));
        }
        _ => {}
    }
}

/// Uppercase field label ("DISPLAY NAME").
fn field_label(ui: &mut egui::Ui, pal: &Pal, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(super::bold(pal.size - 2.5)).color(pal.muted));
}

/// Accent-filled button (Save, Edit User Profile).
fn primary(theme: &Theme, text: &str) -> egui::Button<'static> {
    let c = &theme.colors;
    egui::Button::new(RichText::new(text.to_owned()).color(c.accent_text.to_color32())).fill(c.accent.to_color32()).min_size(vec2(80.0, 32.0))
}

fn primary_button(ui: &mut egui::Ui, theme: &Theme, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, primary(theme, text))
}

/// Folder button that opens `dir` in the file manager (created first); `what` and the path are
/// its tooltip, so the page needs no path text.
fn folder_button(ui: &mut egui::Ui, cx: &mut UiCx<'_>, dir: &std::path::Path, what: &str) {
    let pal = Pal::new(cx.theme);
    let resp = super::messages::icon_button(ui, &pal, "", &format!("{what}\n{}", dir.display()), 28.0, false, pal.text);
    // Painted: the emoji font's 📂 is a thin tilted sketch at this size.
    let color = if resp.hovered() || resp.has_focus() { pal.text } else { pal.muted };
    let r = egui::Rect::from_center_size(resp.rect.center(), vec2(16.0, 12.0));
    let tab = [r.left_top(), r.left_top() + vec2(6.0, 0.0), r.left_top() + vec2(8.0, 2.0), r.right_top() + vec2(0.0, 2.0), r.right_bottom(), r.left_bottom()];
    ui.painter().add(egui::Shape::closed_line(tab.to_vec(), egui::Stroke::new(1.5, color)));
    if !resp.clicked() {
        return;
    }
    let opener = if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::process::Command::new(opener).arg(dir).spawn().map(drop)) {
        cx.view.toast(format!("Could not open the folder: {e}"), true);
    }
}

/// Mute toggle of a server row: a bell, crossed out while muted. The tooltip is the action.
fn bell_button(ui: &mut egui::Ui, pal: &Pal, muted: bool) -> egui::Response {
    let tip = if muted { "Unmute server" } else { "Mute server" };
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), muted, "Mute server"));
    if ui.is_rect_visible(rect) {
        let hot = resp.hovered() || resp.has_focus();
        let bg = if hot { pal.bg.blend(pal.tint()) } else { pal.bg };
        if hot {
            ui.painter().rect_filled(rect, pal.radius, bg);
        }
        let color = if muted { pal.danger } else if hot { pal.text } else { pal.muted };
        paint_bell(ui.painter(), rect.center(), color, bg, muted);
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A 20 px outline bell centred on `c` (the emoji font's 🔔 reads as a blob), crossed out while
/// `muted`; `bg` cuts the cross line free of the outline. Also the thread panel's bell.
pub(super) fn paint_bell(p: &egui::Painter, c: egui::Pos2, color: egui::Color32, bg: egui::Color32, muted: bool) {
    let s = 20.0;
    let stroke = egui::Stroke::new(1.6, color);
    // Dome from the left over the top, the sides down to a flared rim, the clapper below.
    let top = c - vec2(0.0, s * 0.08);
    let r = s * 0.28;
    let mut pts: Vec<egui::Pos2> = (0..=12).map(|i| top + r * egui::Vec2::angled(std::f32::consts::PI * (1.0 + i as f32 / 12.0))).collect();
    pts.extend([c + vec2(r, s * 0.18), c + vec2(r + s * 0.1, s * 0.27), c + vec2(-r - s * 0.1, s * 0.27), c + vec2(-r, s * 0.18)]);
    p.add(egui::Shape::closed_line(pts, stroke));
    p.circle_filled(c + vec2(0.0, s * 0.37), s * 0.07, color);
    if muted {
        let ends = [c - vec2(s * 0.4, s * 0.4), c + vec2(s * 0.4, s * 0.4)];
        p.line_segment(ends, egui::Stroke::new(4.0, bg));
        p.line_segment(ends, stroke);
    }
}

/// A pill switch (Discord's toggle): accent when on. `changed()` when clicked.
fn switch(ui: &mut egui::Ui, pal: &Pal, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *on, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let off = pal.muted.gamma_multiply(0.45);
        let fill = if ui.is_enabled() { off.lerp_to_gamma(pal.accent, t) } else { off };
        ui.painter().rect_filled(rect, 12.0, fill);
        let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), 9.0, egui::Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A setting row: label left, switch right, an optional hint under it. True when toggled.
fn switch_row(ui: &mut egui::Ui, theme: &Theme, label: &str, hint_text: Option<&str>, on: &mut bool) -> bool {
    let pal = Pal::new(theme);
    ui.add_space(4.0);
    let changed = ui
        .horizontal(|ui| {
            ui.label(RichText::new(label).color(pal.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                switch(ui, &pal, on, label).changed()
            })
            .inner
        })
        .inner;
    if let Some(h) = hint_text {
        hint(ui, theme, h);
    }
    ui.add_space(4.0);
    changed
}

#[cfg(test)]
mod tests {
    use super::Page;

    #[test]
    fn pages_have_names_and_search_finds_options() {
        let all: Vec<Page> = Page::GROUPS.iter().flat_map(|g| g.1.iter().copied()).collect();
        assert_eq!(all.len(), 28, "every page is in exactly one group");
        for p in &all {
            assert_eq!(Page::from_name(p.info().1), Some(*p));
        }
        let hits = |q: &str| all.iter().copied().filter(|p| p.matches(q)).collect::<Vec<_>>();
        assert_eq!(hits("zoom"), [Page::Accessibility]);
        // Local rows' labels, not only keywords.
        assert_eq!(hits("gg sans"), [Page::Appearance]);
        assert_eq!(hits("discord font"), [Page::Appearance]);
        assert!(hits("compact").contains(&Page::Appearance), "Message Display");
        assert!(hits("screen capture").contains(&Page::StreamerMode));
        assert!(hits("open inbox").contains(&Page::Keybinds), "a keybind action");
        assert!(hits("spoiler").contains(&Page::Chat), "a synced option's label");
        assert!(hits("developer").contains(&Page::Advanced));
        assert_eq!(hits("authenticator"), [Page::MyAccount]);
        assert_eq!(hits("avatar").first(), Some(&Page::Profiles), "the upload rows moved to Profiles");
        assert_eq!(hits("").len(), all.len());
        assert!(hits("no such setting").is_empty());
    }

    #[test]
    fn search_moves_to_a_match_and_back() {
        use super::search_page;
        let mut before = None;
        assert_eq!(search_page(Page::Chat, &mut before, "zoom"), Page::Accessibility);
        // Still a match while typing on: stays; the first page left is kept.
        assert_eq!(search_page(Page::Accessibility, &mut before, "zoo"), Page::Accessibility);
        assert_eq!(search_page(Page::Accessibility, &mut before, "developer"), Page::Advanced);
        assert_eq!(search_page(Page::Advanced, &mut before, "no such setting"), Page::Advanced);
        assert_eq!(search_page(Page::Advanced, &mut before, ""), Page::Chat);
        assert_eq!((search_page(Page::Chat, &mut before, ""), before), (Page::Chat, None));
    }
}
