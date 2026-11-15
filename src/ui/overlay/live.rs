//! The overlay's logic: what each pane shows, where, and when its window exists. Windows only.
//! Every root pass (`Overlay::logic`, also while minimized) works out the wanted state of the
//! voice and notification panes and `Pane::sync`s it: a pane with something to show is an egui
//! deferred viewport (a small window drawn by `paint.rs`), a pane without one has no window.
//! docs/OVERLAY_DESIGN.md has the why of every number here.

use super::{paint, win32};
use crate::activity::{AddedGame, Table};
use crate::config::{Corner, Settings, Show};
use crate::model::Id;
use crate::state::AppState;
use crate::theme::Theme;
use crate::ui::ViewState;
use crate::ui::images::{ImageCache, user_avatar_url};
use egui::{Color32, TextureId, Vec2, ViewportBuilder, ViewportCommand, ViewportId};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(super) const MARGIN: f32 = 16.0;
pub(super) const PAD: f32 = 4.0;
/// Room around an avatar for the speaking ring.
pub(super) const RING: f32 = 3.0;
/// Name pill width, padding included.
pub(super) const NAME_MAX: f32 = 160.0;
pub(super) const MAX_ROWS: usize = 10;
/// Fastest overlay repaint (10 Hz): speaking flickers no faster.
const MIN_GAP: Duration = Duration::from_millis(100);
const TOAST_FOR: Duration = Duration::from_secs(5);
const MAX_TOASTS: usize = 3;
pub(super) const TOAST_W: f32 = 320.0;
pub(super) const TOAST_H: f32 = 80.0;
pub(super) const GAP: f32 = 8.0;
/// A demo toast never expires (screenshots, idle measurements).
const DEMO_TOAST_FOR: Duration = Duration::from_secs(3600);
/// A demo screenshot run ends itself after this, whatever the panes did.
const DEMO_SHOT_LIMIT: Duration = Duration::from_secs(20);

const VOICE_ID: &str = "rc_overlay_voice";
const TOAST_ID: &str = "rc_overlay_toast";
pub(super) const VOICE_TITLE: &str = "Rustcord voice overlay";
pub(super) const TOAST_TITLE: &str = "Rustcord notification overlay";

/// Demo panes still waiting for their screenshot (`paint::show` counts them down).
pub(super) static SHOTS_PENDING: AtomicUsize = AtomicUsize::new(0);

/// `RUSTCORD_OVERLAY_DEMO`: fake call members and a notification, over Rustcord's own window
/// (`1`, any value but `gated`) or, with `gated`, only while a registered game is in front.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Demo {
    Always,
    Gated,
}

pub(super) fn demo() -> Option<Demo> {
    static D: OnceLock<Option<Demo>> = OnceLock::new();
    *D.get_or_init(|| std::env::var("RUSTCORD_OVERLAY_DEMO").ok().map(|v| if v == "gated" { Demo::Gated } else { Demo::Always }))
}

fn screenshot_run() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| std::env::var_os("RUSTCORD_SCREENSHOT").is_some())
}

/// `<RUSTCORD_SCREENSHOT stem>-overlay-voice.png` / `-overlay-toast.png`, only with a demo.
fn demo_shot(title: &str) -> Option<PathBuf> {
    demo()?;
    let path = PathBuf::from(std::env::var_os("RUSTCORD_SCREENSHOT")?);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let which = if title == VOICE_TITLE { "voice" } else { "toast" };
    Some(path.with_file_name(format!("{stem}-overlay-{which}.png")))
}

#[derive(Default)]
pub struct Overlay {
    voice: Pane,
    toast: Pane,
    toasts: Vec<Toast>,
    demo_toast: bool,
    /// Last shown state per pane, for the `gated` demo's log lines.
    logged: [bool; 2],
    /// Demo screenshot runs: when the first pass ran, and whether a pane waited for its shot.
    started: Option<Instant>,
    shots_seen: bool,
}

struct Toast {
    title: String,
    body: String,
    author: Option<Id>,
    until: Instant,
}

/// Some while declared: the builder (constant for the window's life) and a fresh state per window.
#[derive(Default)]
struct Pane {
    live: Option<(ViewportBuilder, Arc<Mutex<PaneState>>)>,
}

/// What `paint::show` shares with `logic` (HWNDs are kept as `isize`: `HWND` is not `Send`).
pub(super) struct PaneState {
    pub want: Want,
    /// Placement attempts since `want` last changed.
    pub tries: u8,
    pub shown: bool,
    pub styled: Option<(isize, bool)>,
    pub last_paint: Option<Instant>,
    pub created: Instant,
    pub shot: Option<PathBuf>,
    pub title: &'static str,
}

#[derive(Clone, PartialEq)]
pub(super) struct Want {
    pub scene: Scene,
    pub colors: Colors,
    /// The game's area in physical pixels: x, y, w, h.
    pub area: [i32; 4],
    pub size: Vec2,
    pub corner: Corner,
    /// Points away from the corner's edge.
    pub offset: f32,
    pub hide_from_capture: bool,
}

#[derive(Clone, PartialEq)]
pub(super) enum Scene {
    Voice(VoiceScene),
    Toasts(Vec<ToastRow>),
}

#[derive(Clone, PartialEq)]
pub(super) struct VoiceScene {
    pub rows: Vec<Row>,
    pub names: Show,
    pub users: Show,
    pub large: bool,
}

#[derive(Clone, PartialEq)]
pub(super) struct Row {
    pub id: Id,
    pub name: String,
    pub tex: Option<TextureId>,
    pub speaking: bool,
    pub muted: bool,
    pub deaf: bool,
    pub by_server: bool,
}

#[derive(Clone, PartialEq)]
pub(super) struct ToastRow {
    pub title: String,
    pub body: String,
    pub tex: Option<TextureId>,
    pub seed: u64,
}

/// The theme's colours the panes use, read once per pass so the paint thread never sees `Theme`.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Colors {
    pub text: Color32,
    pub muted: Color32,
    pub card: Color32,
    pub rule: Color32,
    pub hc: bool,
    pub positive: Color32,
    pub danger: Color32,
    pub on_media: Color32,
    pub on_media_muted: Color32,
    pub pill: Color32,
    pub card_r: u8,
}

impl Colors {
    fn new(theme: &Theme) -> Self {
        let tk = theme.tokens();
        Self {
            text: theme.colors.text.to_color32(),
            muted: theme.colors.text_muted.to_color32(),
            card: tk.raised,
            rule: tk.rule,
            hc: crate::theme::is_high_contrast(&theme.colors),
            positive: tk.positive,
            danger: tk.danger_fill,
            on_media: tk.on_media,
            on_media_muted: tk.on_media_muted,
            // As on the call tiles: dark enough over any game.
            pill: Color32::from_black_alpha(170),
            // The container tier (1.5 × the theme's radius).
            card_r: (theme.layout.corner_radius * 1.5).round().clamp(0.0, 255.0) as u8,
        }
    }
}

/// Avatar size, row height, name pill height and name font size of the voice widget.
pub(super) fn dims(large: bool) -> (f32, f32, f32, f32) {
    if large { (32.0, 40.0, 24.0, 14.0) } else { (24.0, 32.0, 20.0, 13.0) }
}

impl Overlay {
    /// Every root pass, including logic-only ones (minimized, in the tray).
    pub fn logic(&mut self, ctx: &egui::Context, state: &AppState, view: &ViewState, settings: &Settings, theme: &Theme, images: &mut ImageCache) {
        let demo = demo();
        if !active(settings) {
            self.voice.sync(ctx, VOICE_ID, VOICE_TITLE, None);
            self.toast.sync(ctx, TOAST_ID, TOAST_TITLE, None);
            self.toasts.clear();
            return;
        }
        // A screenshot run (not a demo) never starts the thread; a disabled overlay neither.
        win32::watch(ctx);
        let o = &settings.overlay;
        let table = state.activity.table.as_deref();
        let area = match demo {
            // Rustcord's own window plays the game, so nothing private is under the panes.
            Some(Demo::Always) => ctx.input(|i| {
                let v = i.viewport();
                let k = v.native_pixels_per_point.unwrap_or(1.0);
                v.inner_rect.map(|r| [(r.min.x * k).round() as i32, (r.min.y * k).round() as i32, (r.width() * k).round() as i32, (r.height() * k).round() as i32])
            }),
            _ => win32::foreground().filter(|f| is_game(f, table, &state.activity.added)).and_then(|f| win32::game_area(f.hwnd)),
        };

        let now = Instant::now();
        if demo.is_some() && !self.demo_toast {
            self.demo_toast = true;
            let user = state.users.values().min_by_key(|u| u.id);
            let title = user.map_or_else(|| "Ferris".to_owned(), |u| u.display_name().to_owned());
            self.toasts.push(Toast { title, body: "Anyone up for a round after this match?".into(), author: user.map(|u| u.id), until: now + DEMO_TOAST_FOR });
        }
        self.toasts.retain(|t| t.until > now);
        if let Some(next) = self.toasts.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }

        let colors = Colors::new(theme);
        let hide_from_capture = settings.streamer.enabled && settings.streamer.hide_from_capture;
        let voice_want = area.filter(|_| o.voice_corner != Corner::Off).and_then(|area| {
            let mut rows = if demo.is_some() { demo_rows(state) } else { voice_rows(state, view) };
            if rows.is_empty() {
                return None;
            }
            // Every pass, so the avatars stay out of the idle eviction (it runs at the root's `end_frame`, after this).
            for r in &mut rows {
                r.tex = avatar(ctx, images, state, r.id);
            }
            let size = voice_size(rows.len(), o.show_names, o.show_users, o.large_avatars);
            let scene = Scene::Voice(VoiceScene { rows, names: o.show_names, users: o.show_users, large: o.large_avatars });
            Some(Want { scene, colors, area, size, corner: o.voice_corner, offset: 0.0, hide_from_capture })
        });
        let toast_want = area.filter(|_| o.notification_corner != Corner::Off && !self.toasts.is_empty()).map(|area| {
            let bottom = matches!(o.notification_corner, Corner::BottomLeft | Corner::BottomRight);
            let mut rows: Vec<ToastRow> = self
                .toasts
                .iter()
                .map(|t| ToastRow { title: t.title.clone(), body: t.body.clone(), tex: t.author.and_then(|id| avatar(ctx, images, state, id)), seed: t.author.map_or(0, |id| id.0) })
                .collect();
            // Newest nearest the corner's edge: first for the top corners, last for the bottom ones.
            if !bottom {
                rows.reverse();
            }
            let n = rows.len() as f32;
            let size = Vec2::new(TOAST_W, n * TOAST_H + (n - 1.0) * GAP);
            let offset = voice_want.as_ref().filter(|v| v.corner == o.notification_corner).map_or(0.0, |v| v.size.y + GAP);
            Want { scene: Scene::Toasts(rows), colors, area, size, corner: o.notification_corner, offset, hide_from_capture }
        });

        if demo.is_some() {
            for (i, (name, want)) in [("voice", &voice_want), ("toast", &toast_want)].into_iter().enumerate() {
                if self.logged[i] != want.is_some() {
                    self.logged[i] = want.is_some();
                    eprintln!("overlay: {name} {}", if want.is_some() { "shown" } else { "hidden" });
                }
            }
        }
        self.voice.sync(ctx, VOICE_ID, VOICE_TITLE, voice_want);
        self.toast.sync(ctx, TOAST_ID, TOAST_TITLE, toast_want);
        if demo.is_some() && screenshot_run() {
            self.end_demo_run(ctx, now);
        }
    }

    /// Demo screenshot runs: the root's own screenshot (ui/mod.rs) leaves closing to us, so the
    /// panes get their shot; then (or after `DEMO_SHOT_LIMIT`) the window closes.
    fn end_demo_run(&mut self, ctx: &egui::Context, now: Instant) {
        let started = *self.started.get_or_insert(now);
        let pending = SHOTS_PENDING.load(Ordering::Relaxed);
        self.shots_seen |= pending > 0;
        if (self.shots_seen && pending == 0) || now.duration_since(started) > DEMO_SHOT_LIMIT {
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// A notification that would go to the desktop: shown on the overlay instead when a game is
    /// in front (true), else left to the caller (false).
    pub fn notify(&mut self, state: &AppState, settings: &Settings, title: &str, body: &str, author: Option<Id>) -> bool {
        if !active(settings) || settings.overlay.notification_corner == Corner::Off {
            return false;
        }
        let game = match demo() {
            Some(Demo::Always) => true,
            _ => win32::foreground().is_some_and(|f| is_game(&f, state.activity.table.as_deref(), &state.activity.added)),
        };
        if !game {
            return false;
        }
        self.push(title, body, author);
        true
    }

    /// Newest last; the oldest goes past `MAX_TOASTS`.
    fn push(&mut self, title: &str, body: &str, author: Option<Id>) {
        self.toasts.push(Toast { title: title.to_owned(), body: body.replace('\n', " "), author, until: Instant::now() + TOAST_FOR });
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }
}

/// The overlay is on (or a demo runs); a screenshot run without a demo never shows one.
fn active(settings: &Settings) -> bool {
    let demo = demo();
    (settings.overlay.enabled || demo.is_some()) && !(screenshot_run() && demo.is_none())
}

/// Texture of a user's avatar (their own avatar: the global one, which is all a toast knows).
fn avatar(ctx: &egui::Context, images: &mut ImageCache, state: &AppState, id: Id) -> Option<TextureId> {
    let user = state.users.get(&id).or(state.me.as_ref().filter(|u| u.id == id))?;
    let ch = state.voice.channel_id;
    let guild_id = ch.and_then(|c| state.channels.get(&c)).and_then(|c| c.guild_id);
    let member = ch.and_then(|c| state.voice_states.get(&c)).and_then(|m| m.get(&id)).and_then(|v| v.member.as_ref());
    images.get(ctx, &user_avatar_url(guild_id, user, member, 128, false), 64, false).map(|t| t.id)
}

impl Pane {
    /// Brings the pane's window in line with `want`: created when it first has something to
    /// show, repainted (at most 10 Hz) when it changes, gone when it has nothing.
    fn sync(&mut self, ctx: &egui::Context, id: &str, title: &'static str, want: Option<Want>) {
        let id = ViewportId::from_hash_of(id);
        let Some(w) = want else {
            if self.live.take().is_some() {
                // The root's next full pass drops the undeclared viewport; hidden meanwhile.
                ctx.send_viewport_cmd_to(id, ViewportCommand::Visible(false));
                ctx.request_repaint();
            }
            return;
        };
        let (builder, shared) = self.live.get_or_insert_with(|| {
            // Constant for the window's life: a changed builder would make egui move and resize the window against `win32::set_client_rect`.
            let builder = builder(title, &w, ctx.pixels_per_point());
            let state = PaneState { want: w.clone(), tries: 0, shown: false, styled: None, last_paint: None, created: Instant::now(), shot: demo_shot(title), title };
            if state.shot.is_some() {
                SHOTS_PENDING.fetch_add(1, Ordering::Relaxed);
            }
            (builder, Arc::new(Mutex::new(state)))
        });
        {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            if s.want != w {
                s.want = w;
                s.tries = 0;
                let wait = MIN_GAP.saturating_sub(s.last_paint.map_or(MIN_GAP, |t| t.elapsed()));
                ctx.request_repaint_after_for(wait, id);
            }
        }
        let shared = Arc::clone(shared);
        ctx.show_viewport_deferred(id, builder.clone(), move |ui, _| paint::show(ui, &shared));
        // Minimized or in the tray the root runs logic-only passes, which create no windows.
        if !ctx.input(|i| i.raw.viewports.contains_key(&id)) {
            ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
        }
    }
}

/// The window's first guess (points of the root's scale); `paint::show` places it exactly.
fn builder(title: &str, w: &Want, ppp: f32) -> ViewportBuilder {
    let r = place_want(w, ppp);
    let at = egui::pos2(r[0] as f32 / ppp, r[1] as f32 / ppp);
    ViewportBuilder::default()
        .with_title(title)
        .with_position(at)
        .with_inner_size(Vec2::new(r[2] as f32 / ppp, r[3] as f32 / ppp))
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_active(false)
        .with_drag_and_drop(false)
        .with_visible(false)
}

/// Where `w` goes, in physical pixels at `ppp`.
pub(super) fn place_want(w: &Want, ppp: f32) -> [i32; 4] {
    let px = |v: f32| (v * ppp).round() as i32;
    place(w.area, [px(w.size.x), px(w.size.y)], w.corner, px(MARGIN), px(w.offset))
}

/// The foreground window is a game: same rule as `activity::scan::pick`: an added game by its
/// full path, else the detectable-games table; a process that won't tell its path counts by its
/// file name alone. Rustcord itself never is one.
fn is_game(fg: &win32::Foreground, table: Option<&Table>, added: &[AddedGame]) -> bool {
    if fg.pid == std::process::id() {
        return false;
    }
    let p = fg.path.as_deref().unwrap_or(&fg.name);
    added.iter().any(|g| g.exe == p) || table.is_some_and(|t| t.find(p).is_some())
}

/// Everyone in my call (me even before my voice state echoes), by lowercase name, then id.
/// `tex` is filled in by `logic`.
fn voice_rows(state: &AppState, view: &ViewState) -> Vec<Row> {
    let Some(ch) = state.voice.channel_id else { return Vec::new() };
    let guild_id = state.channels.get(&ch).and_then(|c| c.guild_id);
    let me = state.me.as_ref().map(|u| u.id);
    let states = state.voice_states.get(&ch);
    let mut ids: Vec<Id> = states.map(|m| m.keys().copied().collect()).unwrap_or_default();
    ids.extend(me.filter(|me| !ids.contains(me)));
    let mut rows: Vec<Row> = ids
        .into_iter()
        .map(|id| {
            let vs = states.and_then(|m| m.get(&id));
            let user = state.users.get(&id).or(state.me.as_ref().filter(|u| u.id == id));
            let name = user.map_or_else(|| "Unknown user".into(), |u| state.member_display_name(guild_id, u, vs.and_then(|v| v.member.as_ref())));
            let (mut muted, mut deaf) = (vs.is_some_and(|v| v.self_mute || v.mute), vs.is_some_and(|v| v.self_deaf || v.deaf));
            if Some(id) == me {
                (muted, deaf) = (muted || view.self_mute, deaf || view.self_deaf);
            }
            Row { id, name, tex: None, speaking: state.voice.speaking.contains(&id), muted, deaf, by_server: vs.is_some_and(|v| v.mute || v.deaf) }
        })
        .collect();
    rows.sort_by_cached_key(|r| (r.name.to_lowercase(), r.id));
    rows
}

/// Me plus the first four cached users, for `RUSTCORD_OVERLAY_DEMO`: rows 0 and 2 speak, row 1
/// is muted, row 3 is deafened by the server.
fn demo_rows(state: &AppState) -> Vec<Row> {
    let me = state.me.as_ref();
    let mut users: Vec<&crate::model::User> = state.users.values().filter(|u| Some(u.id) != me.map(|m| m.id)).collect();
    users.sort_by_key(|u| u.id);
    let mut rows: Vec<Row> = me
        .into_iter()
        .chain(users.into_iter().take(4))
        .map(|u| Row { id: u.id, name: u.display_name().to_owned(), tex: None, speaking: false, muted: false, deaf: false, by_server: false })
        .collect();
    rows.sort_by_cached_key(|r| (r.name.to_lowercase(), r.id));
    for (i, r) in rows.iter_mut().enumerate() {
        r.speaking = i == 0 || i == 2;
        r.muted = i == 1;
        (r.deaf, r.by_server) = (i == 3, i == 3);
    }
    rows
}

/// Window rows for `n` people: at most `MAX_ROWS`, plus the "N more" row unless only speakers show.
pub(super) fn slots(n: usize, users: Show) -> usize {
    let shown = n.min(MAX_ROWS);
    if users == Show::Speaking { shown } else { shown + usize::from(n > MAX_ROWS) }
}

/// The voice window's size in points.
fn voice_size(n: usize, names: Show, users: Show, large: bool) -> Vec2 {
    let (a, row, ..) = dims(large);
    let w = PAD + a + 2.0 * RING + if names == Show::Never { 0.0 } else { GAP + NAME_MAX } + PAD;
    Vec2::new(w, 2.0 * PAD + slots(n, users) as f32 * row)
}

/// The window's client rect (physical x, y, w, h): in `corner` of `area`, `margin` in from its
/// edges and `offset` further away from the corner's horizontal edge, kept inside the area.
pub(super) fn place(area: [i32; 4], size: [i32; 2], corner: Corner, margin: i32, offset: i32) -> [i32; 4] {
    let ([ax, ay, aw, ah], [w, h]) = (area, size);
    let x = match corner {
        Corner::TopRight | Corner::BottomRight => ax + aw - margin - w,
        _ => ax + margin,
    };
    let y = match corner {
        Corner::BottomLeft | Corner::BottomRight => ay + ah - margin - offset - h,
        _ => ay + margin + offset,
    };
    [x.clamp(ax, ax.max(ax + aw - w)), y.clamp(ay, ay.max(ay + ah - h)), w, h]
}

/// The rows to draw and how many more there are (the "N more" row).
pub(super) fn visible(scene: &VoiceScene) -> (Vec<&Row>, usize) {
    if scene.users == Show::Speaking {
        (scene.rows.iter().filter(|r| r.speaking).take(MAX_ROWS).collect(), 0)
    } else {
        (scene.rows.iter().take(MAX_ROWS).collect(), scene.rows.len().saturating_sub(MAX_ROWS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{User, VoiceState};

    fn user(id: u64, name: &str) -> User {
        User { id: Id(id), username: name.into(), ..Default::default() }
    }

    #[test]
    fn places_in_each_corner_with_margin_offset_and_clamp() {
        let area = [100, 50, 1000, 600];
        assert_eq!(place(area, [200, 100], Corner::TopLeft, 16, 0), [116, 66, 200, 100]);
        assert_eq!(place(area, [200, 100], Corner::TopRight, 16, 0), [884, 66, 200, 100]);
        assert_eq!(place(area, [200, 100], Corner::BottomLeft, 16, 0), [116, 534, 200, 100]);
        assert_eq!(place(area, [200, 100], Corner::BottomRight, 16, 0), [884, 534, 200, 100]);
        // Offset goes away from the edge: down from the top, up from the bottom.
        assert_eq!(place(area, [200, 100], Corner::TopLeft, 16, 108)[1], 174);
        assert_eq!(place(area, [200, 100], Corner::BottomRight, 16, 108)[1], 426);
        // Bigger than the area: pinned to its top left, never outside it.
        assert_eq!(place([10, 20, 100, 60], [300, 200], Corner::BottomRight, 16, 0), [10, 20, 300, 200]);
        // An offset past the edge is clamped back in.
        assert_eq!(place(area, [200, 100], Corner::BottomLeft, 16, 5000)[1], 50);
    }

    #[test]
    fn slots_and_size_follow_the_options() {
        assert_eq!(slots(3, Show::Always), 3);
        assert_eq!(slots(10, Show::Always), 10);
        assert_eq!(slots(11, Show::Always), 11, "ten and the more row");
        assert_eq!(slots(40, Show::Always), 11);
        assert_eq!(slots(40, Show::Speaking), 10, "no more row for speakers only");
        let s = voice_size(2, Show::Always, Show::Always, false);
        assert_eq!((s.x, s.y), (PAD + 24.0 + 2.0 * RING + GAP + NAME_MAX + PAD, 2.0 * PAD + 64.0));
        let big = voice_size(2, Show::Always, Show::Always, true);
        assert!(big.x > s.x - 1e-3 && big.y > s.y);
        let narrow = voice_size(2, Show::Never, Show::Always, false);
        assert_eq!(s.x - narrow.x, GAP + NAME_MAX);
    }

    #[test]
    fn rows_list_everyone_with_me_first_in_name_order() {
        let mut st = AppState::default();
        let ch = Id(500);
        st.me = Some(user(1, "zed"));
        st.users.insert(Id(2), user(2, "Bob"));
        st.users.insert(Id(3), user(3, "alice"));
        let mut view = ViewState::default();
        assert!(voice_rows(&st, &view).is_empty(), "not in voice");
        st.voice.channel_id = Some(ch);
        // Me before my voice state echoes, plus two others: one muted by a moderator, one speaking.
        st.voice_states.entry(ch).or_default().insert(Id(2), VoiceState { user_id: Id(2), channel_id: Some(ch), mute: true, ..Default::default() });
        st.voice_states.entry(ch).or_default().insert(Id(3), VoiceState { user_id: Id(3), channel_id: Some(ch), self_deaf: true, ..Default::default() });
        st.voice.speaking.insert(Id(3));
        let rows = voice_rows(&st, &view);
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["alice", "Bob", "zed"]);
        let (alice, bob, me) = (&rows[0], &rows[1], &rows[2]);
        assert!(alice.speaking && alice.deaf && !alice.muted && !alice.by_server);
        assert!(bob.muted && bob.by_server && !bob.speaking);
        assert!(!me.muted && !me.deaf);
        // My own mute and deafen live in the view until the echo.
        (view.self_mute, view.self_deaf) = (true, true);
        let me = voice_rows(&st, &view).pop().unwrap();
        assert!(me.muted && me.deaf && !me.by_server);
        // An unknown user keeps a row.
        st.voice_states.entry(ch).or_default().insert(Id(9), VoiceState { user_id: Id(9), channel_id: Some(ch), ..Default::default() });
        assert!(voice_rows(&st, &view).iter().any(|r| r.name == "Unknown user"));
    }

    #[test]
    fn visible_rows_cap_and_count_the_rest() {
        let row = |i: u64, speaking| Row { id: Id(i), name: format!("u{i}"), tex: None, speaking, muted: false, deaf: false, by_server: false };
        let rows: Vec<Row> = (0..14).map(|i| row(i, i % 5 == 0)).collect();
        let mut scene = VoiceScene { rows, names: Show::Always, users: Show::Always, large: false };
        let (shown, more) = visible(&scene);
        assert_eq!((shown.len(), more, shown[0].id), (10, 4, Id(0)));
        scene.users = Show::Speaking;
        let (shown, more) = visible(&scene);
        assert_eq!((shown.iter().map(|r| r.id.0).collect::<Vec<_>>(), more), (vec![0, 5, 10], 0));
        scene.rows.truncate(3);
        scene.users = Show::Always;
        assert_eq!(visible(&scene).1, 0);
    }

    #[test]
    fn game_rule_matches_the_scan() {
        let json = r#"[{"id":"1","name":"Factorio","executables":[{"name":"factorio.exe","os":"win32"}]},
                        {"id":"2","name":"Celeste","executables":[{"name":"celeste/celeste.exe","os":"win32"}]}]"#;
        let t = Table::from_json(json.as_bytes()).unwrap();
        let fg = |pid: u32, path: Option<&str>, name: &str| win32::Foreground { hwnd: 1, pid, path: path.map(str::to_owned), name: name.into() };
        let added = [AddedGame { name: "Mine".into(), exe: "c:/my/mygame.exe".into() }];
        assert!(is_game(&fg(7, Some("c:/games/factorio/factorio.exe"), "factorio.exe"), Some(&t), &[]));
        assert!(is_game(&fg(7, Some("c:/my/mygame.exe"), "mygame.exe"), None, &added), "added by full path");
        assert!(!is_game(&fg(7, Some("e:/mygame.exe"), "mygame.exe"), Some(&t), &added), "same name elsewhere");
        assert!(!is_game(&fg(7, Some("c:/windows/notepad.exe"), "notepad.exe"), Some(&t), &added));
        assert!(!is_game(&fg(std::process::id(), Some("c:/games/factorio/factorio.exe"), "factorio.exe"), Some(&t), &[]), "never Rustcord itself");
        // A protected process gives only its name: bare-name entries match, folder entries do not.
        assert!(is_game(&fg(7, None, "factorio.exe"), Some(&t), &[]));
        assert!(!is_game(&fg(7, None, "celeste.exe"), Some(&t), &[]));
    }

    #[test]
    fn toasts_cap_at_three_and_need_a_game_and_a_corner() {
        let st = AppState::default();
        let mut s = Settings::default();
        let mut o = Overlay::default();
        // No demo in unit tests and no game in front of the test process: the desktop shows it.
        assert!(!o.notify(&st, &s, "a", "b", None));
        assert!(o.toasts.is_empty());
        // Past the gate: the cap, the order and one line per body.
        for i in 0..5 {
            o.push(&format!("t{i}"), "two\nlines", None);
        }
        assert_eq!(o.toasts.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["t2", "t3", "t4"]);
        assert_eq!(o.toasts[0].body, "two lines");
        s.overlay.notification_corner = Corner::Off;
        assert!(!o.notify(&st, &s, "a", "b", None));
        s.overlay.notification_corner = Corner::TopLeft;
        s.overlay.enabled = false;
        assert!(!o.notify(&st, &s, "a", "b", None));
    }

    #[test]
    fn demo_rows_mark_speaking_muted_and_deafened() {
        let mut st = AppState::default();
        st.me = Some(user(1, "Me"));
        for i in 2..9 {
            st.users.insert(Id(i), user(i, &format!("user{i}")));
        }
        let rows = demo_rows(&st);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.iter().map(|r| r.speaking).collect::<Vec<_>>(), [true, false, true, false, false]);
        assert!(rows[1].muted && !rows[1].deaf && !rows[1].by_server && rows[3].deaf && rows[3].by_server);
    }
}
