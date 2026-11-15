//! Windows side of the overlay: who is in front, where the game's window is, and the styling
//! that makes an egui window an overlay (click-through, never focused, no frame). Only user32
//! and gdi32 calls on windows; the game's process is only asked for its exe path
//! (`PROCESS_QUERY_LIMITED_INFORMATION`, as game detection does).

use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, CreateRectRgn, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, SetWindowRgn};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_SYSTEM_FOREGROUND, EnumThreadWindows, GWL_EXSTYLE, GetClientRect, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    HWND_TOPMOST, IsIconic, IsWindowVisible, LWA_ALPHA, MSG, OBJID_WINDOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos, WDA_EXCLUDEFROMCAPTURE,
    WDA_NONE, WINEVENT_OUTOFCONTEXT, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows_sys::core::BOOL;

/// The window in front: its process's full path (normalized) when that process tells it, else
/// its file name alone (protected processes, e.g. under an anti-cheat).
#[derive(Clone, Debug, PartialEq)]
pub struct Foreground {
    pub hwnd: isize,
    pub pid: u32,
    pub path: Option<String>,
    pub name: String,
}

static CTX: OnceLock<egui::Context> = OnceLock::new();
static FOREGROUND: Mutex<Option<Foreground>> = Mutex::new(None);

/// The last foreground window seen (None before the watcher's first look).
pub fn foreground() -> Option<Foreground> {
    FOREGROUND.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Starts the foreground watcher, once. It sleeps in `GetMessageW` and wakes the root UI only
/// when the foreground window changes, so it costs nothing while idle (a poll would not).
pub fn watch(ctx: &egui::Context) {
    if CTX.set(ctx.clone()).is_err() {
        return;
    }
    if let Err(e) = std::thread::Builder::new().name("rustcord-foreground".into()).spawn(run) {
        eprintln!("rustcord: overlay: could not start the foreground watcher: {e}");
    }
}

fn run() {
    // SAFETY: the hook is installed on this thread, which then pumps messages for the life of
    // the process (an out-of-context hook is delivered inside `GetMessageW`); `on_event` only
    // reads its arguments.
    unsafe {
        let hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, std::ptr::null_mut(), Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        if hook.is_null() {
            eprintln!("rustcord: overlay: could not watch the foreground window");
            return;
        }
        seen(GetForegroundWindow());
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn on_event(_hook: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    if id_object == OBJID_WINDOW {
        seen(hwnd);
    }
}

/// Records `hwnd` as the foreground window and wakes the root UI if that changed anything.
fn seen(hwnd: HWND) {
    let mut pid = 0u32;
    if !hwnd.is_null() {
        // SAFETY: a plain query; a stale handle makes it fail and leave `pid` 0.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    }
    // No foreground window (the one in front closed and nothing took over yet) is not a game,
    // and the overlay must go: it is as good as another program being in front.
    let (path, name) = if pid == 0 || pid == std::process::id() {
        (None, String::new())
    } else {
        let path = crate::activity::process_image(pid).map(|(p, _)| p);
        let name = match &path {
            Some(p) => crate::activity::file_name(p).to_owned(),
            None => crate::activity::processes().into_iter().find(|(p, _)| *p == pid).map(|(_, n)| n).unwrap_or_default(),
        };
        (path, name)
    };
    let fg = Foreground { hwnd: hwnd as isize, pid, path, name };
    {
        let mut cur = FOREGROUND.lock().unwrap_or_else(|e| e.into_inner());
        if cur.as_ref() == Some(&fg) {
            return;
        }
        *cur = Some(fg);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint_after_for(Duration::from_millis(1), egui::ViewportId::ROOT);
    }
}

/// The game's client area on its monitor, in physical screen pixels (x, y, w, h); None while it
/// is minimized, hidden or under 64 px. Cheap enough for every root pass.
pub fn game_area(hwnd: isize) -> Option<[i32; 4]> {
    let hwnd = hwnd as HWND;
    // SAFETY: queries on a window handle (stale ones fail) with out-parameters that live through the calls.
    let m = unsafe {
        if IsIconic(hwnd) != 0 || IsWindowVisible(hwnd) == 0 {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
            return None;
        }
        info.rcMonitor
    };
    let c = client_rect(hwnd);
    let (x0, y0) = (c[0].max(m.left), c[1].max(m.top));
    let (x1, y1) = ((c[0] + c[2]).min(m.right), (c[1] + c[3]).min(m.bottom));
    (x1 - x0 >= 64 && y1 - y0 >= 64).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// This thread's window with that title (the overlay windows are created by the UI thread, so
/// another Rustcord's overlay, e.g. a test home, never matches).
pub fn own_window(title: &str) -> Option<HWND> {
    struct Find {
        title: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Find` below, alive for the whole enumeration.
        let find = unsafe { &mut *(lparam as *mut Find) };
        let mut buf = [0u16; 128];
        // SAFETY: the buffer outlives the call and its length is passed.
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 && buf[..n as usize] == find.title[..] {
            find.found = hwnd;
            return 0;
        }
        1
    }
    let mut find = Find { title: title.encode_utf16().collect(), found: std::ptr::null_mut() };
    // SAFETY: see `each`.
    unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(each), &mut find as *mut Find as LPARAM) };
    (!find.found.is_null()).then_some(find.found)
}

/// Makes the window an overlay: never activated, off the taskbar and Alt+Tab, shown with its
/// per-pixel alpha (a click-through window is layered, and shows nothing until it has layer
/// attributes), and left out of screen capture when `hide_from_capture`. winit rewrites the
/// extended style on every show, so call this every pass; true when this is a new window
/// handle or setting (the caller places the window then).
pub fn style(hwnd: HWND, hide_from_capture: bool, styled: &mut Option<(isize, bool)>) -> bool {
    // SAFETY: style queries and updates on a window of this thread.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = (ex | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize) & !(WS_EX_APPWINDOW as isize);
        if ex != want {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        }
        let key = Some((hwnd as isize, hide_from_capture));
        if *styled != key || ex != want {
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            SetWindowDisplayAffinity(hwnd, if hide_from_capture { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE });
        }
        let new = styled.map(|(h, _)| h) != Some(hwnd as isize);
        *styled = key;
        new
    }
}

/// The window's client rect in screen pixels (x, y, w, h).
pub fn client_rect(hwnd: HWND) -> [i32; 4] {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: out-parameters that live through the calls.
    unsafe {
        GetClientRect(hwnd, &mut r);
        ClientToScreen(hwnd, &mut origin);
    }
    [origin.x, origin.y, r.right - r.left, r.bottom - r.top]
}

/// Moves and sizes the window so its client rect is `r`, on top of everything (also a topmost
/// game), and cuts its region to the client rect: a borderless winit window otherwise gets a
/// drop shadow, a grey top line and the Windows 11 border around it.
pub fn set_client_rect(hwnd: HWND, r: [i32; 4]) {
    let mut win = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: out-parameters that live through the calls; the region now belongs to the system.
    unsafe {
        GetWindowRect(hwnd, &mut win);
        let c = client_rect(hwnd);
        let (l, t) = (c[0] - win.left, c[1] - win.top);
        let (rr, b) = (win.right - (c[0] + c[2]), win.bottom - (c[1] + c[3]));
        SetWindowPos(hwnd, HWND_TOPMOST, r[0] - l, r[1] - t, r[2] + l + rr, r[3] + t + b, SWP_NOACTIVATE | SWP_NOOWNERZORDER);
        SetWindowRgn(hwnd, CreateRectRgn(l, t, l + r[2], t + r[3]), 1);
    }
}
