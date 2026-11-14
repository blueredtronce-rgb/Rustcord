//! The activity thread: loads the saved games list, app icons and my added games, then every
//! `SCAN_EVERY` lists the running processes (Windows: one Toolhelp snapshot; Linux: `/proc`;
//! macOS: libproc; the full path only of processes whose file name is a known game) and
//! reports my game when it changes. It never
//! touches the network or the UI thread; it only sends `ActivityEvent`s.

use super::detect::{Table, file_name};
use super::{ActivityEvent, AddedGame, GameActivity};
use crate::backend::{BackendEvent, EventSink};
use crate::model::Id;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// Like the official client: a new game shows within ~15 s.
const SCAN_EVERY: Duration = Duration::from_secs(15);
/// Past this many lines the saved app icons start over (they are only a cache).
const MAX_SAVED_APPS: usize = 4000;

/// A window the "Add it!" picker offers: its title and its program.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub title: String,
    /// `detect::normalize`d path.
    pub exe: String,
}

enum Msg {
    Table(Arc<Table>),
    Added(Vec<AddedGame>),
    Windows,
    SaveApps(Vec<(Id, Option<String>)>),
}

/// Handle of the activity thread (the backend keeps it); the thread ends with the last handle.
#[derive(Clone)]
pub struct Service(mpsc::Sender<Msg>);

fn file(name: &str) -> PathBuf {
    let p = crate::config::paths();
    if name == GAMES_FILE { p.config_dir.join(name) } else { p.cache_dir.join(name) }
}

pub const TABLE_FILE: &str = "detectable.tsv";
const APPS_FILE: &str = "app_icons.tsv";
const GAMES_FILE: &str = "games.json";

/// Where the games list is saved (the backend checks its age).
pub fn table_path() -> PathBuf {
    file(TABLE_FILE)
}

impl Service {
    pub fn spawn(sink: EventSink) -> Service {
        let (tx, rx) = mpsc::channel();
        let run = move || run(&rx, &sink);
        if let Err(e) = std::thread::Builder::new().name("rustcord-activity".into()).spawn(run) {
            eprintln!("rustcord: game detection unavailable: {e}");
        }
        Service(tx)
    }

    /// A freshly fetched games list (already saved).
    pub fn table(&self, t: Arc<Table>) {
        let _ = self.0.send(Msg::Table(t));
    }

    /// My added games changed: saved, and a scan follows at once.
    pub fn set_added(&self, games: Vec<AddedGame>) {
        let _ = self.0.send(Msg::Added(games));
    }

    /// Answer with `ActivityEvent::Windows`.
    pub fn list_windows(&self) {
        let _ = self.0.send(Msg::Windows);
    }

    /// App icons from `/applications/public` (None: the app has none or is unknown); an empty
    /// list forgets the saved ones (logout).
    pub fn save_apps(&self, apps: Vec<(Id, Option<String>)>) {
        let _ = self.0.send(Msg::SaveApps(apps));
    }
}

fn run(rx: &mpsc::Receiver<Msg>, sink: &EventSink) {
    let send = |ev| sink.send(BackendEvent::Activity(ev));
    let mut table = Table::load(&file(TABLE_FILE)).map(Arc::new);
    if let Some(t) = &table {
        send(ActivityEvent::Table(t.clone()));
    }
    let apps = load_apps();
    if !apps.is_empty() {
        send(ActivityEvent::Apps(apps));
    }
    let mut added: Vec<AddedGame> = std::fs::read(file(GAMES_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    send(ActivityEvent::Added(added.clone()));
    // Test hook: that game, started 47 minutes ago.
    let fake = std::env::var("RUSTCORD_FAKE_GAME").ok().filter(|n| !n.is_empty()).map(|n| (n, super::now_ms().saturating_sub(47 * 60_000)));
    let mut current: Option<GameActivity> = None;
    let mut scan = true;
    loop {
        if scan {
            let found = match &fake {
                Some((name, start_ms)) => {
                    let application_id = table.as_ref().and_then(|t| t.by_name(name)).map(|a| a.id);
                    Some(GameActivity { name: name.clone(), application_id, start_ms: *start_ms })
                }
                None => pick(&os::processes(), table.as_deref(), &added, current.as_ref(), os::image),
            };
            if found != current {
                send(ActivityEvent::Detected(found.clone()));
                current = found;
            }
        }
        scan = match rx.recv_timeout(SCAN_EVERY) {
            Ok(Msg::Table(t)) => {
                table = Some(t);
                true
            }
            Ok(Msg::Added(games)) => {
                match serde_json::to_vec_pretty(&games) {
                    Ok(b) => {
                        if let Err(e) = crate::config::write_atomic(&file(GAMES_FILE), &b) {
                            eprintln!("rustcord: could not save the added games: {e}");
                        }
                    }
                    Err(e) => eprintln!("rustcord: could not save the added games: {e}"),
                }
                added = games;
                true
            }
            Ok(Msg::Windows) => {
                send(ActivityEvent::Windows(os::windows()));
                false
            }
            Ok(Msg::SaveApps(apps)) => {
                save_apps(&apps);
                false
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// My game among `procs` (pid, lowercase file name): an added game (by its full path) before
/// the list's; the game already shown while it still runs, else the one started first.
/// `image` gives a process's normalized path and start time (unix ms); a process that won't tell
/// (protected, e.g. under an anti-cheat) still counts by its file name alone, from now.
fn pick(procs: &[(u32, String)], table: Option<&Table>, added: &[AddedGame], current: Option<&GameActivity>, mut image: impl FnMut(u32) -> Option<(String, u64)>) -> Option<GameActivity> {
    let mut found: Vec<GameActivity> = Vec::new();
    for (pid, name) in procs {
        let mine = added.iter().any(|g| file_name(&g.exe) == name);
        if !mine && !table.is_some_and(|t| t.has_file(name)) {
            continue;
        }
        let (path, start_ms) = image(*pid).unwrap_or_else(|| (name.clone(), super::now_ms()));
        let game = match added.iter().find(|g| g.exe == path) {
            Some(g) => Some(GameActivity { name: g.name.clone(), application_id: None, start_ms }),
            None => table.and_then(|t| t.find(&path)).map(|a| GameActivity { name: a.name.to_string(), application_id: Some(a.id), start_ms }),
        };
        found.extend(game);
    }
    let still = current.filter(|c| found.iter().any(|g| g.name == c.name && g.application_id == c.application_id));
    still.cloned().or_else(|| found.into_iter().min_by_key(|g| g.start_ms))
}

fn load_apps() -> Vec<(Id, Option<String>)> {
    let text = std::fs::read_to_string(file(APPS_FILE)).unwrap_or_default();
    let line = |l: &str| {
        let (id, icon) = l.split_once('\t')?;
        Some((Id(id.parse().ok()?), Some(icon.to_owned()).filter(|i| !i.is_empty())))
    };
    text.lines().take(MAX_SAVED_APPS).filter_map(line).collect()
}

fn save_apps(apps: &[(Id, Option<String>)]) {
    use std::io::Write;
    let path = file(APPS_FILE);
    if apps.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let full = std::fs::read_to_string(&path).is_ok_and(|t| t.lines().count() >= MAX_SAVED_APPS);
    let lines: String = apps.iter().map(|(id, icon)| format!("{id}\t{}\n", icon.as_deref().filter(|i| !i.contains(char::is_control)).unwrap_or_default())).collect();
    let file = std::fs::OpenOptions::new().create(true).append(!full).write(true).truncate(full).open(&path);
    if let Err(e) = file.and_then(|mut f| f.write_all(lines.as_bytes())) {
        eprintln!("rustcord: could not save app icons: {e}");
    }
}

#[cfg(windows)]
mod os {
    use super::Window;
    use crate::activity::detect::normalize;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HWND, INVALID_HANDLE_VALUE, LPARAM};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW};
    use windows_sys::core::BOOL;

    /// (pid, lowercase exe file name) of every process.
    pub fn processes() -> Vec<(u32, String)> {
        let mut out = Vec::new();
        // SAFETY: plain Toolhelp calls on a snapshot handle this function owns and closes.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push((e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase()));
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    /// A process's full path (normalized) and start time (unix ms); None when it is gone or
    /// not ours to query.
    pub fn image(pid: u32) -> Option<(String, u64)> {
        // SAFETY: the handle is checked, used with buffers that live through the calls, closed.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let path = (QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]));
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0;
            CloseHandle(h);
            // 100 ns ticks since 1601 -> unix ms.
            let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            let start = if ok { (ticks / 10_000).saturating_sub(11_644_473_600_000) } else { super::super::now_ms() };
            Some((normalize(&path?), start))
        }
    }

    /// Visible top-level app windows (not tool windows, not owned dialogs), one per process,
    /// mine left out, sorted by title.
    pub fn windows() -> Vec<Window> {
        unsafe extern "system" fn each(hwnd: HWND, list: LPARAM) -> BOOL {
            // SAFETY: `list` is the Vec passed below, alive for the whole EnumWindows call.
            unsafe { (*(list as *mut Vec<HWND>)).push(hwnd) };
            1
        }
        let mut hwnds: Vec<HWND> = Vec::new();
        let mut out: Vec<(u32, Window)> = Vec::new();
        // SAFETY: the callback only pushes into `hwnds`; each HWND is only queried.
        unsafe {
            EnumWindows(Some(each), &mut hwnds as *mut Vec<HWND> as LPARAM);
            for hwnd in hwnds {
                let tool = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0;
                if IsWindowVisible(hwnd) == 0 || tool || !GetWindow(hwnd, GW_OWNER).is_null() {
                    continue;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid == std::process::id() || out.iter().any(|(p, _)| *p == pid) {
                    continue;
                }
                let mut buf = [0u16; 256];
                let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
                let title = String::from_utf16_lossy(&buf[..len]).trim().to_owned();
                if let (false, Some((exe, _))) = (title.is_empty(), image(pid)) {
                    out.push((pid, Window { title, exe }));
                }
            }
        }
        let mut list: Vec<Window> = out.into_iter().map(|(_, w)| w).collect();
        list.sort_by_key(|w| w.title.to_lowercase());
        list
    }
}

/// `/proc` (Wine / Proton games by their Windows path).
#[cfg(target_os = "linux")]
#[path = "scan/linux.rs"]
mod os;

/// libproc (games by their `.app` bundle).
#[cfg(target_os = "macos")]
#[path = "scan/macos.rs"]
mod os;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_added_games_first_and_keeps_the_shown_one() {
        let json = r#"[{"id":"1","name":"Factorio","executables":[{"name":"factorio.exe","os":"win32"}]},
                        {"id":"2","name":"Celeste","executables":[{"name":"celeste/celeste.exe","os":"win32"}]}]"#;
        let t = Table::from_json(json.as_bytes()).unwrap();
        let procs: Vec<(u32, String)> = [(1, "explorer.exe"), (2, "factorio.exe"), (3, "celeste.exe"), (4, "mygame.exe")].map(|(p, n)| (p, n.to_owned())).to_vec();
        let paths = |pid: u32| match pid {
            2 => Some(("c:/games/factorio/bin/factorio.exe".to_owned(), 300)),
            3 => Some(("d:/celeste/celeste.exe".to_owned(), 200)),
            4 => Some(("c:/my/mygame.exe".to_owned(), 100)),
            _ => panic!("only candidates are looked up, not pid {pid}"),
        };
        let game = |name: &str, id: Option<u64>, start| GameActivity { name: name.into(), application_id: id.map(Id), start_ms: start };
        // The one started first.
        assert_eq!(pick(&procs, Some(&t), &[], None, paths), Some(game("Celeste", Some(2), 200)));
        // The shown one stays while it runs, with its first start time.
        let shown = game("Factorio", Some(1), 5);
        assert_eq!(pick(&procs, Some(&t), &[], Some(&shown), paths), Some(shown.clone()));
        // An added game wins.
        let added = [AddedGame { name: "My Game".into(), exe: "c:/my/mygame.exe".into() }];
        assert_eq!(pick(&procs, Some(&t), &added, None, paths), Some(game("My Game", None, 100)));
        // An added game elsewhere on disk doesn't count; no list: only added games.
        let elsewhere = [AddedGame { name: "My Game".into(), exe: "e:/mygame.exe".into() }];
        assert_eq!(pick(&procs[3..], None, &elsewhere, None, paths), None);
        assert_eq!(pick(&procs[..1], Some(&t), &[], Some(&shown), paths), None, "game quit");
        // A protected process: its name alone, never for a list entry that needs its folder.
        let hidden = |_| None;
        assert_eq!(pick(&procs[1..2], Some(&t), &[], Some(&shown), hidden), Some(shown.clone()));
        assert_eq!(pick(&procs[2..3], Some(&t), &[], None, hidden), None);
    }

    #[test]
    fn lists_this_process_with_its_path_and_start() {
        let me = std::process::id();
        let (_, name) = os::processes().into_iter().find(|(pid, _)| *pid == me).expect("this process is listed");
        let (path, start) = os::image(me).unwrap();
        assert!(path.ends_with(&format!("/{name}")) && path == crate::activity::detect::normalize(&path), "{path} / {name}");
        let now = crate::activity::now_ms();
        assert!(start <= now && now - start < 3_600_000, "started {start}, now {now}");
        assert!(os::windows().iter().all(|w| !w.title.is_empty() && w.exe.contains('/')));
    }
}
