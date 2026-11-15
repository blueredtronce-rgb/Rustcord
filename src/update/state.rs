//! What the updater shows: written by the updater task (`super`), read by the UI every pass
//! (`ui::update::status`). docs/UPDATER_DESIGN.md §8.
//! OWNERSHIP: updater core. Contract file: change only additively.

use std::path::PathBuf;
use std::sync::Mutex;

/// What the updater is doing, or found.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    /// No check yet, or the updater is off for this run.
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// A newer version that is not downloaded (auto install off, a blocked version, or this
    /// copy can't update itself: see `Status::kind`).
    Available { version: String },
    Downloading { version: String, done: u64, total: u64 },
    /// Downloaded, verified and staged.
    Ready { version: String },
    Installing { version: String },
    /// The new version is installed; this one quits (once no call runs) and it starts.
    Restarting { version: String },
    /// Linux .deb without pkexec / apt-get: the verified package waits at `path`.
    Manual { version: String, path: PathBuf },
    /// One line for the user (no secrets).
    Failed { message: String },
}

/// How the current offer installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    /// Windows setup, Linux install.sh, macOS bundle in a writable folder.
    Auto,
    /// Linux .deb: downloads itself; installing asks for the password (pkexec).
    Package,
    /// Portable, dev or read-only copy, or no package for this platform: the title-bar
    /// button opens the release page.
    #[default]
    NoticeOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub phase: Phase,
    pub kind: Kind,
    /// The full-window update screen covers the app.
    pub screen: bool,
    /// False when the updater is off for this run (debug build, RUSTCORD_* test hooks).
    pub enabled: bool,
    /// Why a background download failed (the phase went back to `Available`; the settings show
    /// it). Cleared by the next phase change.
    pub note: Option<String>,
}

pub(super) static STATUS: Mutex<Status> = Mutex::new(Status { phase: Phase::Idle, kind: Kind::NoticeOnly, screen: false, enabled: false, note: None });

/// The current state (a clone; call once per pass).
pub fn status() -> Status {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
