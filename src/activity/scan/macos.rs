//! Game detection on macOS: libproc. A game is matched by its `.app` bundle
//! (`…/_retail_/world of warcraft.app`, like the list's darwin entries); other users'
//! processes don't tell their path and are skipped.
//! UNCONFIRMED: only `cargo check`ed (no Mac here).

use super::Window;
use crate::activity::detect::{file_name, normalize};

/// (pid, lowercase file name) of the processes that tell their path.
pub fn processes() -> Vec<(u32, String)> {
    pids().into_iter().filter_map(|pid| Some((pid, file_name(&normalize(bundle(&raw_path(pid)?))).to_owned()))).collect()
}

/// A process's path (normalized, cut after its `.app`) and start time (unix ms).
pub fn image(pid: u32) -> Option<(String, u64)> {
    let path = normalize(bundle(&raw_path(pid)?));
    // SAFETY: a zeroed plain C struct of the size passed in; the call fills at most that much.
    let start = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        (libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) == size).then(|| info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
    };
    Some((path, start.unwrap_or_else(crate::activity::now_ms)))
}

/// The "Add it!" picker: running apps (bundles outside /System), one per app, by name.
pub fn windows() -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for pid in pids().into_iter().filter(|&p| p != std::process::id()) {
        let Some(raw) = raw_path(pid) else { continue };
        let app = bundle(&raw);
        let exe = normalize(app);
        if !app.ends_with(".app") || app.starts_with("/System/") || out.iter().any(|w| w.exe == exe) {
            continue;
        }
        let title = app.rsplit('/').next().unwrap_or(app).trim_end_matches(".app").to_owned();
        out.push(Window { title, exe });
    }
    out.sort_by_key(|w| w.title.to_lowercase());
    out
}

fn pids() -> Vec<u32> {
    // SAFETY: a null buffer asks for the count; the second call fills at most the buffer's size.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; n as usize + 64];
        let got = libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<i32>()) as i32);
        pids.truncate(got.max(0) as usize);
        pids.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
    }
}

fn raw_path(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer holds the size passed in.
    let len = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

/// The path up to its first `.app` folder (the bundle), or the whole path.
fn bundle(path: &str) -> &str {
    path.find(".app/").map_or(path, |i| &path[..i + 4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundle_path_cut() {
        let p = "/Applications/World of Warcraft/_retail_/World of Warcraft.app/Contents/MacOS/World of Warcraft";
        assert_eq!(normalize(bundle(p)), "/applications/world of warcraft/_retail_/world of warcraft.app");
        assert_eq!(bundle("/usr/local/bin/game"), "/usr/local/bin/game");
    }
}
