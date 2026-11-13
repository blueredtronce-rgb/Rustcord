//! Desktop notifications.
//! Windows: a tray icon (Shell_NotifyIconW) on a dedicated thread showing balloon/toast
//! notifications; no extra crates (windows-sys only). Other platforms: no-op for now.
//!
//! OWNERSHIP: notify implementer.

/// Show a notification (non-blocking). Title/body are truncated to the platform limits.
pub fn show(title: &str, body: &str) {
    #[cfg(windows)]
    imp::show(title, body);
    #[cfg(not(windows))]
    let _ = (title, body);
}

/// Remove the tray icon (call on exit).
pub fn shutdown() {
    #[cfg(windows)]
    imp::shutdown();
}

#[cfg(windows)]
mod imp {
    use std::ptr::{null, null_mut};
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_TIP, NIIF_RESPECT_QUIET_TIME, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, HICON, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MSG,
        PM_REMOVE, PeekMessageW, TranslateMessage,
    };

    /// At most one balloon per this interval; messages in between are summarized.
    const MIN_GAP: Duration = Duration::from_secs(3);

    enum Msg {
        Show(String, String),
        Shutdown(Sender<()>),
    }

    enum Worker {
        NotStarted,
        Running(Sender<Msg>),
        Stopped,
    }

    static WORKER: Mutex<Worker> = Mutex::new(Worker::NotStarted);

    pub fn show(title: &str, body: &str) {
        let mut worker = WORKER.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*worker, Worker::NotStarted) {
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new().name("rustcord-tray".into()).spawn(move || run(rx));
            *worker = if spawned.is_ok() { Worker::Running(tx) } else { Worker::Stopped };
        }
        if let Worker::Running(tx) = &*worker {
            let _ = tx.send(Msg::Show(title.to_owned(), body.to_owned()));
        }
    }

    pub fn shutdown() {
        let previous = std::mem::replace(&mut *WORKER.lock().unwrap_or_else(|e| e.into_inner()), Worker::Stopped);
        if let Worker::Running(tx) = previous {
            let (ack_tx, ack_rx) = mpsc::channel();
            // Wait briefly so the icon is really gone before the process exits (no ghost icon).
            if tx.send(Msg::Shutdown(ack_tx)).is_ok() {
                let _ = ack_rx.recv_timeout(Duration::from_secs(1));
            }
        }
    }

    fn run(rx: Receiver<Msg>) {
        let Some(tray) = Tray::create() else { return };
        let mut coalescer = Coalescer::default();
        loop {
            tray.pump();
            // Block without a timeout while nothing is pending: the window gets no callbacks, so idle stays at zero wakeups.
            let received = match coalescer.next_flush(Instant::now()) {
                Some(wait) => rx.recv_timeout(wait),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(Msg::Show(title, body)) => {
                    if let Some((t, b)) = coalescer.push(Instant::now(), title, body) {
                        tray.balloon(&t, &b);
                    }
                }
                Ok(Msg::Shutdown(ack)) => {
                    drop(tray);
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if let Some((t, b)) = coalescer.flush(Instant::now()) {
                tray.balloon(&t, &b);
            }
        }
    }

    /// Rate limiting of balloons: the first message shows right away, later ones within
    /// `MIN_GAP` are counted and shown as one summary once the gap has passed.
    #[derive(Default)]
    struct Coalescer {
        last_shown: Option<Instant>,
        pending: u32,
        latest: Option<(String, String)>,
    }

    impl Coalescer {
        fn ready(&self, now: Instant) -> bool {
            self.last_shown.is_none_or(|t| now.duration_since(t) >= MIN_GAP)
        }

        fn push(&mut self, now: Instant, title: String, body: String) -> Option<(String, String)> {
            if self.pending == 0 && self.ready(now) {
                self.last_shown = Some(now);
                return Some((title, body));
            }
            self.pending += 1;
            self.latest = Some((title, body));
            None
        }

        fn flush(&mut self, now: Instant) -> Option<(String, String)> {
            if self.pending == 0 || !self.ready(now) {
                return None;
            }
            let count = std::mem::take(&mut self.pending);
            let latest = self.latest.take();
            self.last_shown = Some(now);
            if count == 1 {
                latest
            } else {
                Some((crate::config::APP_NAME.to_owned(), format!("{count} new messages")))
            }
        }

        /// Time until a pending summary may be shown, `None` if nothing is pending.
        fn next_flush(&self, now: Instant) -> Option<Duration> {
            (self.pending > 0)
                .then(|| self.last_shown.map_or(Duration::ZERO, |t| MIN_GAP.saturating_sub(now.duration_since(t))))
        }
    }

    /// Hidden message-only window + tray icon, owned by the worker thread.
    struct Tray {
        hwnd: HWND,
        icon: HICON,
    }

    impl Tray {
        fn create() -> Option<Self> {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            // SAFETY: plain Win32 calls with valid, NUL-terminated arguments; the handles are
            // only used on this thread.
            unsafe {
                let module = GetModuleHandleW(null());
                let hwnd = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), module, null());
                if hwnd.is_null() {
                    return None;
                }
                // Resource id 1 = the app icon embedded by the build script.
                let mut icon = LoadIconW(module, std::ptr::without_provenance(1));
                if icon.is_null() {
                    icon = LoadIconW(null_mut(), IDI_APPLICATION);
                }
                let tray = Tray { hwnd, icon };
                tray.add().then_some(tray)
            }
        }

        fn data(&self) -> NOTIFYICONDATAW {
            NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            }
        }

        fn add(&self) -> bool {
            let mut data = self.data();
            data.uFlags = NIF_ICON | NIF_TIP;
            data.hIcon = self.icon;
            fill_wide(&mut data.szTip, crate::config::APP_NAME);
            // SAFETY: `data` is a fully initialized NOTIFYICONDATAW with the right cbSize.
            unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
        }

        fn balloon(&self, title: &str, body: &str) {
            let mut data = self.data();
            data.uFlags = NIF_INFO;
            data.dwInfoFlags = NIIF_USER | NIIF_RESPECT_QUIET_TIME;
            data.hBalloonIcon = self.icon;
            fill_wide(&mut data.szInfoTitle, title);
            // An empty szInfo would remove the balloon instead of showing it.
            fill_wide(&mut data.szInfo, if body.trim().is_empty() { "New message" } else { body });
            // SAFETY: as in `add`.
            unsafe {
                // Fails when Explorer restarted and dropped our icon: add it again and retry.
                if Shell_NotifyIconW(NIM_MODIFY, &data) == 0 && self.add() {
                    Shell_NotifyIconW(NIM_MODIFY, &data);
                }
            }
        }

        fn pump(&self) {
            let mut msg = MSG::default();
            // SAFETY: standard message loop on the thread that owns the window.
            unsafe {
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let data = self.data();
            // SAFETY: handles were created by this thread and are destroyed exactly once.
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &data);
                DestroyWindow(self.hwnd);
            }
        }
    }

    /// Copies `s` into a fixed UTF-16 buffer with a terminating NUL.
    fn fill_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
        let units = truncate_utf16(s, N - 1);
        dst[..units.len()].copy_from_slice(&units);
        dst[units.len()] = 0;
    }

    /// UTF-16 of `s` (NULs removed) in at most `max` units; cut on a char boundary and marked
    /// with "…" when it doesn't fit.
    fn truncate_utf16(s: &str, max: usize) -> Vec<u16> {
        let clean: String = s.chars().filter(|&c| c != '\0').collect();
        let units: Vec<u16> = clean.encode_utf16().collect();
        if units.len() <= max {
            return units;
        }
        let mut out = Vec::with_capacity(max);
        let mut buf = [0u16; 2];
        for c in clean.chars() {
            let enc = c.encode_utf16(&mut buf);
            if out.len() + enc.len() + 1 > max {
                break;
            }
            out.extend_from_slice(enc);
        }
        if max > 0 {
            out.push('…' as u16);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn truncation_fits_and_respects_char_boundaries() {
            assert_eq!(truncate_utf16("hi", 63), "hi".encode_utf16().collect::<Vec<_>>());
            assert_eq!(truncate_utf16("a\0b", 63), "ab".encode_utf16().collect::<Vec<_>>());

            let exact = "x".repeat(63);
            assert_eq!(truncate_utf16(&exact, 63).len(), 63);

            let long = "y".repeat(300);
            let cut = truncate_utf16(&long, 255);
            assert_eq!(cut.len(), 255);
            assert_eq!(*cut.last().unwrap(), '…' as u16);

            // 😀 is a surrogate pair; never keep half of one.
            let emoji = "😀".repeat(40);
            let cut = truncate_utf16(&emoji, 63);
            assert!(cut.len() <= 63);
            assert!(String::from_utf16(&cut).is_ok());
            assert!(String::from_utf16(&cut).unwrap().ends_with("😀…"));

            let mut buf = [1u16; 64];
            fill_wide(&mut buf, &long);
            assert_eq!(buf[63], 0);
        }

        #[test]
        fn coalescer_rate_limits_and_summarizes() {
            let t0 = Instant::now();
            let s = |n: u64| t0 + Duration::from_millis(n);
            let mut c = Coalescer::default();
            let msg = |i: u32| (format!("t{i}"), format!("b{i}"));

            assert_eq!(c.push(s(0), "t0".into(), "b0".into()), Some(msg(0)));
            assert_eq!(c.push(s(100), "t1".into(), "b1".into()), None);
            assert_eq!(c.next_flush(s(1000)), Some(Duration::from_millis(2000)));
            assert_eq!(c.flush(s(2999)), None);
            // A single throttled message is shown as itself.
            assert_eq!(c.flush(s(3000)), Some(msg(1)));
            assert_eq!(c.next_flush(s(3000)), None);

            for i in 2..6 {
                assert_eq!(c.push(s(3500), format!("t{i}"), format!("b{i}")), None);
            }
            let (_, body) = c.flush(s(6000)).unwrap();
            assert_eq!(body, "4 new messages");
            assert_eq!(c.flush(s(9000)), None);

            // Quiet for long enough: shown immediately again.
            assert_eq!(c.push(s(9001), "t9".into(), "b9".into()), Some(msg(9)));
        }
    }
}
