//! Self-update from GitHub releases (docs/UPDATER_DESIGN.md, with its design review's changes).
//! A signed feed is checked in the background at start and every 6 h; a newer package is
//! downloaded, verified and staged in `<cache>/update/`, then installed behind the update screen
//! (found right after start, a click on the title-bar button, or the next start) or silently
//! after 10 minutes in the tray, never during a call. Nothing blocks the first frame and no
//! failure blocks the app; portable, dev and read-only copies only get a notice.
//!
//! `feed`: key, signature, manifest, HTTP, download. `store`: state.toml. `machine`: every
//! decision, without I/O. `platform`: install kind, apply, leftovers. `state`: the UI contract.
//! One task on the app's runtime runs the machine's actions; the pub fns below only send it
//! events, so they never block the UI thread.
//! OWNERSHIP: updater core.

mod feed;
mod machine;
mod platform;
mod state;
mod store;

pub use state::{Kind, Phase, Status, status};

use machine::{Action, ApplyError, Engine, StartPlan, Ui};
use reqwest::Url;
use semver::Version;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

enum Event {
    Ui,
    Button,
    Install,
    Dismiss,
    CheckNow,
    SetAuto(bool),
    Checked(Result<feed::Offer, feed::CheckError>),
    Progress(u64),
    Downloaded(Result<(), feed::DownloadError>),
    /// Ok(true): the new copy already runs.
    Applied(Result<bool, ApplyError>),
}

static TX: OnceLock<UnboundedSender<Event>> = OnceLock::new();
static CTX: OnceLock<egui::Context> = OnceLock::new();
static HIDDEN: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static INTERACTED: AtomicBool = AtomicBool::new(false);
/// Windows and Linux: the installed new version, started by `relaunch` once the event loop ended.
static RELAUNCH: Mutex<Option<std::process::Command>> = Mutex::new(None);
/// The updater asked for the quit (`Action::Quit`), not the user.
static QUIT_FOR_UPDATE: AtomicBool = AtomicBool::new(false);

/// §5: whether and against what the updater runs, from the `RUSTCORD_*` variables.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// The reason is logged when not empty.
    Off(&'static str),
    /// Portable data (`RUSTCORD_HOME` only): checks, never downloads.
    Notice,
    Full,
    Test(TestHooks),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestHooks {
    feed: Url,
    key: Option<[u8; 32]>,
    from: Option<Version>,
    /// `RUSTCORD_SETUP_PREFIX` with `RUSTCORD_SETUP_TEST=1`: the Linux tarball install root.
    prefix: Option<PathBuf>,
}

/// `vars`: the `RUSTCORD_*` variables, names upper-cased. A test feed also takes
/// `RUSTCORD_OFFLINE` (an end-to-end run must not read the saved login or reach Discord; the feed
/// is loopback only anyway), `RUSTCORD_TRACE_STARTUP` (first-frame timing with a slow feed) and
/// the screenshot hooks (the real update screen in an end-to-end run).
fn mode(vars: &[(String, String)], debug: bool) -> Mode {
    const TEST_VARS: [&str; 11] = [
        "RUSTCORD_UPDATE_FEED",
        "RUSTCORD_UPDATE_PUBKEY",
        "RUSTCORD_UPDATE_FROM",
        "RUSTCORD_HOME",
        "RUSTCORD_SETUP_TEST",
        "RUSTCORD_SETUP_PREFIX",
        "RUSTCORD_OFFLINE",
        "RUSTCORD_TRACE_STARTUP",
        "RUSTCORD_SCREENSHOT",
        "RUSTCORD_SCREENSHOT_FRAME",
        "RUSTCORD_WINDOW_SIZE",
    ];
    let get = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if let Some(feed) = get("RUSTCORD_UPDATE_FEED") {
        let others = vars.iter().any(|(k, _)| !TEST_VARS.contains(&k.as_str()));
        let key = get("RUSTCORD_UPDATE_PUBKEY").map(feed::decode_key);
        let from = get("RUSTCORD_UPDATE_FROM").map(|v| Version::parse(v).ok());
        return match (feed::loopback_url(feed), key, from) {
            _ if others => Mode::Off("RUSTCORD_UPDATE_FEED goes with no other RUSTCORD_ variable but the test ones"),
            (None, ..) => Mode::Off("RUSTCORD_UPDATE_FEED is not an http(s) URL on 127.0.0.1, [::1] or localhost"),
            (_, Some(None), _) => Mode::Off("RUSTCORD_UPDATE_PUBKEY is not the base64 of an Ed25519 key"),
            (_, _, Some(None)) => Mode::Off("RUSTCORD_UPDATE_FROM is not a version"),
            (Some(feed), key, from) => {
                let prefix = get("RUSTCORD_SETUP_PREFIX").filter(|_| get("RUSTCORD_SETUP_TEST") == Some("1")).map(PathBuf::from);
                Mode::Test(TestHooks { feed, key: key.flatten(), from: from.flatten(), prefix })
            }
        };
    }
    if debug || vars.iter().any(|(k, _)| k != "RUSTCORD_HOME") {
        Mode::Off("")
    } else if vars.is_empty() {
        Mode::Full
    } else {
        Mode::Notice
    }
}

/// main.rs only, once, in the eframe creator after preload.join(), before RustcordApp::new.
/// < 1 ms (env, argv, one registry read or a few `stat`s, state.toml); sets Status (incl. the
/// screen for an update applied at start) before the first frame.
pub fn start(rt: &tokio::runtime::Handle, ctx: &egui::Context, auto: bool) {
    let vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().to_ascii_uppercase(), v.to_string_lossy().into_owned()))
        .filter(|(k, _)| k.starts_with("RUSTCORD_"))
        .collect();
    let mode = mode(&vars, cfg!(debug_assertions));
    let test = match &mode {
        Mode::Off(why) => {
            if !why.is_empty() {
                eprintln!("rustcord: updater off: {why}");
            }
            return;
        }
        Mode::Test(t) => Some(t.clone()),
        Mode::Notice | Mode::Full => None,
    };
    let src = match &test {
        Some(t) => feed::Source::test(t.feed.clone(), t.key),
        None => feed::Source::github(),
    };
    let Some(src) = src else { return };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hidden = args.iter().any(|a| a == "--minimized");
    let after_update = args.iter().any(|a| a == "--after-update");
    let running = test.as_ref().and_then(|t| t.from.clone()).unwrap_or_else(|| Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0)));
    let exe = std::env::current_exe().unwrap_or_default();
    let install = if mode == Mode::Notice {
        platform::Install { can: Kind::NoticeOnly, key: None, exe, test_identity: false }
    } else {
        platform::detect(exe, test.is_some(), test.as_ref().and_then(|t| t.prefix.as_deref()))
    };

    let dir = store::dir();
    let state_file = dir.join("state.toml");
    let mut saved = store::Saved::load(&state_file);
    let loaded = saved.clone();
    let setup_error = saved.attempted.as_ref().and_then(|_| store::take_setup_error(&dir));
    let staged_ok = saved.staged.as_ref().zip(install.key).is_some_and(|(s, k)| std::fs::metadata(dir.join(k.file(&s.version))).is_ok_and(|m| m.len() == s.size));
    let macos = cfg!(target_os = "macos");
    let plan = machine::plan_start(&mut saved, &running, install.can, auto, hidden, macos, staged_ok, setup_error.as_deref());
    if saved != loaded
        && let Err(e) = saved.save(&state_file)
    {
        eprintln!("rustcord: update: could not save {}: {e}", state_file.display());
    }
    let engine = Engine::new(&plan, saved, running.clone(), install.can, install.key, dir.clone(), auto, hidden, Instant::now());
    HIDDEN.store(hidden, Ordering::Relaxed);
    let _ = CTX.set(ctx.clone());
    publish(&engine.status);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    if TX.set(tx.clone()).is_err() {
        return;
    }
    let task = Task { tx, src, install, dir, state_file, running };
    rt.spawn(task.run(engine, plan, rx, after_update));
}

/// Every RustcordApp::logic pass. hidden = window waits in the tray; busy = in a voice
/// channel/call (joining included); interacted = a key or pointer button was pressed since
/// start (sticky).
pub fn sync_ui(hidden: bool, busy: bool, interacted: bool) {
    let changed = (HIDDEN.swap(hidden, Ordering::Relaxed) != hidden) | (BUSY.swap(busy, Ordering::Relaxed) != busy) | (INTERACTED.swap(interacted, Ordering::Relaxed) != interacted);
    if changed {
        send(Event::Ui);
    }
}

/// The title-bar button (Available/Ready/Manual). NoticeOnly opens the release page.
pub fn button_clicked() {
    let s = status();
    if let (Phase::Available { version }, Kind::NoticeOnly, true) = (&s.phase, s.kind, s.enabled) {
        // Built from the verified version, never from a URL in the manifest.
        if let Some(ctx) = CTX.get() {
            ctx.open_url(egui::OpenUrl::new_tab(format!("{}/tag/v{version}", feed::RELEASES)));
        }
        return;
    }
    send(Event::Button);
}

/// The update screen's primary "Install" (Ready + Kind::Package).
pub fn install() {
    send(Event::Install);
}

/// The screen's "Not now" / "Continue", Esc, a click on an error. Ignored during
/// Installing/Restarting.
pub fn dismiss() {
    send(Event::Dismiss);
}

/// Settings "Check for updates".
pub fn check_now() {
    send(Event::CheckNow);
}

/// Settings "Install updates automatically" changed (ui also saves Settings::auto_update).
pub fn set_auto(on: bool) {
    send(Event::SetAuto(on));
}

/// main.rs, after the event loop ended: starts the installed version, hidden when this window
/// was. Linux execs it in this process (a systemd unit that started Rustcord through XDG
/// autostart keeps the PID); Windows spawns it. A Quit by the user while the restart waited for
/// a call to end stays a quit: the new version runs at the next start.
pub fn relaunch() {
    if !QUIT_FOR_UPDATE.load(Ordering::Relaxed) {
        return;
    }
    let Some(mut cmd) = RELAUNCH.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    cmd.args(HIDDEN.load(Ordering::Relaxed).then_some("--minimized"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        eprintln!("rustcord: update: could not restart in place: {e}");
    }
    if let Err(e) = cmd.spawn() {
        eprintln!("rustcord: update: could not start the new version: {e}");
    }
}

fn send(event: Event) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(event);
    }
}

fn publish(status: &Status) {
    let mut shared = state::STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if *shared != *status {
        *shared = status.clone();
        drop(shared);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint_after(Duration::from_millis(1));
        }
    }
}

fn ui_now() -> Ui {
    Ui { hidden: HIDDEN.load(Ordering::Relaxed), busy: BUSY.load(Ordering::Relaxed), interacted: INTERACTED.load(Ordering::Relaxed) }
}

/// The updater task: runs the machine's actions, feeds results back, sleeps on one timer.
struct Task {
    tx: UnboundedSender<Event>,
    src: feed::Source,
    install: platform::Install,
    dir: PathBuf,
    state_file: PathBuf,
    running: Version,
}

impl Task {
    async fn run(self, mut engine: Engine, plan: StartPlan, mut rx: UnboundedReceiver<Event>, after_update: bool) {
        let (install, dir, keep) = (self.install.clone(), self.dir.clone(), engine.staged_file());
        let cleanup = after_update || plan.succeeded;
        let probe = tokio::task::spawn_blocking(move || {
            store::prune(&dir, keep.as_deref());
            if cleanup {
                platform::cleanup_after_update(&install.exe);
            }
            #[cfg(windows)]
            let writable = install.key.is_none() || platform::writable(&install);
            #[cfg(not(windows))]
            let writable = true;
            (writable, platform::deb_tools())
        });
        let (writable, deb_tools) = probe.await.unwrap_or((true, true));
        if !writable && !plan.apply {
            engine.set_can(Kind::NoticeOnly);
        }
        engine.manual = !deb_tools;
        engine.ui = ui_now();
        let mut actions = engine.begin(&plan, Instant::now());
        loop {
            while !actions.is_empty() {
                let now = Instant::now();
                for action in std::mem::take(&mut actions) {
                    actions.extend(self.execute(action, &mut engine, now));
                }
            }
            publish(&engine.status);
            let wake = engine.next_wake(Instant::now());
            let sleep = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            let event = tokio::select! {
                e = rx.recv() => match e {
                    Some(e) => Some(e),
                    None => return,
                },
                () = sleep => None,
            };
            let now = Instant::now();
            if let Some(event) = event {
                actions = match event {
                    Event::Ui => engine.ui_changed(ui_now(), now),
                    Event::Button => engine.button(now),
                    Event::Install => engine.install(now),
                    Event::Dismiss => {
                        engine.dismiss(now);
                        Vec::new()
                    }
                    Event::CheckNow => engine.check_now(),
                    Event::SetAuto(on) => engine.set_auto(on, now),
                    Event::Checked(r) => engine.checked(r, now),
                    Event::Progress(done) => {
                        engine.progress(done);
                        Vec::new()
                    }
                    Event::Downloaded(r) => engine.downloaded(r, now),
                    Event::Applied(r) => engine.applied(r, now),
                };
            }
            actions.extend(engine.tick(now));
        }
    }

    /// Starts one action; results come back as events. Returns follow-up actions when the
    /// action failed on the spot.
    fn execute(&self, action: Action, engine: &mut Engine, now: Instant) -> Vec<Action> {
        match action {
            Action::Check => {
                let (tx, src, key) = (self.tx.clone(), self.src.clone(), self.install.key);
                tokio::spawn(async move {
                    let _ = tx.send(Event::Checked(feed::check(&src, key).await));
                });
            }
            Action::Download(offer) => {
                let (Some(key), Some(pkg)) = (self.install.key, offer.package) else { return engine.downloaded(Err(feed::DownloadError::Damaged), now) };
                let (tx, src, dest) = (self.tx.clone(), self.src.clone(), self.dir.join(key.file(&offer.version)));
                tokio::spawn(async move {
                    let result = feed::download(&src, &pkg, dest, |done| {
                        let _ = tx.send(Event::Progress(done));
                    })
                    .await;
                    let _ = tx.send(Event::Downloaded(result));
                });
            }
            Action::Apply(staged) => {
                // The loop guard first: without a saved `attempted`, a package that does not
                // install would be applied at every start.
                engine.saved.attempted = Some(staged.version.clone());
                if let Err(e) = engine.saved.save(&self.state_file) {
                    engine.saved.attempted = None;
                    let message = format!("Could not save the update: {}.", e.trim_end_matches('.'));
                    return engine.applied(Err(ApplyError::Failed { message, block: false }), now);
                }
                let (tx, install, dir, running) = (self.tx.clone(), self.install.clone(), self.dir.clone(), self.running.clone());
                tokio::task::spawn_blocking(move || {
                    let hidden = || HIDDEN.load(Ordering::Relaxed);
                    let result = platform::apply(&install, &dir, &staged, &running, &hidden).map(|relaunch| match relaunch {
                        platform::Relaunch::Started => true,
                        platform::Relaunch::Later(cmd) => {
                            *RELAUNCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(cmd);
                            false
                        }
                    });
                    let _ = tx.send(Event::Applied(result));
                });
            }
            Action::Save => {
                if let Err(e) = engine.saved.save(&self.state_file) {
                    eprintln!("rustcord: update: could not save {}: {e}", self.state_file.display());
                }
            }
            // In line (a few files): the Apply of the same batch writes state.tmp and the
            // Linux extract folder right after, and a prune on another thread would delete them.
            Action::Prune => store::prune(&self.dir, engine.staged_file().as_deref()),
            Action::Quit => {
                QUIT_FOR_UPDATE.store(true, Ordering::Relaxed);
                crate::notify::request_quit();
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn mode_table() {
        let feed = ("RUSTCORD_UPDATE_FEED", "http://127.0.0.1:8765/latest.json");
        let key = ("RUSTCORD_UPDATE_PUBKEY", "qfhI8y0rxvesm8m+knaaFmR1EsdXWzPB+o3F8g2/yeM=");
        let test = |list: &[(&str, &str)], debug| match mode(&vars(list), debug) {
            Mode::Test(t) => Some(t),
            _ => None,
        };
        // Test mode: release and debug builds, only with the test variables.
        let t = test(&[feed, key, ("RUSTCORD_UPDATE_FROM", "0.0.9"), ("RUSTCORD_HOME", "h"), ("RUSTCORD_SETUP_TEST", "1"), ("RUSTCORD_SETUP_PREFIX", "/p")], true).unwrap();
        assert_eq!((t.feed.as_str(), t.from, t.prefix), ("http://127.0.0.1:8765/latest.json", Some(Version::new(0, 0, 9)), Some(PathBuf::from("/p"))));
        assert!(t.key.is_some());
        let t = test(&[feed, ("RUSTCORD_SETUP_PREFIX", "/p")], false).unwrap();
        assert_eq!((t.key, t.from, t.prefix), (None, None, None), "prefix only with RUSTCORD_SETUP_TEST=1");
        let e2e = [feed, ("RUSTCORD_OFFLINE", "1"), ("RUSTCORD_TRACE_STARTUP", "t.txt"), ("RUSTCORD_SCREENSHOT", "s.png"), ("RUSTCORD_SCREENSHOT_FRAME", "60"), ("RUSTCORD_WINDOW_SIZE", "1180x760")];
        assert!(test(&e2e, false).is_some(), "end-to-end runs");
        // A bad feed, key or version, or any other hook: off.
        for list in [
            vec![("RUSTCORD_UPDATE_FEED", "https://github.com/r0sc/Rustcord/releases/latest/download/latest.json")],
            vec![("RUSTCORD_UPDATE_FEED", "http://127.0.0.1.evil.com/latest.json")],
            vec![feed, ("RUSTCORD_UPDATE_PUBKEY", "AAAA")],
            vec![feed, ("RUSTCORD_UPDATE_FROM", "x")],
            vec![feed, ("RUSTCORD_OPEN", "settings")],
            vec![feed, ("RUSTCORD_UPDATE_DEMO", "download")],
        ] {
            assert!(matches!(mode(&vars(&list), false), Mode::Off(why) if !why.is_empty()), "{list:?}");
        }
        // The key or version hooks without a feed, any other hook, debug builds: off.
        for list in [
            vec![key],
            vec![("RUSTCORD_UPDATE_FROM", "0.0.1")],
            vec![("RUSTCORD_SCREENSHOT", "x.png")],
            vec![("RUSTCORD_OFFLINE", "1")],
            vec![("RUSTCORD_TRACE_STARTUP", "t.txt")],
            vec![("RUSTCORD_HOME", "h"), ("RUSTCORD_OPEN", "settings")],
        ] {
            assert_eq!(mode(&vars(&list), false), Mode::Off(""), "{list:?}");
        }
        assert_eq!(mode(&[], true), Mode::Off(""));
        assert_eq!(mode(&vars(&[("RUSTCORD_HOME", "h")]), true), Mode::Off(""));
        // Portable data: a notice only. Nothing set: the full updater.
        assert_eq!(mode(&vars(&[("RUSTCORD_HOME", "h")]), false), Mode::Notice);
        assert_eq!(mode(&[], false), Mode::Full);
    }

    #[test]
    fn a_users_quit_never_relaunches() {
        // The restart waited for a call to end and the user picked Quit meanwhile: only the
        // updater's own quit starts the new version.
        *RELAUNCH.lock().unwrap() = Some(std::process::Command::new("rustcord-relaunch-test-missing"));
        relaunch();
        assert!(RELAUNCH.lock().unwrap().take().is_some(), "nothing was started");
    }

    #[test]
    fn prune_is_done_before_the_next_action() {
        // Save, Prune and Apply come in one batch; a prune still running on another thread
        // deleted the Apply's state.tmp (and the Linux extract folder) under it.
        let dir = std::env::temp_dir().join(format!("rc-update-task-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.part"), "x").unwrap();
        let running = Version::new(0, 1, 0);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let src = feed::Source::test(Url::parse("http://127.0.0.1:9/latest.json").unwrap(), None).unwrap();
        let install = platform::Install { can: Kind::Auto, key: Some(feed::Key::Windows), exe: PathBuf::new(), test_identity: false };
        let task = Task { tx, src, install, dir: dir.clone(), state_file: dir.join("state.toml"), running: running.clone() };
        let plan = StartPlan { phase: Phase::Idle, screen: false, apply: false, succeeded: false };
        let mut engine = Engine::new(&plan, store::Saved::default(), running, Kind::Auto, Some(feed::Key::Windows), dir.clone(), true, false, Instant::now());
        // No runtime here: a Prune handed to another thread would panic.
        for action in [Action::Save, Action::Prune] {
            assert!(task.execute(action, &mut engine, Instant::now()).is_empty());
        }
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["state.toml"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pub_fns_before_start_are_no_ops() {
        // No task: nothing is sent, nothing blocks.
        sync_ui(true, true, true);
        button_clicked();
        install();
        dismiss();
        check_now();
        set_auto(false);
        relaunch();
        assert!(!status().enabled);
    }
}
