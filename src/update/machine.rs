//! The updater's decisions (docs/UPDATER_DESIGN.md §8 with the design review's changes), free
//! of I/O: events in, `Action`s out, so every transition is unit tested. `super` runs the
//! actions and feeds the results back.

use super::feed::{CheckError, DownloadError, Key, Offer};
use super::state::{Kind, Phase, Status};
use super::store::{Saved, Staged};
use semver::Version;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Found within this long after start (and nothing pressed yet): the update screen comes up.
pub const START_WINDOW: Duration = Duration::from_secs(3);
/// The automatic start screen hands a slow download off to the background after this long.
pub const SCREEN_DOWNLOAD_MAX: Duration = Duration::from_secs(10);
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
pub const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);
/// An error on the update screen continues into the app after this long.
pub const ERROR_SCREEN: Duration = Duration::from_secs(5);
/// A silent install waits until the window has been in the tray this long without a break
/// (drafts and uploads live in memory only).
pub const HIDDEN_WAIT: Duration = Duration::from_secs(10 * 60);

pub const MSG_DOWNLOAD: &str = "Could not download the update. Check your connection; Rustcord tries again later.";
pub const MSG_SIGNATURE: &str = "The update's signature is not valid, so it was not installed.";
pub const MSG_DAMAGED: &str = "The downloaded update was damaged, so it was not installed.";
pub const MSG_CHECK: &str = "Could not check for updates. Check your connection.";
pub const MSG_NO_PASSWORD: &str = "The update was not installed: no password was given. Select Install to try again.";

/// What the window is doing (`update::sync_ui`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ui {
    pub hidden: bool,
    pub busy: bool,
    pub interacted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Check,
    /// Download the current offer's package.
    Download(Offer),
    /// Write `attempted`, re-hash the staged file, install it.
    Apply(Staged),
    /// Write state.toml.
    Save,
    /// Delete everything in the updater folder but state.toml and the staged file.
    Prune,
    /// The new version runs (or runs after the exit): quit this one.
    Quit,
}

/// How an install ended badly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// `block`: never install this version automatically again (ignored for packages, which
    /// are only installed on a click).
    Failed { message: String, block: bool },
    /// pkexec 127: not authorized / no polkit agent: the package waits for the package manager.
    Manual,
    /// The staged file no longer has the signed size and hash.
    Damaged,
}

/// What the start found (§8.2), applied before the first frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartPlan {
    pub phase: Phase,
    pub screen: bool,
    /// Install the staged version now, instead of checking.
    pub apply: bool,
    /// The previous run's update installed: clean up its leftovers.
    pub succeeded: bool,
}

/// The loop guard and the staged package at start (§8.2 steps 3 and 4). `staged_ok`: the staged
/// file exists with the right size.
#[allow(clippy::too_many_arguments)]
pub fn plan_start(saved: &mut Saved, running: &Version, can: Kind, auto: bool, hidden: bool, macos: bool, staged_ok: bool, setup_error: Option<&str>) -> StartPlan {
    let mut plan = StartPlan { phase: Phase::Idle, screen: false, apply: false, succeeded: false };
    if let Some(a) = saved.attempted.take() {
        saved.staged = None;
        if *running >= a {
            plan.succeeded = true;
            if saved.blocked.as_ref() == Some(&a) {
                saved.blocked = None;
            }
        } else {
            let why = setup_error.map(|e| format!(": {}", e.trim_end_matches('.'))).unwrap_or_default();
            plan.phase = Phase::Failed { message: format!("Rustcord {a} did not install{why}. Version {running} keeps running.") };
            plan.screen = !hidden;
            saved.blocked = Some(a);
        }
    }
    match saved.staged.clone() {
        Some(s) if s.version > *running && can != Kind::NoticeOnly && staged_ok => {
            // Nothing is in memory yet, so a hidden start installs too, except on macOS, where
            // the new bundle may ask for keychain access: that waits for a visible start.
            if saved.blocked.as_ref() != Some(&s.version) && auto && can == Kind::Auto && !(macos && hidden) {
                plan = StartPlan { phase: Phase::Installing { version: s.version.to_string() }, screen: !hidden, apply: true, ..plan };
            } else {
                plan.phase = Phase::Ready { version: s.version.to_string() };
            }
        }
        Some(_) => saved.staged = None,
        None => {}
    }
    plan
}

/// The running updater (§8.3, §8.4).
pub struct Engine {
    pub status: Status,
    pub saved: Saved,
    pub ui: Ui,
    pub auto: bool,
    running: Version,
    /// What this copy can install (`Status::kind` is the kind of the current offer).
    can: Kind,
    key: Option<Key>,
    dir: PathBuf,
    /// Linux .deb without /usr/bin/pkexec or /usr/bin/apt-get.
    pub manual: bool,
    macos: bool,
    started: Instant,
    hidden_since: Option<Instant>,
    /// The newest verified offer.
    offer: Option<Offer>,
    /// The user asked for this update (title-bar button, Install): install when it is ready.
    user_wants: bool,
    checking: bool,
    user_check: bool,
    /// The phase a silent check replaces with `Checking`, restored when it finds nothing.
    before_check: Phase,
    /// When the screen's current phase began (10 s hand-off, 5 s error).
    screen_at: Instant,
    next_check: Instant,
    /// Background download failures in a row (retry backoff).
    failures: u32,
    /// Restarting: quit once not busy (or right away when the new copy already runs).
    quit_pending: Option<bool>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(plan: &StartPlan, saved: Saved, running: Version, can: Kind, key: Option<Key>, dir: PathBuf, auto: bool, hidden: bool, now: Instant) -> Self {
        Self {
            status: Status { phase: plan.phase.clone(), kind: can, screen: plan.screen, enabled: true, note: None },
            saved,
            ui: Ui { hidden, ..Ui::default() },
            auto,
            running,
            can,
            key,
            dir,
            manual: false,
            macos: cfg!(target_os = "macos"),
            started: now,
            hidden_since: hidden.then_some(now),
            offer: None,
            user_wants: false,
            checking: false,
            user_check: false,
            before_check: Phase::Idle,
            screen_at: now,
            next_check: now,
            failures: 0,
            quit_pending: None,
        }
    }

    /// The task's first actions: the staged install of the start plan, or a check.
    pub fn begin(&mut self, plan: &StartPlan, now: Instant) -> Vec<Action> {
        match self.saved.staged.clone() {
            Some(s) if plan.apply => {
                self.screen_at = now;
                vec![Action::Apply(s)]
            }
            // A start error stays up first; the check follows when it is gone (`tick`).
            _ => self.tick(now),
        }
    }

    /// This copy turned out unable to install (Windows: its folder is not writable).
    pub fn set_can(&mut self, can: Kind) {
        self.can = can;
        self.status.kind = can;
    }

    /// The staged package's path (`Manual`, `Prune`).
    pub fn staged_file(&self) -> Option<String> {
        Some(self.key?.file(&self.saved.staged.as_ref()?.version))
    }

    fn set_phase(&mut self, phase: Phase, now: Instant) {
        if std::mem::discriminant(&phase) != std::mem::discriminant(&self.status.phase) {
            self.screen_at = now;
        }
        self.status.phase = phase;
        self.status.note = None;
    }

    fn can_check(&self) -> bool {
        let busy_phase = matches!(self.status.phase, Phase::Downloading { .. } | Phase::Installing { .. } | Phase::Restarting { .. });
        !self.checking && !busy_phase && !self.status.screen
    }

    /// When `tick` has something to do next; None: only events wake the task.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let mut at = Vec::new();
        if self.can_check() {
            at.push(self.next_check);
        }
        if self.status.screen {
            match self.status.phase {
                Phase::Downloading { .. } if !self.user_wants => at.push(self.screen_at + SCREEN_DOWNLOAD_MAX),
                Phase::Failed { .. } => at.push(self.screen_at + ERROR_SCREEN),
                _ => {}
            }
        }
        if let (Phase::Ready { .. }, Some(h)) = (&self.status.phase, self.hidden_since) {
            at.push(h + HIDDEN_WAIT);
        }
        // Deadlines that passed were handled by the `tick` before this, or wait for an event.
        at.into_iter().filter(|t| *t > now).min()
    }

    /// Timers: the 10 s screen hand-off, the 5 s error screen, the silent install after
    /// `HIDDEN_WAIT`, a pending quit, the next check. Runs after every event too.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let since = now.saturating_duration_since(self.screen_at);
        if self.status.screen {
            match self.status.phase {
                Phase::Downloading { .. } if !self.user_wants && since >= SCREEN_DOWNLOAD_MAX => self.status.screen = false,
                Phase::Failed { .. } if since >= ERROR_SCREEN => self.dismiss(now),
                _ => {}
            }
        }
        let mut out = self.maybe_install(now);
        out.extend(self.maybe_quit());
        if out.is_empty() && self.can_check() && now >= self.next_check {
            out.extend(self.start_check(false));
        }
        out
    }

    fn start_check(&mut self, user: bool) -> Vec<Action> {
        self.checking = true;
        self.user_check = user;
        self.before_check = self.status.phase.clone();
        if matches!(self.status.phase, Phase::Idle | Phase::UpToDate | Phase::Failed { .. }) {
            self.status.phase = Phase::Checking;
        }
        vec![Action::Check]
    }

    /// Settings "Check for updates".
    pub fn check_now(&mut self) -> Vec<Action> {
        let busy_phase = matches!(self.status.phase, Phase::Downloading { .. } | Phase::Installing { .. } | Phase::Restarting { .. });
        if self.checking || busy_phase { Vec::new() } else { self.start_check(true) }
    }

    pub fn checked(&mut self, result: Result<Offer, CheckError>, now: Instant) -> Vec<Action> {
        self.checking = false;
        let user = std::mem::take(&mut self.user_check);
        if self.status.phase == Phase::Checking {
            self.status.phase = std::mem::replace(&mut self.before_check, Phase::Idle);
        }
        // A click or a silent install started meanwhile: that one goes on.
        if matches!(self.status.phase, Phase::Downloading { .. } | Phase::Installing { .. } | Phase::Restarting { .. }) {
            self.next_check = now + CHECK_EVERY;
            return Vec::new();
        }
        let offer = match result {
            Ok(offer) => offer,
            Err(e) => {
                eprintln!("rustcord: update check: {e:?}");
                let no_feed = matches!(e, CheckError::NoFeed(_));
                self.next_check = now + if no_feed { CHECK_EVERY } else { RETRY_AFTER };
                let pending = matches!(self.status.phase, Phase::Available { .. } | Phase::Ready { .. } | Phase::Manual { .. });
                if user && !pending {
                    let phase = match e {
                        // Nothing published yet: nothing newer either.
                        CheckError::NoFeed(_) => Phase::UpToDate,
                        CheckError::Signature => Phase::Failed { message: MSG_SIGNATURE.into() },
                        _ => Phase::Failed { message: MSG_CHECK.into() },
                    };
                    self.set_phase(phase, now);
                }
                return Vec::new();
            }
        };
        self.next_check = now + CHECK_EVERY;
        if !user && self.saved.bad_manifest == Some(offer.manifest) {
            return Vec::new();
        }
        let v = offer.version.clone();
        if v <= self.running {
            self.offer = None;
            self.status.kind = self.can;
            self.set_phase(Phase::UpToDate, now);
            // A staged package the feed no longer offers (pulled release) goes too.
            return if self.saved.staged.take().is_some() { vec![Action::Save, Action::Prune] } else { Vec::new() };
        }
        let notice = offer.package.is_none() || self.can == Kind::NoticeOnly;
        self.offer = Some(offer);
        self.status.kind = if notice { Kind::NoticeOnly } else { self.can };
        if self.saved.staged.as_ref().is_some_and(|s| s.version == v) {
            if !matches!(self.status.phase, Phase::Manual { .. }) {
                self.set_phase(Phase::Ready { version: v.to_string() }, now);
            }
            return self.maybe_install(now);
        }
        if notice || self.saved.blocked.as_ref() == Some(&v) || !self.auto {
            self.set_phase(Phase::Available { version: v.to_string() }, now);
            return Vec::new();
        }
        let early = now.saturating_duration_since(self.started) < START_WINDOW;
        let screen = self.can == Kind::Auto && !self.ui.hidden && early && !self.ui.interacted;
        // The automatic screen is not a user wish: it hands off after 10 s, a dismiss cancels it.
        self.start_download(screen, false, now)
    }

    fn start_download(&mut self, screen: bool, user_wants: bool, now: Instant) -> Vec<Action> {
        let Some(offer) = self.offer.clone() else { return Vec::new() };
        let Some(pkg) = &offer.package else { return Vec::new() };
        self.set_phase(Phase::Downloading { version: offer.version.to_string(), done: 0, total: pkg.size }, now);
        self.screen_at = now;
        self.status.screen = screen;
        self.user_wants = user_wants;
        vec![Action::Download(offer)]
    }

    pub fn progress(&mut self, bytes: u64) {
        if let Phase::Downloading { done, .. } = &mut self.status.phase {
            *done = bytes;
        }
    }

    pub fn downloaded(&mut self, result: Result<(), DownloadError>, now: Instant) -> Vec<Action> {
        let (Phase::Downloading { version, .. }, Some(offer)) = (self.status.phase.clone(), self.offer.clone()) else { return Vec::new() };
        let Some(pkg) = offer.package.clone() else { return Vec::new() };
        if let Err(e) = result {
            eprintln!("rustcord: update download {version}: {e:?}");
            let mut out = Vec::new();
            let message = match e {
                DownloadError::Damaged => {
                    // The same latest.json is not tried again by itself (a click still may).
                    self.saved.bad_manifest = Some(offer.manifest);
                    out.push(Action::Save);
                    MSG_DAMAGED.to_owned()
                }
                DownloadError::Network(_) => MSG_DOWNLOAD.to_owned(),
                DownloadError::Disk(e) => format!("Could not save the update: {}.", e.trim_end_matches('.')),
            };
            self.failures += 1;
            self.next_check = now + [RETRY_AFTER, Duration::from_secs(3600), CHECK_EVERY][(self.failures as usize - 1).min(2)];
            self.user_wants = false;
            if self.status.screen {
                self.set_phase(Phase::Failed { message }, now);
            } else {
                // In the background the button stays; the settings show why.
                self.set_phase(Phase::Available { version }, now);
                self.status.note = Some(message);
            }
            return out;
        }
        self.failures = 0;
        self.saved.staged = Some(Staged { version: offer.version.clone(), sha256: pkg.sha256, size: pkg.size });
        let mut out = vec![Action::Save, Action::Prune];
        if self.can == Kind::Package && self.manual {
            let path = self.dir.join(self.staged_file().unwrap_or_default());
            self.user_wants = false;
            self.set_phase(Phase::Manual { version, path }, now);
            return out;
        }
        self.set_phase(Phase::Ready { version }, now);
        if self.user_wants || self.status.screen {
            if self.ui.busy {
                // Never cover a call: the button waits ("Installs after your call ends").
                self.status.screen = false;
            } else if self.can == Kind::Package {
                // The screen's Install button asks for the password.
                self.status.screen = true;
                self.user_wants = false;
            } else {
                self.user_wants = true;
            }
        }
        out.extend(self.maybe_install(now));
        out
    }

    /// Ready -> Installing when the user asked (and is not in a call) or for a silent install.
    fn maybe_install(&mut self, now: Instant) -> Vec<Action> {
        let Phase::Ready { version } = &self.status.phase else { return Vec::new() };
        let blocked = self.saved.blocked.as_ref().is_some_and(|b| b.to_string() == *version);
        if self.can != Kind::Auto || self.ui.busy {
            return Vec::new();
        }
        if self.user_wants {
            return self.begin_apply(!self.ui.hidden, now);
        }
        let long_hidden = self.ui.hidden && self.hidden_since.is_some_and(|h| now >= h + HIDDEN_WAIT);
        if !self.status.screen && long_hidden && self.auto && !blocked && !self.macos {
            return self.begin_apply(false, now);
        }
        Vec::new()
    }

    /// Enter Installing: the one place, only when not busy.
    fn begin_apply(&mut self, screen: bool, now: Instant) -> Vec<Action> {
        let Some(staged) = self.saved.staged.clone() else { return Vec::new() };
        if self.ui.busy {
            return Vec::new();
        }
        self.user_wants = false;
        self.set_phase(Phase::Installing { version: staged.version.to_string() }, now);
        self.status.screen = screen;
        vec![Action::Apply(staged)]
    }

    /// `started`: the new copy already runs and waits 30 s for this one's lock (macOS `open`), so
    /// this one quits even in a call; macOS installs only on a click or at a start (§18).
    pub fn applied(&mut self, result: Result<bool, ApplyError>, now: Instant) -> Vec<Action> {
        let Phase::Installing { version } = self.status.phase.clone() else { return Vec::new() };
        match result {
            Ok(started) => {
                // Visible: "Restarting" on the screen, unless that would cover a call.
                self.status.screen = !self.ui.hidden && !self.ui.busy;
                self.set_phase(Phase::Restarting { version }, now);
                self.quit_pending = Some(started);
                self.maybe_quit()
            }
            Err(e) => {
                eprintln!("rustcord: update install {version}: {e:?}");
                self.saved.attempted = None;
                let mut out = vec![Action::Save];
                match e {
                    ApplyError::Manual => {
                        let path = self.dir.join(self.staged_file().unwrap_or_default());
                        self.manual = true;
                        self.set_phase(Phase::Manual { version, path }, now);
                    }
                    ApplyError::Damaged => {
                        self.saved.staged = None;
                        out.push(Action::Prune);
                        self.set_phase(Phase::Failed { message: MSG_DAMAGED.into() }, now);
                    }
                    ApplyError::Failed { message, block } => {
                        if block && self.can != Kind::Package {
                            self.saved.blocked = self.saved.staged.as_ref().map(|s| s.version.clone());
                        }
                        self.set_phase(Phase::Failed { message }, now);
                    }
                }
                out
            }
        }
    }

    fn maybe_quit(&mut self) -> Vec<Action> {
        match self.quit_pending {
            Some(started) if started || !self.ui.busy => {
                self.quit_pending = None;
                vec![Action::Quit]
            }
            _ => Vec::new(),
        }
    }

    /// The title-bar button (NoticeOnly offers open the release page in `super`).
    pub fn button(&mut self, now: Instant) -> Vec<Action> {
        match (&self.status.phase, self.status.kind) {
            (Phase::Available { .. }, Kind::Auto | Kind::Package) => {
                // In a call it downloads in the background and installs after the call.
                let screen = !self.ui.busy;
                self.start_download(screen, true, now)
            }
            (Phase::Ready { .. }, Kind::Auto) => self.begin_apply(true, now),
            (Phase::Ready { .. }, Kind::Package) | (Phase::Manual { .. }, _) => {
                self.status.screen = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The screen's Install (Ready, Package).
    pub fn install(&mut self, now: Instant) -> Vec<Action> {
        if matches!(self.status.phase, Phase::Ready { .. }) && self.can == Kind::Package { self.begin_apply(true, now) } else { Vec::new() }
    }

    /// "Not now", "Continue", Esc, or the error screen's timeout.
    pub fn dismiss(&mut self, now: Instant) {
        if matches!(self.status.phase, Phase::Installing { .. } | Phase::Restarting { .. }) {
            return;
        }
        if let Phase::Failed { .. } = self.status.phase {
            let phase = match (&self.saved.staged, &self.offer) {
                (Some(s), _) if s.version > self.running => Phase::Ready { version: s.version.to_string() },
                (_, Some(o)) => Phase::Available { version: o.version.to_string() },
                _ => Phase::Idle,
            };
            self.set_phase(phase, now);
        }
        self.status.screen = false;
        self.user_wants = false;
    }

    pub fn set_auto(&mut self, on: bool, now: Instant) -> Vec<Action> {
        self.auto = on;
        self.maybe_install(now)
    }

    pub fn ui_changed(&mut self, ui: Ui, now: Instant) -> Vec<Action> {
        self.ui = ui;
        self.hidden_since = if ui.hidden { self.hidden_since.or(Some(now)) } else { None };
        let mut out = self.maybe_install(now);
        out.extend(self.maybe_quit());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::feed::Package;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn staged(s: &str) -> Staged {
        Staged { version: v(s), sha256: [1; 32], size: 10 }
    }

    fn offer(s: &str, package: bool) -> Offer {
        let pkg = Package { url: format!("https://x/{s}"), sha256: [1; 32], size: 10 };
        Offer { version: v(s), package: package.then_some(pkg), manifest: [9; 32] }
    }

    fn engine(can: Kind) -> (Engine, Instant) {
        let now = Instant::now();
        let plan = StartPlan { phase: Phase::Idle, screen: false, apply: false, succeeded: false };
        let mut e = Engine::new(&plan, Saved::default(), v("0.1.0"), can, Some(Key::Windows), PathBuf::from("u"), true, false, now);
        e.macos = false;
        // Checks only where a test asks for one.
        e.next_check = now + CHECK_EVERY * 100;
        (e, now)
    }

    fn ready(version: &str) -> Phase {
        Phase::Ready { version: version.into() }
    }

    fn installing(version: &str) -> Phase {
        Phase::Installing { version: version.into() }
    }

    fn after(now: Instant, secs: u64) -> Instant {
        now + Duration::from_secs(secs)
    }

    #[test]
    fn loop_guard_at_start() {
        let run = v("0.1.0");
        // The update installed: attempted, its block and its staged file go.
        let mut s = Saved { attempted: Some(v("0.1.0")), blocked: Some(v("0.1.0")), staged: Some(staged("0.1.0")), ..Saved::default() };
        let p = plan_start(&mut s, &run, Kind::Auto, true, false, false, true, None);
        assert!(p.succeeded && !p.apply && p.phase == Phase::Idle);
        assert_eq!(s, Saved::default());
        // It did not: blocked, shown once (on the screen when visible), never tried again.
        let mut s = Saved { attempted: Some(v("0.1.1")), staged: Some(staged("0.1.1")), ..Saved::default() };
        let p = plan_start(&mut s, &run, Kind::Auto, true, false, false, true, Some("Access is denied."));
        assert_eq!(p.phase, Phase::Failed { message: "Rustcord 0.1.1 did not install: Access is denied. Version 0.1.0 keeps running.".into() });
        assert!(p.screen && !p.apply && !p.succeeded);
        assert_eq!((s.attempted, s.blocked, s.staged), (None, Some(v("0.1.1")), None));
        let mut s = Saved { attempted: Some(v("0.1.1")), ..Saved::default() };
        let p = plan_start(&mut s, &run, Kind::Auto, true, true, false, false, None);
        assert_eq!(p.phase, Phase::Failed { message: "Rustcord 0.1.1 did not install. Version 0.1.0 keeps running.".into() });
        assert!(!p.screen, "hidden");
    }

    #[test]
    fn staged_at_start() {
        let run = v("0.1.0");
        let plan = |s: &mut Saved, can, auto, hidden, macos, ok| plan_start(s, &run, can, auto, hidden, macos, ok, None);
        let fresh = || Saved { staged: Some(staged("0.1.1")), ..Saved::default() };
        // Installs before the main UI, also hidden (nothing in memory yet).
        let p = plan(&mut fresh(), Kind::Auto, true, false, false, true);
        assert_eq!((p.phase, p.screen, p.apply), (installing("0.1.1"), true, true));
        let p = plan(&mut fresh(), Kind::Auto, true, true, false, true);
        assert_eq!((p.phase, p.screen, p.apply), (installing("0.1.1"), false, true));
        // Not automatically: auto off, blocked, a package, macOS hidden.
        for (can, auto, hidden, macos, blocked) in [(Kind::Auto, false, false, false, false), (Kind::Auto, true, false, false, true), (Kind::Package, true, false, false, false), (Kind::Auto, true, true, true, false)] {
            let mut s = fresh();
            if blocked {
                s.blocked = Some(v("0.1.1"));
            }
            let p = plan(&mut s, can, auto, hidden, macos, true);
            assert_eq!((p.phase, p.apply), (ready("0.1.1"), false), "{can:?} {auto} {hidden} {macos} {blocked}");
            assert!(s.staged.is_some());
        }
        // Stale, missing, or a copy that can't install: forgotten.
        let mut s = Saved { staged: Some(staged("0.1.0")), ..Saved::default() };
        assert_eq!(plan(&mut s, Kind::Auto, true, false, false, true).phase, Phase::Idle);
        assert!(s.staged.is_none());
        let mut s = fresh();
        assert_eq!(plan(&mut s, Kind::Auto, true, false, false, false).phase, Phase::Idle);
        assert!(s.staged.is_none());
        let mut s = fresh();
        assert_eq!(plan(&mut s, Kind::NoticeOnly, true, false, false, true).phase, Phase::Idle);
        assert!(s.staged.is_none());
    }

    #[test]
    fn check_results() {
        let (mut e, now) = engine(Kind::Auto);
        e.next_check = now;
        assert_eq!(e.begin(&StartPlan { phase: Phase::Idle, screen: false, apply: false, succeeded: false }, now), [Action::Check]);
        assert_eq!(e.status.phase, Phase::Checking);
        assert!(e.tick(now).is_empty(), "one check at a time");
        // Not newer.
        assert!(e.checked(Ok(offer("0.1.0", true)), now).is_empty());
        assert_eq!(e.status.phase, Phase::UpToDate);
        assert_eq!(e.next_wake(now), Some(now + CHECK_EVERY));
        // A failed silent check keeps the phase and retries in 15 min; 4xx in 6 h.
        e.tick(now + CHECK_EVERY);
        assert!(e.checked(Err(CheckError::Network("x".into())), now).is_empty());
        assert_eq!((e.status.phase.clone(), e.next_check), (Phase::UpToDate, now + RETRY_AFTER));
        e.tick(now + RETRY_AFTER);
        e.checked(Err(CheckError::NoFeed(404)), now);
        assert_eq!((e.status.phase.clone(), e.next_check), (Phase::UpToDate, now + CHECK_EVERY));
        // From "Check for updates": the error shows (in the settings, not on the screen).
        e.check_now();
        e.checked(Err(CheckError::Signature), now);
        assert_eq!(e.status.phase, Phase::Failed { message: MSG_SIGNATURE.into() });
        assert!(!e.status.screen);
        e.check_now();
        e.checked(Err(CheckError::Network("x".into())), now);
        assert_eq!(e.status.phase, Phase::Failed { message: MSG_CHECK.into() });
        e.check_now();
        e.checked(Err(CheckError::NoFeed(404)), now);
        assert_eq!(e.status.phase, Phase::UpToDate);
        // No package for this copy: a notice.
        e.check_now();
        assert!(e.checked(Ok(offer("0.1.1", false)), now).is_empty());
        assert_eq!((e.status.phase.clone(), e.status.kind), (Phase::Available { version: "0.1.1".into() }, Kind::NoticeOnly));
        // Auto off: offered, downloads on click.
        let (mut e, now) = engine(Kind::Auto);
        e.auto = false;
        e.check_now();
        assert!(e.checked(Ok(offer("0.1.1", true)), now).is_empty());
        assert_eq!((e.status.phase.clone(), e.status.kind), (Phase::Available { version: "0.1.1".into() }, Kind::Auto));
        // Blocked: offered too.
        let (mut e, now) = engine(Kind::Auto);
        e.saved.blocked = Some(v("0.1.1"));
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.status.phase, Phase::Available { version: "0.1.1".into() });
    }

    #[test]
    fn download_screen_only_right_after_start() {
        let (mut e, now) = engine(Kind::Auto);
        e.check_now();
        assert_eq!(e.checked(Ok(offer("0.1.1", true)), after(now, 2)), [Action::Download(offer("0.1.1", true))]);
        assert_eq!(e.status.phase, Phase::Downloading { version: "0.1.1".into(), done: 0, total: 10 });
        assert!(e.status.screen);
        // Too slow: the screen hands off after 10 s.
        assert_eq!(e.next_wake(after(now, 2)), Some(after(now, 12)));
        e.tick(after(now, 12));
        assert!(!e.status.screen);
        // Late, hidden, interacted, or a package: in the background.
        for (secs, hidden, interacted, can) in [(4, false, false, Kind::Auto), (1, true, false, Kind::Auto), (1, false, true, Kind::Auto), (1, false, false, Kind::Package)] {
            let (mut e, now) = engine(can);
            e.ui = Ui { hidden, busy: false, interacted };
            e.check_now();
            assert_eq!(e.checked(Ok(offer("0.1.1", true)), after(now, secs)).len(), 1);
            assert!(!e.status.screen, "{secs} {hidden} {interacted} {can:?}");
        }
    }

    #[test]
    fn download_done() {
        // The start screen installs right away.
        let (mut e, now) = engine(Kind::Auto);
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        e.progress(5);
        assert_eq!(e.status.phase, Phase::Downloading { version: "0.1.1".into(), done: 5, total: 10 });
        assert_eq!(e.downloaded(Ok(()), now), [Action::Save, Action::Prune, Action::Apply(staged("0.1.1"))]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (installing("0.1.1"), true));
        // In the background: Ready (the title-bar button).
        let (mut e, now) = engine(Kind::Auto);
        e.ui.interacted = true;
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.downloaded(Ok(()), now), [Action::Save, Action::Prune]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (ready("0.1.1"), false));
        assert_eq!(e.staged_file().as_deref(), Some("Rustcord-Setup-0.1.1.exe"));
        // The next check finds it staged: stays Ready.
        e.check_now();
        assert!(e.checked(Ok(offer("0.1.1", true)), now).is_empty());
        assert_eq!(e.status.phase, ready("0.1.1"));
        // A newer version replaces it.
        e.check_now();
        assert_eq!(e.checked(Ok(offer("0.1.2", true)), now).len(), 1);
        assert!(matches!(e.status.phase, Phase::Downloading { .. }));
        // A .deb without pkexec: Manual with the path.
        let (mut e, now) = engine(Kind::Package);
        e.key = Some(Key::LinuxDeb);
        e.manual = true;
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        e.downloaded(Ok(()), now);
        assert_eq!(e.status.phase, Phase::Manual { version: "0.1.1".into(), path: PathBuf::from("u").join("rustcord_0.1.1_amd64.deb") });
    }

    #[test]
    fn download_errors() {
        // In the background: the button stays, retries back off 15 min, 1 h, 6 h.
        let (mut e, now) = engine(Kind::Auto);
        e.ui.interacted = true;
        for wait in [RETRY_AFTER, Duration::from_secs(3600), CHECK_EVERY, CHECK_EVERY] {
            e.check_now();
            assert_eq!(e.checked(Ok(offer("0.1.1", true)), now).len(), 1);
            assert!(e.downloaded(Err(DownloadError::Network("reset".into())), now).is_empty());
            assert_eq!((e.status.phase.clone(), e.next_check), (Phase::Available { version: "0.1.1".into() }, now + wait));
            assert_eq!(e.status.note.as_deref(), Some(MSG_DOWNLOAD), "the settings say why");
        }
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.status.note, None, "a new download clears it");
        e.downloaded(Ok(()), now);
        assert_eq!(e.failures, 0);
        // On the screen: the error, then Available after 5 s.
        let (mut e, now) = engine(Kind::Auto);
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        e.downloaded(Err(DownloadError::Disk("No space left on device.".into())), now);
        assert_eq!(e.status.phase, Phase::Failed { message: "Could not save the update: No space left on device.".into() });
        assert!(e.status.screen);
        assert_eq!(e.next_wake(now), Some(after(now, 5)));
        e.tick(after(now, 5));
        assert_eq!((e.status.phase.clone(), e.status.screen), (Phase::Available { version: "0.1.1".into() }, false));
        // Damaged: that manifest is remembered and silently skipped; a user check retries.
        let (mut e, now) = engine(Kind::Auto);
        e.ui.interacted = true;
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.downloaded(Err(DownloadError::Damaged), now), [Action::Save]);
        assert_eq!(e.saved.bad_manifest, Some([9; 32]));
        e.tick(now + CHECK_EVERY);
        assert!(e.checked(Ok(offer("0.1.1", true)), now).is_empty());
        assert_eq!(e.status.phase, Phase::Available { version: "0.1.1".into() });
        e.check_now();
        assert_eq!(e.checked(Ok(offer("0.1.1", true)), now).len(), 1, "check_now ignores bad_manifest once");
    }

    #[test]
    fn never_covers_a_call() {
        // The button in a call: background download, Ready without a screen, install after.
        let (mut e, now) = engine(Kind::Auto);
        e.auto = false;
        e.ui = Ui { hidden: false, busy: true, interacted: true };
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.button(now).len(), 1);
        assert!(!e.status.screen);
        assert_eq!(e.downloaded(Ok(()), now), [Action::Save, Action::Prune]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (ready("0.1.1"), false));
        assert!(e.button(now).is_empty(), "Ready + busy: disabled");
        assert_eq!(e.ui_changed(Ui { busy: false, ..e.ui }, now), [Action::Apply(staged("0.1.1"))]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (installing("0.1.1"), true));
        // The start screen's download finishing during a call: Ready, no screen.
        let (mut e, now) = engine(Kind::Auto);
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        e.ui.busy = true;
        e.downloaded(Ok(()), now);
        assert_eq!((e.status.phase.clone(), e.status.screen), (ready("0.1.1"), false));
        // Install (package) in a call: stays Ready.
        let (mut e, now) = engine(Kind::Package);
        e.saved.staged = Some(staged("0.1.1"));
        e.status.phase = ready("0.1.1");
        e.ui.busy = true;
        assert!(e.install(now).is_empty());
        assert_eq!(e.status.phase, ready("0.1.1"));
        e.ui.busy = false;
        assert_eq!(e.install(now), [Action::Apply(staged("0.1.1"))]);
        // Restarting while busy (a call joined during a silent install): Windows and Linux wait,
        // a macOS copy that already started does not.
        let (mut e, now) = engine(Kind::Auto);
        e.status.phase = installing("0.1.1");
        e.ui.busy = true;
        assert!(e.applied(Ok(false), now).is_empty());
        assert!(!e.status.screen);
        assert_eq!(e.ui_changed(Ui::default(), now), [Action::Quit]);
        e.status.phase = installing("0.1.1");
        e.ui.busy = true;
        assert_eq!(e.applied(Ok(true), now), [Action::Quit]);
    }

    #[test]
    fn silent_install_after_ten_minutes_in_the_tray() {
        let (mut e, now) = engine(Kind::Auto);
        e.saved.staged = Some(staged("0.1.1"));
        e.status.phase = ready("0.1.1");
        assert!(e.ui_changed(Ui { hidden: true, ..Ui::default() }, now).is_empty());
        assert_eq!(e.next_wake(now), Some(now + HIDDEN_WAIT));
        assert!(e.tick(now + HIDDEN_WAIT - Duration::from_secs(1)).is_empty());
        // Shown in between: the wait starts over.
        e.ui_changed(Ui::default(), after(now, 60));
        e.ui_changed(Ui { hidden: true, ..Ui::default() }, after(now, 120));
        assert!(e.tick(now + HIDDEN_WAIT).is_empty());
        assert_eq!(e.tick(after(now, 120) + HIDDEN_WAIT), [Action::Apply(staged("0.1.1"))]);
        assert!(!e.status.screen);
        // Never: busy, auto off, blocked, a package, macOS.
        for case in 0..5 {
            let (mut e, now) = engine(if case == 3 { Kind::Package } else { Kind::Auto });
            e.saved.staged = Some(staged("0.1.1"));
            e.status.phase = ready("0.1.1");
            e.auto = case != 1;
            e.macos = case == 4;
            if case == 2 {
                e.saved.blocked = Some(v("0.1.1"));
            }
            e.ui_changed(Ui { hidden: true, busy: case == 0, interacted: false }, now);
            assert!(e.tick(now + HIDDEN_WAIT).is_empty(), "case {case}");
        }
    }

    #[test]
    fn buttons() {
        // Available: download with the screen; dismiss hides it.
        let (mut e, now) = engine(Kind::Auto);
        e.auto = false;
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.button(now).len(), 1);
        assert!(e.status.screen);
        assert_eq!(e.next_wake(now), None, "a clicked download keeps its screen");
        e.dismiss(now);
        assert!(!e.status.screen && !e.user_wants);
        assert_eq!(e.downloaded(Ok(()), now), [Action::Save, Action::Prune]);
        assert_eq!(e.status.phase, ready("0.1.1"));
        // Ready: installs with the screen.
        assert_eq!(e.button(now), [Action::Apply(staged("0.1.1"))]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (installing("0.1.1"), true));
        e.dismiss(now);
        assert!(e.status.screen, "Installing can't be dismissed");
        // Ready (package): the screen with Install; Manual: the screen with the path.
        let (mut e, now) = engine(Kind::Package);
        e.status.phase = ready("0.1.1");
        assert!(e.button(now).is_empty());
        assert!(e.status.screen);
        e.status = Status { phase: Phase::Manual { version: "0.1.1".into(), path: "p".into() }, kind: Kind::Package, screen: false, enabled: true, note: None };
        e.button(now);
        assert!(e.status.screen);
        // NoticeOnly: nothing here (super opens the release page).
        let (mut e, now) = engine(Kind::NoticeOnly);
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        assert_eq!(e.status.kind, Kind::NoticeOnly);
        assert!(e.button(now).is_empty() && !e.status.screen);
    }

    #[test]
    fn apply_results() {
        let start = |can| {
            let (mut e, now) = engine(can);
            e.saved.staged = Some(staged("0.1.1"));
            e.saved.attempted = Some(v("0.1.1"));
            e.status.phase = installing("0.1.1");
            e.status.screen = true;
            (e, now)
        };
        let (mut e, now) = start(Kind::Auto);
        assert_eq!(e.applied(Ok(true), now), [Action::Quit]);
        assert_eq!((e.status.phase.clone(), e.status.screen), (Phase::Restarting { version: "0.1.1".into() }, true));
        // A failed install: blocked, the error on the screen, then Ready (a click still may).
        let (mut e, now) = start(Kind::Auto);
        assert_eq!(e.applied(Err(ApplyError::Failed { message: "Could not install the update: x.".into(), block: true }), now), [Action::Save]);
        assert_eq!((e.saved.attempted.clone(), e.saved.blocked.clone()), (None, Some(v("0.1.1"))));
        e.tick(after(now, 5));
        assert_eq!((e.status.phase.clone(), e.status.screen), (ready("0.1.1"), false));
        assert!(e.ui_changed(Ui { hidden: true, ..Ui::default() }, now).is_empty());
        assert!(e.tick(now + HIDDEN_WAIT).is_empty(), "blocked: no silent retry");
        // A package never blocks; no password: back to Ready; 127: Manual.
        let (mut e, now) = start(Kind::Package);
        e.key = Some(Key::LinuxDeb);
        e.applied(Err(ApplyError::Failed { message: MSG_NO_PASSWORD.into(), block: true }), now);
        assert_eq!(e.saved.blocked, None);
        e.dismiss(now);
        assert_eq!(e.status.phase, ready("0.1.1"));
        e.status.phase = installing("0.1.1");
        e.applied(Err(ApplyError::Manual), now);
        assert!(matches!(&e.status.phase, Phase::Manual { path, .. } if path.ends_with("rustcord_0.1.1_amd64.deb")));
        // Damaged staged file: forgotten.
        let (mut e, now) = start(Kind::Auto);
        assert_eq!(e.applied(Err(ApplyError::Damaged), now), [Action::Save, Action::Prune]);
        assert_eq!(e.saved.staged, None);
        e.dismiss(now);
        assert_eq!(e.status.phase, Phase::Idle);
    }

    #[test]
    fn a_late_check_result_never_replaces_work() {
        // A silent install began while a check was out.
        let (mut e, now) = engine(Kind::Auto);
        e.saved.staged = Some(staged("0.1.1"));
        e.status.phase = ready("0.1.1");
        e.check_now();
        e.ui_changed(Ui { hidden: true, ..Ui::default() }, now);
        assert_eq!(e.tick(now + HIDDEN_WAIT), [Action::Apply(staged("0.1.1"))]);
        assert!(e.checked(Ok(offer("0.1.1", true)), now).is_empty());
        assert_eq!(e.status.phase, installing("0.1.1"));
        // A click started the download while a check was out.
        let (mut e, now) = engine(Kind::Auto);
        e.auto = false;
        e.check_now();
        e.checked(Ok(offer("0.1.1", true)), now);
        e.check_now();
        e.button(now);
        assert!(e.checked(Err(CheckError::Network("x".into())), now).is_empty());
        assert!(matches!(e.status.phase, Phase::Downloading { .. }) && e.status.screen);
    }

    #[test]
    fn start_plan_runs_first() {
        let now = Instant::now();
        let mut saved = Saved { staged: Some(staged("0.1.1")), ..Saved::default() };
        let plan = plan_start(&mut saved, &v("0.1.0"), Kind::Auto, true, false, false, true, None);
        let mut e = Engine::new(&plan, saved, v("0.1.0"), Kind::Auto, Some(Key::Windows), PathBuf::new(), true, false, now);
        assert_eq!(e.status, Status { phase: installing("0.1.1"), kind: Kind::Auto, screen: true, enabled: true, note: None });
        assert_eq!(e.begin(&plan, now), [Action::Apply(staged("0.1.1"))]);
        // A start error stays up for 5 s before the first check.
        let mut saved = Saved { attempted: Some(v("0.1.1")), ..Saved::default() };
        let plan = plan_start(&mut saved, &v("0.1.0"), Kind::Auto, true, false, false, true, None);
        let mut e = Engine::new(&plan, saved, v("0.1.0"), Kind::Auto, Some(Key::Windows), PathBuf::new(), true, false, now);
        assert!(e.begin(&plan, now).is_empty());
        assert_eq!(e.next_wake(now), Some(after(now, 5)));
        assert_eq!(e.tick(after(now, 5)), [Action::Check]);
        assert!(!e.status.screen);
    }
}
