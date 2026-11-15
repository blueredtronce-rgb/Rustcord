//! What this copy is and how it installs a package (docs/UPDATER_DESIGN.md §6 and §9, with the
//! design review's changes): detection from the exe path, the apply per platform with a
//! deadline per step, and the leftovers of the previous version.

use super::feed::{self, Key};
use super::machine::{ApplyError, MSG_NO_PASSWORD};
use super::state::Kind;
use super::store::{self, Staged};
use semver::Version;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// tar, install.sh, open and the Windows setup.
const STEP_WAIT: Duration = Duration::from_secs(120);
/// pkexec: a human types the password.
const PKEXEC_WAIT: Duration = Duration::from_secs(600);
const PKEXEC: &str = "/usr/bin/pkexec";
const APT_GET: &str = "/usr/bin/apt-get";
/// The Windows setup's test identity (its Apps entry name).
#[cfg(windows)]
const TEST_IDENTITY: &str = "RustcordSetupTest";

/// This copy, captured once at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub can: Kind,
    pub key: Option<Key>,
    /// Linux: `current_exe()` reads "… (deleted)" once install.sh replaced the binary.
    pub exe: PathBuf,
    /// Windows: the setup's test identity owns this copy (`--test-identity`).
    pub test_identity: bool,
}

/// How the new version gets started.
pub enum Relaunch {
    /// It already runs and waits for this one's instance lock (macOS `open`).
    Started,
    /// Windows, Linux: started once this one quit, which waits for a call to end
    /// (`update::relaunch`, which also adds `--minimized`).
    Later(Command),
}

#[cfg(any(unix, test))]
fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// §6. `test`: Test mode (also the Windows test identity, any macOS folder); `prefix`:
/// `RUSTCORD_SETUP_PREFIX` with `RUSTCORD_SETUP_TEST=1` (Linux tarball install root).
pub fn detect(exe: PathBuf, test: bool, prefix: Option<&Path>) -> Install {
    let mut install = Install { can: Kind::NoticeOnly, key: None, exe, test_identity: false };
    // Only x86_64 packages exist for Windows and Linux; the macOS one is universal.
    #[cfg(windows)]
    if cfg!(target_arch = "x86_64") {
        let id = if crate::autostart::in_install_dir(&install.exe, crate::config::APP_NAME) {
            Some(false)
        } else {
            (test && crate::autostart::in_install_dir(&install.exe, TEST_IDENTITY)).then_some(true)
        };
        if let Some(test_identity) = id {
            install = Install { can: Kind::Auto, key: Some(Key::native(false)), test_identity, ..install };
        }
    }
    #[cfg(target_os = "linux")]
    if cfg!(target_arch = "x86_64") {
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        let root = if test && prefix.is_some() { prefix.map(Path::to_path_buf) } else { home };
        let dpkg = Path::new("/var/lib/dpkg/info/rustcord.list").exists();
        if let Some(can) = linux_kind(&install.exe, root.as_deref(), dpkg) {
            install = Install { can, key: Some(Key::native(can == Kind::Package)), ..install };
        }
    }
    #[cfg(target_os = "macos")]
    if let Some((_, parent)) = bundle_of(&canon(&install.exe)) {
        let home = directories::BaseDirs::new().map(|d| canon(d.home_dir()));
        let installed = test || crate::autostart::unix_installed(&canon(&install.exe), home.as_deref());
        let c = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes()).unwrap_or_default();
        // SAFETY: a NUL-terminated path.
        let writable = unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0;
        if installed && writable {
            install = Install { can: Kind::Auto, key: Some(Key::native(false)), ..install };
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = prefix;
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = test;
    install
}

/// `<root>/.local/bin/rustcord` (install.sh) or the .deb's `/usr/bin/rustcord`. Both sides
/// canonical: a symlinked home (`/home -> /data/home`) is still the same file.
#[cfg(any(target_os = "linux", test))]
fn linux_kind(exe: &Path, root: Option<&Path>, dpkg: bool) -> Option<Kind> {
    let exe = canon(exe);
    if root.is_some_and(|r| canon(&r.join(".local/bin/rustcord")) == exe) {
        Some(Kind::Auto)
    } else if exe == Path::new("/usr/bin/rustcord") && dpkg {
        Some(Kind::Package)
    } else {
        None
    }
}

/// `<parent>/<Name>.app/Contents/MacOS/<exe>` -> (bundle, parent).
#[cfg(any(target_os = "macos", test))]
fn bundle_of(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    let macos = exe.parent().filter(|p| p.file_name().is_some_and(|n| n == "MacOS"))?;
    let contents = macos.parent().filter(|p| p.file_name().is_some_and(|n| n == "Contents"))?;
    let bundle = contents.parent().filter(|p| p.extension().is_some_and(|e| e == "app"))?;
    Some((bundle.to_path_buf(), bundle.parent()?.to_path_buf()))
}

/// The macOS swap folders next to the bundle (same volume: renames are atomic).
fn swap_dirs(parent: &Path, new: &Version, running: &Version) -> (PathBuf, PathBuf) {
    (parent.join(format!(".Rustcord-update-{new}")), parent.join(format!(".Rustcord-old-{running}")))
}

/// After a successful update: the previous version's leftovers (only these names).
pub fn cleanup_after_update(exe: &Path) {
    #[cfg(windows)]
    let _ = std::fs::remove_file(exe.with_file_name("rustcord.exe.old"));
    #[cfg(target_os = "macos")]
    if let Some((_, parent)) = bundle_of(&canon(exe)) {
        for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".Rustcord-old-") || name.starts_with(".Rustcord-update-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = exe;
}

/// Windows: the installed folder must be writable (an elevated `--dir` install is not).
#[cfg(windows)]
pub fn writable(install: &Install) -> bool {
    let Some(dir) = install.exe.parent() else { return false };
    let probe = dir.join(format!(".rustcord-update-probe-{}", std::process::id()));
    let ok = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[derive(Debug)]
enum RunError {
    Start(std::io::Error),
    Timeout,
}

/// Runs `cmd` without a shell (stdin/stdout null; stderr kept when `stderr`) and waits at most
/// `wait`; then the child is killed. Ok: the exit code and the last stderr line.
fn run(mut cmd: Command, wait: Duration, stderr: bool) -> Result<(Option<i32>, String), RunError> {
    cmd.env_remove("RUSTCORD_UPDATE_FROM").stdin(Stdio::null()).stdout(Stdio::null());
    cmd.stderr(if stderr { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().map_err(RunError::Start)?;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut err) = child.stderr.take() {
        // Read on the side so a chatty child never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
    }
    let end = Instant::now() + wait;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
            return Ok((status.code(), last.chars().take(200).collect()));
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn failed(why: &str) -> ApplyError {
    ApplyError::Failed { message: format!("Could not install the update: {}.", why.trim_end_matches('.')), block: true }
}

/// A step that must exit 0; anything else is a failed install with the reason.
fn step(result: Result<(Option<i32>, String), RunError>) -> Result<(), ApplyError> {
    match result {
        Ok((Some(0), _)) => Ok(()),
        Ok((code, line)) if line.is_empty() => Err(failed(&format!("exit code {}", code.unwrap_or(-1)))),
        Ok((_, line)) => Err(failed(&line)),
        Err(RunError::Start(e)) => Err(failed(&e.to_string())),
        Err(RunError::Timeout) => Err(failed("it took too long")),
    }
}

fn relaunch_command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg("--after-update");
    cmd.env_remove("RUSTCORD_UPDATE_FROM").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

/// §9: re-hash the staged file, then install it. `hidden` is read right before macOS starts
/// the new version (`--minimized`). Blocking: runs in `spawn_blocking`.
pub fn apply(install: &Install, dir: &Path, staged: &Staged, running: &Version, hidden: &dyn Fn() -> bool) -> Result<Relaunch, ApplyError> {
    let key = install.key.ok_or(ApplyError::Damaged)?;
    let v = &staged.version;
    let file = dir.join(key.file(v));
    if !feed::file_matches(&file, staged.size, &staged.sha256) {
        return Err(ApplyError::Damaged);
    }
    match key {
        Key::Windows => windows(dir, &file, install.test_identity).map(|()| Relaunch::Later(relaunch_command(&install.exe))),
        Key::Linux => {
            let extract = dir.join(format!("extract-{v}"));
            let result = tarball(&file, &extract, v);
            let _ = std::fs::remove_dir_all(&extract);
            result.map(|()| Relaunch::Later(relaunch_command(&install.exe)))
        }
        Key::LinuxDeb => deb(&file),
        Key::Macos => macos(&install.exe, &file, v, running, hidden),
    }
}

/// The new setup swaps the files (§11) while this one waits for its exit code with the
/// Installing screen up; the new app is started once this one quit, never during a call.
fn windows(dir: &Path, setup: &Path, test_identity: bool) -> Result<(), ApplyError> {
    let _ = std::fs::remove_file(dir.join("setup-error.txt"));
    let mut cmd = Command::new(setup);
    cmd.arg("--update").args(test_identity.then_some("--test-identity"));
    match run(cmd, STEP_WAIT, false) {
        Ok((Some(0), _)) => Ok(()),
        Ok((code, _)) => Err(failed(&store::take_setup_error(dir).unwrap_or_else(|| format!("the setup ended with exit code {}", code.unwrap_or(-1))))),
        Err(RunError::Start(e)) => Err(ApplyError::Failed { message: format!("Could not start the update: {}.", e.to_string().trim_end_matches('.')), block: true }),
        Err(RunError::Timeout) => Err(failed("the setup did not finish")),
    }
}

/// §9.2: unpack, then the package's own `install.sh --update`.
fn tarball(file: &Path, extract: &Path, v: &Version) -> Result<(), ApplyError> {
    let _ = std::fs::remove_dir_all(extract);
    std::fs::create_dir_all(extract).map_err(|e| failed(&e.to_string()))?;
    let tar = ["/usr/bin/tar", "/bin/tar"].into_iter().find(|p| Path::new(p).exists()).ok_or_else(|| failed("tar was not found"))?;
    let mut cmd = Command::new(tar);
    cmd.arg("-xzf").arg(file).arg("-C").arg(extract);
    step(run(cmd, STEP_WAIT, true))?;
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(extract.join(format!("rustcord-{v}")).join("install.sh")).arg("--update");
    step(run(cmd, STEP_WAIT, true))
}

/// §9.3: apt-get through polkit's password dialog. A package is never blocked: every try is a
/// click.
/// /usr/bin/pkexec and /usr/bin/apt-get are there (else a .deb is only saved: `Manual`).
pub fn deb_tools() -> bool {
    Path::new(PKEXEC).exists() && Path::new(APT_GET).exists()
}

fn deb(file: &Path) -> Result<Relaunch, ApplyError> {
    if !deb_tools() {
        return Err(ApplyError::Manual);
    }
    let mut cmd = Command::new(PKEXEC);
    // The lock timeout waits out unattended-upgrades instead of failing.
    cmd.args([APT_GET, "-o", "DPkg::Lock::Timeout=120", "install", "-y"]).arg(file);
    deb_result(run(cmd, PKEXEC_WAIT, false)).map(|()| Relaunch::Later(relaunch_command(Path::new("/usr/bin/rustcord"))))
}

fn deb_result(result: Result<(Option<i32>, String), RunError>) -> Result<(), ApplyError> {
    let not_blocked = |message: String| ApplyError::Failed { message, block: false };
    match result {
        Ok((Some(0), _)) => Ok(()),
        // The dialog was dismissed.
        Ok((Some(126), _)) => Err(not_blocked(MSG_NO_PASSWORD.into())),
        // Not authorized or no polkit agent: retrying can't help.
        Ok((Some(127), _)) | Err(RunError::Start(_)) => Err(ApplyError::Manual),
        Ok((code, _)) => Err(not_blocked(format!("apt-get could not install the update (exit code {}).", code.unwrap_or(-1)))),
        Err(RunError::Timeout) => Err(not_blocked(MSG_NO_PASSWORD.into())),
    }
}

/// §9.4: unpack next to the bundle, swap (the old one kept until the new one started), open.
fn macos(exe: &Path, file: &Path, v: &Version, running: &Version, hidden: &dyn Fn() -> bool) -> Result<Relaunch, ApplyError> {
    let (bundle, parent) = bundle_of_exe(exe).ok_or_else(|| failed("Rustcord is not in an app bundle"))?;
    let (work, old) = swap_dirs(&parent, v, running);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&old);
    let result = swap_and_open(&bundle, file, &work, &old, hidden);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn swap_and_open(bundle: &Path, file: &Path, work: &Path, old: &Path, hidden: &dyn Fn() -> bool) -> Result<Relaunch, ApplyError> {
    std::fs::create_dir_all(work).map_err(|e| failed(&e.to_string()))?;
    let mut cmd = Command::new("/usr/bin/tar");
    cmd.arg("-xzf").arg(file).arg("-C").arg(work);
    step(run(cmd, STEP_WAIT, true))?;
    let new = work.join("Rustcord.app");
    if !new.join("Contents/MacOS/rustcord").is_file() {
        return Err(failed("the package holds no Rustcord.app"));
    }
    std::fs::rename(bundle, old).map_err(|e| failed(&e.to_string()))?;
    if let Err(e) = std::fs::rename(&new, bundle) {
        let _ = std::fs::rename(old, bundle);
        return Err(failed(&e.to_string()));
    }
    let minimized = hidden();
    let mut cmd = Command::new("/usr/bin/open");
    // -n: a new instance (else LaunchServices activates this one); -g: stay in the background.
    cmd.arg("-n").args(minimized.then_some("-g")).arg(bundle).arg("--args").arg("--after-update").args(minimized.then_some("--minimized"));
    if let Err(e) = step(run(cmd, STEP_WAIT, false)) {
        let _ = std::fs::rename(bundle, &new);
        let _ = std::fs::rename(old, bundle);
        return Err(e);
    }
    Ok(Relaunch::Started)
}

fn bundle_of_exe(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    #[cfg(any(target_os = "macos", test))]
    {
        bundle_of(&canon(exe))
    }
    #[cfg(not(any(target_os = "macos", test)))]
    {
        let _ = exe;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_install_kinds() {
        let home = Some(Path::new("/home/ann"));
        assert_eq!(linux_kind(Path::new("/home/ann/.local/bin/rustcord"), home, false), Some(Kind::Auto));
        assert_eq!(linux_kind(Path::new("/usr/bin/rustcord"), home, true), Some(Kind::Package));
        assert_eq!(linux_kind(Path::new("/usr/bin/rustcord"), home, false), None, "not from the .deb");
        assert_eq!(linux_kind(Path::new("/opt/rustcord/rustcord"), home, true), None);
        assert_eq!(linux_kind(Path::new("/home/ann/rc-target/release/rustcord"), home, true), None);
        assert_eq!(linux_kind(Path::new("/home/ann/.local/bin/rustcord"), None, false), None);
        // The test prefix (RUSTCORD_SETUP_PREFIX) is the root in Test mode.
        assert_eq!(linux_kind(Path::new("/tmp/p/.local/bin/rustcord"), Some(Path::new("/tmp/p")), false), Some(Kind::Auto));
        let native = [Key::native(false), Key::native(true)];
        let expected = match std::env::consts::OS {
            "windows" => [Key::Windows; 2],
            "macos" => [Key::Macos; 2],
            _ => [Key::Linux, Key::LinuxDeb],
        };
        assert_eq!(native, expected);
    }

    /// A symlinked home is the same install (canonical paths on both sides).
    #[cfg(unix)]
    #[test]
    fn linux_kind_follows_symlinks() {
        let base = std::env::temp_dir().join(format!("rc-update-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data/ann/.local/bin")).unwrap();
        std::fs::write(base.join("data/ann/.local/bin/rustcord"), "").unwrap();
        std::os::unix::fs::symlink(base.join("data"), base.join("home")).unwrap();
        let real = base.join("data/ann/.local/bin/rustcord");
        assert_eq!(linux_kind(&real, Some(&base.join("home/ann")), false), Some(Kind::Auto));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_bundle_and_swap_paths() {
        let exe = Path::new("/Applications/Rustcord.app/Contents/MacOS/rustcord");
        assert_eq!(bundle_of(exe), Some((PathBuf::from("/Applications/Rustcord.app"), PathBuf::from("/Applications"))));
        assert_eq!(bundle_of(Path::new("/Users/a/Applications/R 2.app/Contents/MacOS/rustcord")).unwrap().1, PathBuf::from("/Users/a/Applications"));
        assert_eq!(bundle_of(Path::new("/usr/bin/rustcord")), None);
        assert_eq!(bundle_of(Path::new("/Applications/Rustcord/Contents/MacOS/rustcord")), None, "no .app");
        assert_eq!(bundle_of(Path::new("/x/Rustcord.app/Contents/Resources/rustcord")), None);
        let (work, old) = swap_dirs(Path::new("/Applications"), &Version::new(0, 1, 1), &Version::new(0, 1, 0));
        assert_eq!((work, old), (PathBuf::from("/Applications/.Rustcord-update-0.1.1"), PathBuf::from("/Applications/.Rustcord-old-0.1.0")));
    }

    #[test]
    fn deb_exit_codes() {
        assert!(deb_result(Ok((Some(0), String::new()))).is_ok());
        assert_eq!(deb_result(Ok((Some(126), String::new()))), Err(ApplyError::Failed { message: MSG_NO_PASSWORD.into(), block: false }));
        assert_eq!(deb_result(Ok((Some(127), String::new()))), Err(ApplyError::Manual));
        assert_eq!(deb_result(Err(RunError::Start(std::io::ErrorKind::NotFound.into()))), Err(ApplyError::Manual));
        assert_eq!(
            deb_result(Ok((Some(100), "E: lock".into()))),
            Err(ApplyError::Failed { message: "apt-get could not install the update (exit code 100).".into(), block: false })
        );
    }

    #[test]
    fn step_messages() {
        assert!(step(Ok((Some(0), "noise".into()))).is_ok());
        let msg = |r| match step(r) {
            Err(ApplyError::Failed { message, block: true }) => message,
            other => panic!("{:?}", other.err()),
        };
        assert_eq!(msg(Ok((Some(1), "install.sh: nothing to update.".into()))), "Could not install the update: install.sh: nothing to update.");
        assert_eq!(msg(Ok((Some(2), String::new()))), "Could not install the update: exit code 2.");
        assert_eq!(msg(Ok((None, String::new()))), "Could not install the update: exit code -1.");
        assert_eq!(msg(Err(RunError::Timeout)), "Could not install the update: it took too long.");
    }

    /// The runner: exit code, the last stderr line, the deadline (a real child process).
    #[cfg(unix)]
    #[test]
    fn run_waits_and_kills() {
        let sh = |script: &str| {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(script);
            c
        };
        let (code, line) = run(sh("echo one >&2; echo '  two  ' >&2; echo; exit 3"), Duration::from_secs(10), true).unwrap();
        assert_eq!((code, line.as_str()), (Some(3), "two"));
        let started = Instant::now();
        assert!(matches!(run(sh("sleep 30"), Duration::from_millis(300), false), Err(RunError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(run(Command::new("/nonexistent/x"), Duration::from_secs(1), false), Err(RunError::Start(_))));
        // The pretend version never reaches a child.
        let (code, _) = run(sh("test -z \"$RUSTCORD_UPDATE_FROM\""), Duration::from_secs(10), false).unwrap();
        assert_eq!(code, Some(0));
    }

    /// The tarball apply end to end against a fake package (unix, a temp dir only).
    #[cfg(unix)]
    #[test]
    fn tarball_runs_install_sh() {
        let base = std::env::temp_dir().join(format!("rc-update-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src/rustcord-0.1.1");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("install.sh"), "[ \"$1\" = --update ] || exit 9\necho ran > \"$(dirname \"$0\")/../../done\"\n").unwrap();
        let tgz = base.join("pkg.tar.gz");
        let ok = Command::new("tar").arg("-czf").arg(&tgz).arg("-C").arg(base.join("src")).arg("rustcord-0.1.1").status().unwrap();
        assert!(ok.success());
        let extract = base.join("extract-0.1.1");
        tarball(&tgz, &extract, &Version::new(0, 1, 1)).unwrap();
        assert!(base.join("done").exists());
        std::fs::write(src.join("install.sh"), "echo 'install.sh: nothing to update' >&2; exit 1\n").unwrap();
        Command::new("tar").arg("-czf").arg(&tgz).arg("-C").arg(base.join("src")).arg("rustcord-0.1.1").status().unwrap();
        assert_eq!(tarball(&tgz, &extract, &Version::new(0, 1, 1)), Err(failed("install.sh: nothing to update")));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
