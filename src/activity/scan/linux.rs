//! Game detection on Linux: my own processes in `/proc`. A Wine / Proton game shows its Windows
//! path as argv[0] (`Z:\home\ann\.steam\steamapps\common\Celeste\Celeste.exe`), so it matches
//! the list's win32 entries like on Windows; native games match by their executable.

use super::Window;
use crate::activity::detect::{file_name, normalize};
use std::os::unix::fs::MetadataExt;

/// (pid, lowercase file name) of my processes.
pub fn processes() -> Vec<(u32, String)> {
    my_pids().filter_map(|pid| Some((pid, file_name(&normalize(&raw_path(pid)?)).to_owned()))).collect()
}

/// A process's path (normalized) and start time (unix ms); None when it is gone.
pub fn image(pid: u32) -> Option<(String, u64)> {
    let path = normalize(&raw_path(pid)?);
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
    let btime = std::fs::read_to_string("/proc/stat").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()));
    // SAFETY: sysconf only reads a system constant.
    let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|&h| h > 0).unwrap_or(100);
    let start = stat.zip(btime).and_then(|(stat, btime)| start_ms(&stat, btime, hz)).unwrap_or_else(crate::activity::now_ms);
    Some((path, start))
}

/// The "Add it!" picker: my programs outside the system folders, one per program, by name
/// (Linux has no portable window list without X11 code).
pub fn windows() -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for pid in my_pids().filter(|&p| p != std::process::id()) {
        let Some(raw) = raw_path(pid) else { continue };
        let exe = normalize(&raw);
        let system = ["/usr/", "/bin/", "/sbin/", "/lib"].iter().any(|p| exe.starts_with(p)) || exe.contains("/windows/system32/");
        if system || out.iter().any(|w| w.exe == exe) {
            continue;
        }
        let title = raw.rsplit(['/', '\\']).next().unwrap_or(&raw).to_owned();
        out.push(Window { title, exe });
    }
    out.sort_by_key(|w| w.title.to_lowercase());
    out
}

/// Numeric `/proc` entries owned by me (other users' processes are none of my business).
fn my_pids() -> impl Iterator<Item = u32> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let entries = std::fs::read_dir("/proc").into_iter().flatten().flatten();
    entries.filter(move |e| e.metadata().is_ok_and(|m| m.uid() == uid)).filter_map(|e| e.file_name().to_str()?.parse().ok())
}

/// argv[0] when it is a Windows program (Wine / Proton), else the executable's path.
fn raw_path(pid: u32) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    let _ = std::fs::File::open(format!("/proc/{pid}/cmdline")).and_then(|f| f.take(4096).read_to_end(&mut head));
    let argv0 = String::from_utf8_lossy(head.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    pick_path(argv0, || std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned()))
}

fn pick_path(argv0: String, exe: impl FnOnce() -> Option<String>) -> Option<String> {
    if argv0.to_lowercase().ends_with(".exe") { Some(argv0) } else { exe() }
}

/// Start time (unix ms) from a `/proc/<pid>/stat` line: field 22 (clock ticks after boot),
/// counted after the last `)` (the name may hold spaces and parentheses).
fn start_ms(stat: &str, btime_s: u64, hz: u64) -> Option<u64> {
    let ticks: u64 = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    Some(btime_s * 1000 + ticks * 1000 / hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_start_time() {
        // comm "my (game) x": spaces and parentheses; starttime (field 22) = 4250 ticks.
        let stat = "1234 (my (game) x) S 1 1234 1234 0 -1 4194304 100 0 0 0 5 3 0 0 20 0 4 0 4250 123456 789 18446744073709551615";
        assert_eq!(start_ms(stat, 1_700_000_000, 100), Some(1_700_000_000_000 + 42_500));
        assert_eq!(start_ms("1234 (x) S 1", 0, 100), None);
    }

    #[test]
    fn wine_argv0_is_the_game() {
        let argv0 = r"Z:\home\ann\.steam\steamapps\common\Celeste\Celeste.exe".to_owned();
        let path = normalize(&pick_path(argv0, || panic!("not asked for a Windows program")).unwrap());
        assert_eq!(path, "z:/home/ann/.steam/steamapps/common/celeste/celeste.exe");
        assert_eq!(file_name(&path), "celeste.exe");
        let t = crate::activity::detect::Table::from_json(br#"[{"id":"2","name":"Celeste","executables":[{"name":"celeste/celeste.exe","os":"win32"}]}]"#).unwrap();
        assert_eq!(t.find(&path).map(|a| &*a.name), Some("Celeste"));
        // A native program: its executable.
        assert_eq!(pick_path("bash".into(), || Some("/usr/bin/bash".into())).as_deref(), Some("/usr/bin/bash"));
    }
}
