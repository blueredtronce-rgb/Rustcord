//! The detectable-games list (`GET /games/detectable`) as a compact table: every application
//! with a Windows executable or an icon (id, name, icon hash), and its Windows executables
//! sorted by file name. Parsed straight from the JSON body (one pass, no intermediate list),
//! kept on disk as a small TSV whose modification time says when it was fetched.

use crate::model::Id;
use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use std::borrow::Cow;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// The list is fetched again after this long (at most once a day).
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);
const HEADER: &str = "#rustcord detectable 1";

pub struct App {
    pub id: Id,
    pub name: Box<str>,
    /// The icon hash (32 hex digits) as a number; 0 = none.
    icon: u128,
}

/// A Windows executable of an app: a lowercase path suffix like `factorio.exe` or
/// `_retail_/wow.exe`.
struct Exe {
    suffix: Box<str>,
    app: u32,
}

impl Exe {
    fn file(&self) -> &str {
        file_name(&self.suffix)
    }
}

#[derive(Default)]
pub struct Table {
    /// Sorted by id.
    apps: Vec<App>,
    /// Sorted by file name.
    exes: Vec<Exe>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Table({} apps, {} executables)", self.len(), self.exes.len())
    }
}

impl Table {
    pub fn app(&self, id: Id) -> Option<&App> {
        self.apps.binary_search_by_key(&id, |a| a.id).ok().map(|i| &self.apps[i])
    }

    /// Icon hash of application `id`, if the list has one.
    pub fn icon(&self, id: Id) -> Option<String> {
        self.app(id).filter(|a| a.icon != 0).map(|a| format!("{:032x}", a.icon))
    }

    /// Test hook (`RUSTCORD_FAKE_GAME`): an app by its exact name.
    pub fn by_name(&self, name: &str) -> Option<&App> {
        self.apps.iter().find(|a| &*a.name == name)
    }

    /// Executables named `file` (lowercase file name).
    fn named<'a>(&'a self, file: &'a str) -> impl Iterator<Item = &'a Exe> + 'a {
        let start = self.exes.partition_point(|e| e.file() < file);
        self.exes[start..].iter().take_while(move |e| e.file() == file)
    }

    /// Whether a process with this (lowercase) file name can be a game: only those get their
    /// full path looked up.
    pub fn has_file(&self, file: &str) -> bool {
        self.named(file).next().is_some()
    }

    /// The app whose executable `path` (`normalize`d) is: one of its suffixes ends the path at
    /// a folder boundary.
    pub fn find(&self, path: &str) -> Option<&App> {
        let exe = self.named(file_name(path)).find(|e| suffix_of(path, &e.suffix))?;
        self.apps.get(exe.app as usize)
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    /// Parses the API's JSON array. Windows executables only; launchers and `>` entries (those
    /// need the command line, which Rustcord doesn't read) are left out.
    pub fn from_json(bytes: &[u8]) -> Result<Table, String> {
        struct Build;
        impl<'de> Visitor<'de> for Build {
            type Value = Table;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of applications")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Table, A::Error> {
                let mut t = Table::default();
                while let Some(w) = seq.next_element::<WireApp<'de>>()? {
                    let win = |e: &WireExe| e.os.as_deref() == Some("win32") && e.is_launcher != Some(true);
                    let exes: Vec<Cow<str>> = w.executables.into_iter().flatten().filter(win).filter_map(|e| e.name).collect();
                    if !w.id.is_zero() {
                        t.push(w.id, w.name.as_deref().unwrap_or_default(), w.icon_hash.as_deref(), exes.iter().map(|e| e.as_ref()));
                    }
                }
                Ok(t.finish())
            }
        }
        use serde::Deserializer as _;
        serde_json::Deserializer::from_slice(bytes).deserialize_seq(Build).map_err(|e| format!("detectable list: {e}"))
    }

    /// One line per app: id, icon hash, name, executables (tab-separated).
    pub fn to_tsv(&self) -> String {
        let mut exes: Vec<Vec<&str>> = vec![Vec::new(); self.apps.len()];
        for e in &self.exes {
            exes[e.app as usize].push(&e.suffix);
        }
        let mut out = String::with_capacity(self.apps.len() * 64);
        out.push_str(HEADER);
        out.push('\n');
        for (a, exes) in self.apps.iter().zip(exes) {
            let icon = if a.icon == 0 { String::new() } else { format!("{:032x}", a.icon) };
            out.push_str(&format!("{}\t{icon}\t{}", a.id, a.name));
            for e in exes {
                out.push('\t');
                out.push_str(e);
            }
            out.push('\n');
        }
        out
    }

    /// `to_tsv` read back; None for another format.
    pub fn from_tsv(text: &str) -> Option<Table> {
        let mut lines = text.lines();
        if lines.next()? != HEADER {
            return None;
        }
        let mut t = Table::default();
        for line in lines {
            let mut f = line.split('\t');
            let (Some(id), Some(icon), Some(name)) = (f.next().and_then(|s| s.parse().ok()), f.next(), f.next()) else { continue };
            t.push(Id(id), name, Some(icon), f);
        }
        Some(t.finish())
    }

    /// The table saved on disk, if there is one.
    pub fn load(path: &Path) -> Option<Table> {
        Table::from_tsv(&std::fs::read_to_string(path).ok()?)
    }

    pub fn save(&self, path: &Path) {
        if let Err(e) = crate::config::write_atomic(path, self.to_tsv().as_bytes()) {
            eprintln!("rustcord: could not save the games list: {e}");
        }
    }

    fn push<'a>(&mut self, id: Id, name: &str, icon: Option<&str>, exes: impl Iterator<Item = &'a str>) {
        let icon = icon.filter(|h| h.len() == 32).and_then(|h| u128::from_str_radix(h, 16).ok()).unwrap_or(0);
        let app = self.apps.len() as u32;
        let before = self.exes.len();
        // Names and paths go into a tab-separated file: control characters never do.
        let clean = |s: &str| !s.is_empty() && !s.chars().any(char::is_control) && !s.starts_with('>');
        self.exes.extend(exes.filter(|e| clean(e)).map(|e| Exe { suffix: e.to_lowercase().into(), app }));
        if self.exes.len() > before || icon != 0 {
            self.apps.push(App { id, name: name.replace(char::is_control, " ").into(), icon });
        }
    }

    /// Sorts both lists (the exes keep pointing at their app).
    fn finish(mut self) -> Table {
        let mut order: Vec<u32> = (0..self.apps.len() as u32).collect();
        order.sort_by_key(|&i| self.apps[i as usize].id);
        let mut new_index = vec![0u32; order.len()];
        for (new, &old) in order.iter().enumerate() {
            new_index[old as usize] = new as u32;
        }
        let mut apps: Vec<Option<App>> = self.apps.into_iter().map(Some).collect();
        self.apps = order.iter().filter_map(|&i| apps[i as usize].take()).collect();
        for e in &mut self.exes {
            e.app = new_index[e.app as usize];
        }
        self.exes.sort_by(|a, b| a.file().cmp(b.file()));
        self.apps.shrink_to_fit();
        self.exes.shrink_to_fit();
        self
    }
}

// Every field may be missing or null (a bad entry must not cost the whole list).
#[derive(Deserialize)]
struct WireApp<'a> {
    #[serde(default)]
    id: Id,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    icon_hash: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    executables: Option<Vec<WireExe<'a>>>,
}

#[derive(Deserialize)]
struct WireExe<'a> {
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    os: Option<Cow<'a, str>>,
    #[serde(default)]
    is_launcher: Option<bool>,
}

/// Whether the saved list at `path` is missing or older than `MAX_AGE`.
pub fn stale(path: &Path) -> bool {
    let modified = std::fs::metadata(path).and_then(|m| m.modified());
    modified.ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_none_or(|age| age >= MAX_AGE)
}

/// A process path as the table compares it: lowercase with `/` separators.
pub fn normalize(path: &str) -> String {
    path.to_lowercase().replace('\\', "/")
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `path` ends with `suffix` at a folder boundary.
fn suffix_of(path: &str, suffix: &str) -> bool {
    path.strip_suffix(suffix).is_some_and(|rest| rest.is_empty() || rest.ends_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"[
        {"id":"356875221078245376","name":"Overwatch","icon_hash":"a60bb76ba4d4acafbd4cb9aad6e61739","hook":true,
         "executables":[{"is_launcher":false,"name":"overwatch.exe","os":"win32"}],"aliases":[]},
        {"id":"5","name":"World of Warcraft\u2122","icon_hash":null,
         "executables":[{"name":"_retail_/wow.exe","os":"win32"},{"name":"_retail_/world of warcraft.app","os":"darwin"},
                        {"name":">hl2.exe","os":"win32"},{"name":"wowlauncher.exe","os":"win32","is_launcher":true}]},
        {"id":"7","name":"Mac only","icon_hash":"zz","executables":[{"name":"mac.app","os":"darwin"}]},
        {"id":"6","name":"Web game","icon_hash":"00000000000000000000000000000abc","executables":null},
        {"id":"8","name":"Star Conflict","executables":[{"name":"star conflict/game.exe","os":"win32"}]}
    ]"#;

    #[test]
    fn parses_and_matches_like_the_official_list() {
        let t = Table::from_json(JSON.as_bytes()).unwrap();
        // Mac-only without an icon is dropped; the web game stays for its icon.
        assert_eq!(t.len(), 4);
        assert_eq!(t.icon(Id(356875221078245376)).as_deref(), Some("a60bb76ba4d4acafbd4cb9aad6e61739"));
        assert_eq!(t.icon(Id(6)).as_deref(), Some("00000000000000000000000000000abc"));
        assert_eq!((t.icon(Id(5)), t.icon(Id(99))), (None, None));
        let find = |p: &str| t.find(&normalize(p)).map(|a| &*a.name);
        assert_eq!(find(r"C:\Program Files\Overwatch\Overwatch.exe"), Some("Overwatch"), "case and separators");
        assert_eq!(find(r"D:\Games\World of Warcraft\_retail_\Wow.exe"), Some("World of Warcraft™"));
        assert_eq!(find(r"D:\Games\World of Warcraft\_classic_\Wow.exe"), None, "path suffix must match");
        assert_eq!(find(r"D:\Games\x_retail_\Wow.exe"), None, "only at a folder boundary");
        assert_eq!(find(r"C:\Steam\Star Conflict\game.exe"), Some("Star Conflict"));
        assert_eq!(find(r"C:\Other\game.exe"), None, "generic names need their folder");
        assert_eq!(find("wowlauncher.exe"), None, "launchers never count");
        assert_eq!(find("hl2.exe"), None, "`>` entries need the command line");
        assert!(t.has_file("game.exe") && !t.has_file("explorer.exe"));
        assert_eq!(t.by_name("Overwatch").map(|a| a.id), Some(Id(356875221078245376)));
        assert!(Table::from_json(b"{}").is_err());
    }

    #[test]
    fn disk_format_round_trips() {
        let t = Table::from_json(JSON.as_bytes()).unwrap();
        let again = Table::from_tsv(&t.to_tsv()).unwrap();
        assert_eq!(again.to_tsv(), t.to_tsv());
        assert_eq!(again.find("c:/games/star conflict/game.exe").map(|a| a.id), Some(Id(8)));
        assert!(Table::from_tsv("something else\n1\t\tx").is_none());
        let dir = std::env::temp_dir().join(format!("rc-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("detectable.tsv");
        assert!(stale(&path), "missing");
        t.save(&path);
        assert!(!stale(&path));
        assert_eq!(Table::load(&path).map(|t| t.len()), Some(4));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
