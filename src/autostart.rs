//! Open Rustcord at sign-in, one API on three systems:
//! - Windows: the HKCU Run value "Rustcord" (`winreg.rs`; the installer writes it too).
//! - Linux: `$XDG_CONFIG_HOME/autostart/rustcord.desktop` (XDG autostart).
//! - macOS: `~/Library/LaunchAgents/io.github.r0sc.rustcord.plist` (launchd loads it at the
//!   next login; no `launchctl` call).
//!
//! `default_once` turns it on (minimized) once per data dir, for an installed copy only: never
//! in debug builds, never with a `RUSTCORD_*` test hook set, never over an existing entry.
//! OWNERSHIP: desktop engineer.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// The bundle id; also the LaunchAgent's label and file name.
#[cfg(any(unix, test))]
const BUNDLE_ID: &str = "io.github.r0sc.rustcord";

/// The sign-in entry, if there is one and it is not switched off: the Run command line on
/// Windows, the entry file's text elsewhere (both contain `--minimized` when it starts hidden).
pub fn get() -> Option<String> {
    #[cfg(windows)]
    {
        crate::winreg::autostart(crate::config::APP_NAME)
    }
    #[cfg(unix)]
    {
        read_entry(&entry_path()?)
    }
}

/// Writes the entry for this exe (`--minimized` when `minimized`), or removes it (`on == false`).
pub fn set(on: bool, minimized: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        crate::winreg::set_autostart(crate::config::APP_NAME, on.then_some(exe.as_path()), minimized)
    }
    #[cfg(target_os = "linux")]
    let text = desktop_entry(&exe, minimized);
    #[cfg(target_os = "macos")]
    let text = launch_agent(&exe, minimized);
    #[cfg(unix)]
    {
        write_entry(&entry_path().ok_or("no home folder")?, on.then_some(text.as_str()))
    }
}

/// The entry file's text, None when missing or switched off in the desktop's startup settings
/// (the file stays then).
#[cfg(unix)]
fn read_entry(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let off = text.lines().any(|l| matches!(l.trim(), "Hidden=true" | "X-GNOME-Autostart-enabled=false"));
    (!off).then_some(text)
}

/// Writes `text` to `path`, or removes the file (`None`; a missing one is fine).
#[cfg(unix)]
fn write_entry(path: &Path, text: Option<&str>) -> Result<(), String> {
    match text {
        Some(t) => crate::config::write_atomic(path, t.as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
    }
}

/// Once per data dir: autostart on (minimized) for an installed copy without an entry. Runs on
/// the preload thread; an explicit choice on the settings page also marks it done.
pub fn default_once(s: &mut crate::config::Settings) {
    let hooks = std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("RUSTCORD_"));
    let debug = cfg!(debug_assertions);
    // Dev builds and test runs never even look at the OS entry.
    if s.autostart_default_done || hooks || debug {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let installed = installed_copy(&exe);
    let existing = installed && entry_exists();
    if should_default(s.autostart_default_done, hooks, debug, installed, existing)
        && let Err(e) = set(true, true)
    {
        eprintln!("rustcord: autostart: {e}");
        return;
    }
    if installed {
        s.autostart_default_done = true;
        if let Err(e) = s.save() {
            eprintln!("rustcord: could not save settings: {e}");
        }
    }
}

/// The one-time default writes the entry only for an installed copy that has none yet.
fn should_default(done: bool, hooks: bool, debug: bool, installed: bool, existing: bool) -> bool {
    !done && !hooks && !debug && installed && !existing
}

/// An entry is there, also one the user switched off (never overwritten).
fn entry_exists() -> bool {
    #[cfg(windows)]
    {
        get().is_some()
    }
    #[cfg(unix)]
    {
        entry_path().is_some_and(|p| p.exists())
    }
}

/// This exe is the installed one: the installer's folder on Windows, a system or per-user
/// install (or an app bundle) elsewhere. Dev builds in a target dir never are.
fn installed_copy(exe: &Path) -> bool {
    #[cfg(windows)]
    {
        let key = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Rustcord";
        let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
        crate::winreg::get_string(key, "InstallLocation").is_some_and(|dir| exe.parent().is_some_and(|p| norm(p) == norm(Path::new(&dir))))
    }
    #[cfg(unix)]
    {
        unix_installed(exe, directories::BaseDirs::new().as_ref().map(|d| d.home_dir()))
    }
}

/// Under `/usr/`, `/opt/` or `~/.local/`, or an app bundle in `/Applications` or
/// `~/Applications` (not the mounted dmg or an App Translocation copy, gone at the next login).
#[cfg(any(unix, test))]
fn unix_installed(exe: &Path, home: Option<&Path>) -> bool {
    let local = home.map(|h| h.join(".local"));
    let apps = exe.starts_with("/Applications") || home.is_some_and(|h| exe.starts_with(h.join("Applications")));
    exe.starts_with("/usr") || exe.starts_with("/opt") || local.is_some_and(|l| exe.starts_with(l)) || (apps && exe.to_string_lossy().contains(".app/Contents/MacOS/"))
}

#[cfg(unix)]
fn entry_path() -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    Some(if cfg!(target_os = "macos") {
        dirs.home_dir().join("Library/LaunchAgents").join(format!("{BUNDLE_ID}.plist"))
    } else {
        dirs.config_dir().join("autostart/rustcord.desktop")
    })
}

/// The XDG autostart file. `TryExec` makes desktops skip it once the binary is gone.
#[cfg(any(target_os = "linux", test))]
fn desktop_entry(exe: &Path, minimized: bool) -> String {
    let path = exe.to_string_lossy();
    // Exec: a quoted argument (`"`, `` ` ``, `$`, `\` escaped), then the string escape, `%` doubled.
    let quoted: String = path.chars().flat_map(|c| if matches!(c, '"' | '`' | '$' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
    let exec = format!("\"{}\"{}", string_escape(&quoted).replace('%', "%%"), if minimized { " --minimized" } else { "" });
    format!(
        "[Desktop Entry]\nType=Application\nName=Rustcord\nComment=Start Rustcord when you log in\nExec={exec}\nTryExec={}\nIcon=rustcord\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        string_escape(&path)
    )
}

/// Desktop Entry string escape: `\` doubled, line breaks and tabs as `\n` `\r` `\t`.
#[cfg(any(target_os = "linux", test))]
fn string_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('\n', r"\n").replace('\r', r"\r").replace('\t', r"\t")
}

/// The LaunchAgent property list.
#[cfg(any(target_os = "macos", test))]
fn launch_agent(exe: &Path, minimized: bool) -> String {
    let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let min = if minimized { "<string>--minimized</string>" } else { "" };
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\"><dict>\n",
            "  <key>Label</key><string>{id}</string>\n",
            "  <key>ProgramArguments</key><array><string>{exe}</string>{min}</array>\n",
            "  <key>RunAtLoad</key><true/>\n",
            "  <key>ProcessType</key><string>Interactive</string>\n",
            "</dict></plist>\n"
        ),
        id = BUNDLE_ID,
        exe = xml(&exe.to_string_lossy()),
        min = min
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_quotes_exec() {
        let e = desktop_entry(Path::new("/usr/bin/rustcord"), true);
        assert!(e.contains("\nExec=\"/usr/bin/rustcord\" --minimized\n") && e.contains("\nTryExec=/usr/bin/rustcord\n"), "{e}");
        assert!(e.starts_with("[Desktop Entry]\nType=Application\n") && e.contains("\nX-GNOME-Autostart-enabled=true\n"));
        assert!(desktop_entry(Path::new("/usr/bin/rustcord"), false).contains("\nExec=\"/usr/bin/rustcord\"\n"));
        // Quoting first (\ " ` $ get a backslash), then the string escape doubles every backslash.
        let odd = desktop_entry(Path::new("/home/a b/$x\"y`z\\w%/rustcord"), false);
        assert!(odd.contains(r#"Exec="/home/a b/\\$x\\"y\\`z\\\\w%%/rustcord""#), "{odd}");
        assert!(odd.contains(r#"TryExec=/home/a b/$x"y`z\\w%/rustcord"#), "{odd}");
        // A line break can't start a new key.
        assert!(!desktop_entry(Path::new("/tmp/a\nHidden=true/rustcord"), false).contains("\nHidden=true"));
    }

    #[test]
    fn launch_agent_escapes_xml() {
        let p = launch_agent(Path::new("/Applications/R&D <x>.app/Contents/MacOS/rustcord"), true);
        assert!(p.contains("<string>/Applications/R&amp;D &lt;x&gt;.app/Contents/MacOS/rustcord</string><string>--minimized</string>"), "{p}");
        assert!(p.contains("<key>Label</key><string>io.github.r0sc.rustcord</string>") && p.contains("<key>RunAtLoad</key><true/>"));
        assert!(!launch_agent(Path::new("/Applications/Rustcord.app/Contents/MacOS/rustcord"), false).contains("--minimized"));
    }

    #[test]
    fn default_once_rules() {
        assert!(should_default(false, false, false, true, false));
        for (done, hooks, debug, installed, existing) in
            [(true, false, false, true, false), (false, true, false, true, false), (false, false, true, true, false), (false, false, false, false, false), (false, false, false, true, true)]
        {
            assert!(!should_default(done, hooks, debug, installed, existing), "{done} {hooks} {debug} {installed} {existing}");
        }
    }

    #[test]
    fn installed_copy_paths() {
        let home = Some(Path::new("/home/ann"));
        assert!(unix_installed(Path::new("/usr/bin/rustcord"), home));
        assert!(unix_installed(Path::new("/opt/rustcord/rustcord"), home));
        assert!(unix_installed(Path::new("/home/ann/.local/bin/rustcord"), home));
        assert!(unix_installed(Path::new("/Applications/Rustcord.app/Contents/MacOS/rustcord"), None));
        assert!(unix_installed(Path::new("/Users/ann/Applications/Rustcord.app/Contents/MacOS/rustcord"), Some(Path::new("/Users/ann"))));
        assert!(!unix_installed(Path::new("/Volumes/Rustcord/Rustcord.app/Contents/MacOS/rustcord"), home), "the mounted dmg");
        assert!(!unix_installed(Path::new("/private/var/folders/x/AppTranslocation/y/d/Rustcord.app/Contents/MacOS/rustcord"), home));
        assert!(!unix_installed(Path::new("/Applications/rustcord"), None), "a bundle only");
        assert!(!unix_installed(Path::new("/home/ann/rc-target/release/rustcord"), home));
        assert!(!unix_installed(Path::new("/usrx/rustcord"), home), "whole folder names only");
        assert!(!unix_installed(Path::new("/home/ann/.localx/rustcord"), home));
    }

    /// A temp dir only, never the real ~/.config/autostart.
    #[cfg(unix)]
    #[test]
    fn autostart_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("rc-autostart-{}", std::process::id()));
        let path = dir.join("autostart/rustcord.desktop");
        let entry = desktop_entry(Path::new("/usr/bin/rustcord"), true);
        assert_eq!(read_entry(&path), None);
        write_entry(&path, Some(&entry)).unwrap();
        assert_eq!(read_entry(&path).as_deref(), Some(entry.as_str()));
        // Switched off by the desktop: off, but the file is still there.
        write_entry(&path, Some(&format!("{entry}Hidden=true\n"))).unwrap();
        assert!(read_entry(&path).is_none() && path.exists());
        write_entry(&path, None).unwrap();
        write_entry(&path, None).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
