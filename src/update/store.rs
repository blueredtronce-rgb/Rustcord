//! `<cache>/update/`: `state.toml` and the folder's files (docs/UPDATER_DESIGN.md §7).

use super::feed::{self, MAX_SIZE};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn dir() -> PathBuf {
    crate::config::paths().cache_dir.join("update")
}

/// What the updater remembers between runs. Read from a user-writable file, so every value is
/// validated on load and an invalid one counts as absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    /// Apply started for this version; the next start checks it (loop guard).
    pub attempted: Option<Version>,
    /// Did not install: never applied automatically again (a click still may).
    pub blocked: Option<Version>,
    /// SHA-256 of a `latest.json` whose package failed its hash.
    pub bad_manifest: Option<[u8; 32]>,
    pub staged: Option<Staged>,
}

/// The verified package waiting in the folder. Its file name is never stored: it follows
/// from the version and this copy's platform key (`Key::file`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub version: Version,
    pub sha256: [u8; 32],
    pub size: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    attempted: Option<String>,
    blocked: Option<String>,
    bad_manifest: Option<String>,
    staged: Option<FileStaged>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FileStaged {
    version: String,
    sha256: String,
    size: u64,
}

impl Saved {
    pub fn load(path: &Path) -> Self {
        crate::config::read_toml::<File>(path).map(Self::from_file).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        crate::config::write_toml(path, &self.to_file())
    }

    fn from_file(f: File) -> Self {
        let v = |s: Option<String>| s.as_deref().and_then(feed::plain_version);
        let staged = f.staged.filter(|s| (1..=MAX_SIZE).contains(&s.size)).and_then(|s| {
            Some(Staged { version: feed::plain_version(&s.version)?, sha256: feed::parse_hex32(&s.sha256)?, size: s.size })
        });
        Self { attempted: v(f.attempted), blocked: v(f.blocked), bad_manifest: f.bad_manifest.as_deref().and_then(feed::parse_hex32), staged }
    }

    fn to_file(&self) -> File {
        let s = |v: &Option<Version>| v.as_ref().map(|v| v.to_string());
        File {
            attempted: s(&self.attempted),
            blocked: s(&self.blocked),
            bad_manifest: self.bad_manifest.map(|h| feed::hex(&h)),
            staged: self.staged.as_ref().map(|st| FileStaged { version: st.version.to_string(), sha256: feed::hex(&st.sha256), size: st.size }),
        }
    }
}

/// Removes everything in the updater folder but `state.toml` and `keep` (the staged file):
/// `.part` downloads, `extract-*` folders, replaced packages, the setup's error file. Only
/// while nothing else writes there: it would delete `state.tmp` or an extract under it.
pub fn prune(dir: &Path, keep: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        if name == "state.toml" || keep.is_some_and(|k| name == k) {
            continue;
        }
        let path = e.path();
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}

/// The first line of the Windows setup's `setup-error.txt` (at most 200 chars), then the file
/// is deleted.
pub fn take_setup_error(dir: &Path) -> Option<String> {
    let path = dir.join("setup-error.txt");
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let line: String = text?.lines().next()?.trim().chars().take(200).collect();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-update-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn state_round_trip_and_validation() {
        let dir = temp("state");
        let path = dir.join("state.toml");
        assert_eq!(Saved::load(&path), Saved::default());
        let s = Saved {
            attempted: Some(Version::new(0, 1, 1)),
            blocked: Some(Version::new(0, 1, 2)),
            bad_manifest: Some([7; 32]),
            staged: Some(Staged { version: Version::new(0, 2, 0), sha256: [0xab; 32], size: 42 }),
        };
        s.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("attempted = \"0.1.1\"") && text.contains("[staged]") && !text.contains("file"), "{text}");
        assert_eq!(Saved::load(&path), s);
        Saved::default().save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "");
        // Hand-edited or damaged values count as absent; a path never comes from the file.
        std::fs::write(
            &path,
            "attempted = \"../../x\"\nblocked = \"0.1.0-beta\"\nbad_manifest = \"zz\"\n[staged]\nversion = \"0.2.0\"\nfile = \"../../evil.exe\"\nsha256 = \"abab\"\nsize = 5\n",
        )
        .unwrap();
        assert_eq!(Saved::load(&path), Saved::default());
        std::fs::write(&path, format!("[staged]\nversion = \"0.2.0\"\nsha256 = \"{}\"\nsize = 0\n", "ab".repeat(32))).unwrap();
        assert_eq!(Saved::load(&path).staged, None, "size 0");
        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Saved::load(&path), Saved::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_keeps_state_and_the_staged_file() {
        let dir = temp("prune");
        std::fs::create_dir_all(dir.join("extract-0.1.1/rustcord-0.1.1")).unwrap();
        for f in ["state.toml", "Rustcord-Setup-0.1.1.exe", "Rustcord-Setup-0.1.0.exe", "x.part", "setup-error.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        prune(&dir, Some("Rustcord-Setup-0.1.1.exe"));
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Rustcord-Setup-0.1.1.exe", "state.toml"]);
        prune(&dir, None);
        assert!(!dir.join("Rustcord-Setup-0.1.1.exe").exists() && dir.join("state.toml").exists());
        prune(&dir.join("missing"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setup_error_is_read_once() {
        let dir = temp("setup-error");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(take_setup_error(&dir), None);
        std::fs::write(dir.join("setup-error.txt"), format!("  disk full {}\nsecond line\n", "x".repeat(300))).unwrap();
        let line = take_setup_error(&dir).unwrap();
        assert!(line.starts_with("disk full x") && line.chars().count() == 200, "{line}");
        assert!(!dir.join("setup-error.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
