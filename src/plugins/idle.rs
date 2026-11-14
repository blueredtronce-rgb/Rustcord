//! CustomIdle (PLUGINS_NOTES S45): like the official client's auto-idle, with my own timeout.
//! After N minutes without input anywhere on the system (Windows `GetLastInputInfo`, macOS
//! CoreGraphics) an Online status becomes Idle with one op 3 (`afk: true`, as the official
//! client sends it); the first input after that restores Online with one more.
//! Never persisted (no settings write), only while Online, a no-op on Linux.

use super::{Plugin, enabled, opt_int};
use crate::backend::{ChatCommand, Command, CommandSender};
use crate::model::UserStatus;
use crate::state::AppState;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// While auto-idle: the status to restore.
static RESTORE: Mutex<Option<UserStatus>> = Mutex::new(None);

/// How often an auto-idle session looks for the user's return (input in any app counts, and
/// nothing wakes a window that input doesn't reach).
const RETURN_POLL: Duration = Duration::from_secs(5);

/// The `watch_return` thread is running.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Called from `App::logic` (also while minimized) on the main screen.
pub fn logic(ctx: &egui::Context, state: &AppState, cmd: &CommandSender) {
    let timeout = enabled(Plugin::CustomIdle).then(|| opt_int(Plugin::CustomIdle, "idleTimeout")).filter(|&m| m > 0).map(|m| Duration::from_secs(m as u64 * 60));
    let mut restore = RESTORE.lock().unwrap_or_else(|e| e.into_inner());
    if timeout.is_none() && restore.is_none() {
        return;
    }
    let Some(idle) = system_idle() else { return };
    if state.conn != crate::backend::ConnState::Connected {
        return;
    }
    if let Some(status) = step(&mut restore, state.chat.my_status, idle, timeout) {
        cmd.send(Command::Chat(ChatCommand::SetStatus { status, afk: status == UserStatus::Idle }));
    }
    match (*restore, timeout) {
        (Some(_), Some(t)) => watch_return(ctx, t),
        (None, Some(t)) if state.chat.my_status == Some(UserStatus::Online) => ctx.request_repaint_after(t.saturating_sub(idle) + Duration::from_secs(1)),
        _ => {}
    }
}

/// While auto-idle: polls the system idle time off the UI thread and wakes the UI only once input
/// was seen (a 5 s repaint would run a full UI pass for as long as the user is away).
fn watch_return(ctx: &egui::Context, timeout: Duration) {
    if WATCHING.swap(true, Ordering::Relaxed) {
        return;
    }
    let woken = ctx.clone();
    let watch = move || {
        loop {
            std::thread::sleep(RETURN_POLL);
            let ended = RESTORE.lock().unwrap_or_else(|e| e.into_inner()).is_none();
            let back = !ended && system_idle().is_none_or(|idle| idle < timeout);
            if ended || back {
                // Before the wake-up, so the frame it brings may start the next watcher.
                WATCHING.store(false, Ordering::Relaxed);
                if back {
                    woken.request_repaint();
                }
                return;
            }
        }
    };
    if std::thread::Builder::new().name("idle-return".into()).spawn(watch).is_err() {
        WATCHING.store(false, Ordering::Relaxed);
        ctx.request_repaint_after(RETURN_POLL);
    }
}

/// One check: the status to send, if this is a transition. `timeout` None: plugin off (an
/// auto-idle still in force is ended).
fn step(restore: &mut Option<UserStatus>, status: Option<UserStatus>, idle: Duration, timeout: Option<Duration>) -> Option<UserStatus> {
    let away = timeout.is_some_and(|t| idle >= t);
    match *restore {
        None if away && status == Some(UserStatus::Online) => {
            *restore = Some(UserStatus::Online);
            Some(UserStatus::Idle)
        }
        // Back: restore, unless the user picked another status meanwhile (the echo of my op 3
        // may not have arrived yet: still the old status then).
        Some(prev) if !away => {
            *restore = None;
            matches!(status, Some(s) if s == UserStatus::Idle || s == prev).then_some(prev)
        }
        _ => None,
    }
}

#[cfg(windows)]
fn system_idle() -> Option<Duration> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: plain Win32 calls; `info` is a correctly sized out struct.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: no arguments; both tick counts wrap after ~49.7 days together.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime))))
}

/// Seconds since any input (keyboard, mouse) in the HID system state.
#[cfg(target_os = "macos")]
fn system_idle() -> Option<Duration> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    // SAFETY: a pure query; 1 = kCGEventSourceStateHIDSystemState, !0 = any input event type.
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(1, u32::MAX) };
    Duration::try_from_secs_f64(secs).ok()
}

// shortcut: no idle time on Linux (X11 would need the screensaver extension); add it with x11rb.
#[cfg(not(any(windows, target_os = "macos")))]
fn system_idle() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use UserStatus::*;

    #[test]
    fn custom_idle_transitions() {
        let (min, t) = (Duration::from_secs(60), Some(Duration::from_secs(600)));
        let mut r = None;
        // Active, or not online: nothing.
        assert_eq!(step(&mut r, Some(Online), 9 * min, t), None);
        assert_eq!(step(&mut r, Some(Dnd), 11 * min, t), None);
        assert_eq!(step(&mut r, Some(Online), 11 * min, None), None);
        // Away: one Idle, then nothing while still away.
        assert_eq!(step(&mut r, Some(Online), 10 * min, t), Some(Idle));
        assert_eq!(step(&mut r, Some(Idle), 30 * min, t), None);
        // Input: one Online.
        assert_eq!(step(&mut r, Some(Idle), Duration::ZERO, t), Some(Online));
        assert_eq!(step(&mut r, Some(Online), Duration::ZERO, t), None);
        // The user picked Do Not Disturb while idle: kept.
        assert_eq!(step(&mut r, Some(Online), 10 * min, t), Some(Idle));
        assert_eq!(step(&mut r, Some(Dnd), Duration::ZERO, t), None);
        assert_eq!(r, None);
        // Switched off while idle: back online.
        assert_eq!(step(&mut r, Some(Online), 10 * min, t), Some(Idle));
        assert_eq!(step(&mut r, Some(Idle), 20 * min, None), Some(Online));
    }
}
