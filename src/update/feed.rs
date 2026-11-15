//! The signed update feed (docs/UPDATER_DESIGN.md §3): the key, the signature check, the
//! manifest, the URL rules, the HTTP client and the download into the cache.

use base64::Engine as _;
use reqwest::Url;
use semver::Version;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const RELEASES: &str = "https://github.com/r0sc/Rustcord/releases";
const FEED: &str = "https://github.com/r0sc/Rustcord/releases/latest/download/latest.json";
const KEY: &str = include_str!("../../assets/update-signing.pub");
const MAX_FEED: usize = 64 * 1024;
const MAX_SIG: usize = 512;
/// The largest package the app accepts (also the manifest's `size` limit).
pub const MAX_SIZE: u64 = 200 * 1024 * 1024;
const USER_AGENT: &str = concat!("Rustcord/", env!("CARGO_PKG_VERSION"), " (+https://github.com/r0sc/Rustcord)");

/// A platform key of the manifest and its package (§2; frozen names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Windows,
    Linux,
    LinuxDeb,
    Macos,
}

impl Key {
    /// This build's package: the .deb (`deb`) or the tarball on Linux.
    pub fn native(deb: bool) -> Self {
        match std::env::consts::OS {
            "windows" => Key::Windows,
            "macos" => Key::Macos,
            _ if deb => Key::LinuxDeb,
            _ => Key::Linux,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Key::Windows => "windows-x86_64",
            Key::Linux => "linux-x86_64",
            Key::LinuxDeb => "linux-x86_64-deb",
            Key::Macos => "macos-universal",
        }
    }

    pub fn file(self, v: &Version) -> String {
        match self {
            Key::Windows => format!("Rustcord-Setup-{v}.exe"),
            Key::Linux => format!("rustcord-{v}-linux-x86_64.tar.gz"),
            Key::LinuxDeb => format!("rustcord_{v}_amd64.deb"),
            Key::Macos => format!("Rustcord-{v}-macos-universal.app.tar.gz"),
        }
    }
}

/// Where the feed and the packages come from: GitHub with the committed key, or a loopback
/// server in Test mode (§5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub feed: Url,
    pub key: [u8; 32],
    pub test: bool,
}

impl Source {
    pub fn github() -> Option<Self> {
        Some(Self { feed: Url::parse(FEED).ok()?, key: decode_key(KEY)?, test: false })
    }

    /// A Test-mode source: `feed` must be a loopback URL (`loopback_url`).
    pub fn test(feed: Url, key: Option<[u8; 32]>) -> Option<Self> {
        Some(Self { feed, key: key.or_else(|| decode_key(KEY))?, test: true })
    }

    fn sig_url(&self) -> String {
        format!("{}.sig", self.feed)
    }

    /// The folder every package of `v` must come from (§3.3).
    pub fn asset_base(&self, v: &Version) -> String {
        if self.test {
            // loopback_url refused a query or fragment, so the last '/' ends the folder.
            let s = self.feed.as_str();
            s[..=s.rfind('/').unwrap_or(s.len() - 1)].to_owned()
        } else {
            format!("{RELEASES}/download/v{v}/")
        }
    }
}

/// The 32-byte Ed25519 key from standard base64, if it is a valid curve point.
pub fn decode_key(b64: &str) -> Option<[u8; 32]> {
    let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok().map(|_| bytes)
}

/// `sig_text` (base64 of 64 bytes) signs exactly `json` with `key`.
pub fn verify(key: &[u8; 32], json: &[u8], sig_text: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(sig_text) else { return false };
    let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(text.trim()) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig) else { return false };
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(key) else { return false };
    key.verify_strict(json, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// `X.Y.Z` without prerelease or build suffix (manifest and state.toml versions).
pub fn plain_version(s: &str) -> Option<Version> {
    Version::parse(s).ok().filter(|v| v.pre.is_empty() && v.build.is_empty())
}

/// A test feed URL: http(s) on 127.0.0.1, [::1] or localhost, no user info, query or
/// fragment. The parsed host decides, so `127.0.0.1.evil.com` or `127.0.0.1@evil.com` fail.
pub fn loopback_url(s: &str) -> Option<Url> {
    let url = Url::parse(s).ok()?;
    let plain = url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none();
    (matches!(url.scheme(), "http" | "https") && plain && is_loopback(&url)).then_some(url)
}

/// The parsed host, serialized canonically (`127.1` -> `127.0.0.1`, `::ffff:127.0.0.1` ->
/// `[::ffff:7f00:1]`, domains lowercased), so a string compare is a host compare.
fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
}

/// A redirect hop may go to any https URL; Test mode also to plain http on a loopback host.
fn hop_ok(url: &Url, test: bool) -> bool {
    url.scheme() == "https" || (test && url.scheme() == "http" && url.username().is_empty() && is_loopback(url))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 lowercase hex digits.
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// One verified package of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub url: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

/// What a verified feed offers this copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub version: Version,
    /// The package for this copy's key; None: no key or none in the manifest (notice only).
    pub package: Option<Package>,
    /// SHA-256 of the `latest.json` bytes (`bad_manifest`).
    pub manifest: [u8; 32],
}

#[derive(serde::Deserialize)]
struct RawManifest {
    schema: u64,
    version: String,
    platforms: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct RawPackage {
    url: String,
    sha256: String,
    size: u64,
}

/// Parses verified manifest bytes (§3.1, §3.3 steps 3 and 5). Only this copy's entry is read.
pub fn parse(json: &[u8], key: Option<Key>, src: &Source) -> Result<Offer, String> {
    let raw: RawManifest = serde_json::from_slice(json).map_err(|e| format!("manifest: {e}"))?;
    if raw.schema != 1 {
        return Err(format!("manifest schema {}", raw.schema));
    }
    let version = plain_version(&raw.version).ok_or_else(|| format!("manifest version {:?}", raw.version))?;
    let package = match key.and_then(|k| raw.platforms.get(k.name()).map(|v| (k, v))) {
        None => None,
        Some((k, value)) => {
            let p: RawPackage = serde_json::from_value(value.clone()).map_err(|e| format!("manifest {}: {e}", k.name()))?;
            if p.url != format!("{}{}", src.asset_base(&version), k.file(&version)) {
                return Err(format!("manifest {}: unexpected url", k.name()));
            }
            let sha256 = parse_hex32(&p.sha256).ok_or_else(|| format!("manifest {}: bad sha256", k.name()))?;
            if !(1..=MAX_SIZE).contains(&p.size) {
                return Err(format!("manifest {}: bad size {}", k.name(), p.size));
            }
            Some(Package { url: p.url, sha256, size: p.size })
        }
    };
    Ok(Offer { version, package, manifest: Sha256::digest(json).into() })
}

/// Why a check found nothing usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// Transport error, 5xx, an oversized answer: retry soon.
    Network(String),
    /// 4xx (no release published yet): retry in 6 h.
    NoFeed(u16),
    Signature,
    Invalid(String),
}

/// One client per check / download, dropped afterwards (§3.4): no Discord headers, token or
/// cookies; redirects stay on https.
fn client(src: &Source) -> reqwest::Result<reqwest::Client> {
    let test = src.test;
    let policy = reqwest::redirect::Policy::custom(move |a| {
        if a.previous().len() > 10 {
            a.error("too many redirects")
        } else if hop_ok(a.url(), test) {
            a.follow()
        } else {
            a.error("redirect to a non-https URL")
        }
    });
    let mut b = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .no_gzip()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .redirect(policy);
    if test {
        // "localhost" never goes to DNS or a proxy: the test feed stays on this machine.
        b = b.no_proxy().resolve("localhost", std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
    }
    b.build()
}

fn net(e: reqwest::Error) -> CheckError {
    CheckError::Network(e.without_url().to_string())
}

/// Fetches `latest.json` + `.sig`, verifies the signature, then parses (§3.3).
pub async fn check(src: &Source, key: Option<Key>) -> Result<Offer, CheckError> {
    let client = client(src).map_err(net)?;
    let json = get_capped(&client, src.feed.as_str(), MAX_FEED).await?;
    let sig = get_capped(&client, &src.sig_url(), MAX_SIG).await?;
    // No field is read before this.
    if !verify(&src.key, &json, &sig) {
        return Err(CheckError::Signature);
    }
    parse(&json, key, src).map_err(CheckError::Invalid)
}

async fn get_capped(client: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>, CheckError> {
    let mut resp = client.get(url).timeout(Duration::from_secs(30)).send().await.map_err(net)?;
    let status = resp.status();
    if status.is_client_error() {
        return Err(CheckError::NoFeed(status.as_u16()));
    }
    if status != reqwest::StatusCode::OK {
        return Err(CheckError::Network(format!("HTTP {}", status.as_u16())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(net)? {
        if body.len() + chunk.len() > cap {
            return Err(CheckError::Invalid(format!("feed file over {cap} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    Network(String),
    /// Writing the file failed (disk full, permissions).
    Disk(String),
    /// The complete answer does not match the signed size or hash.
    Damaged,
}

/// Streams a package into `<dest>.part`, checks size and SHA-256, then renames it to `dest`.
/// The `.part` file goes away on every failure (also when dropped mid-way).
pub struct Sink {
    file: Option<std::fs::File>,
    part: PathBuf,
    dest: PathBuf,
    hasher: Sha256,
    done: u64,
    size: u64,
    sha256: [u8; 32],
}

impl Sink {
    pub fn create(dest: PathBuf, pkg: &Package) -> Result<Self, DownloadError> {
        let disk = |e: std::io::Error| DownloadError::Disk(e.to_string());
        let mut part = dest.clone().into_os_string();
        part.push(".part");
        let part = PathBuf::from(part);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(disk)?;
        }
        let file = std::fs::File::create(&part).map_err(disk)?;
        Ok(Self { file: Some(file), part, dest, hasher: Sha256::new(), done: 0, size: pkg.size, sha256: pkg.sha256 })
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<(), DownloadError> {
        if self.done + chunk.len() as u64 > self.size {
            return Err(DownloadError::Damaged);
        }
        let file = self.file.as_mut().ok_or(DownloadError::Damaged)?;
        file.write_all(chunk).map_err(|e| DownloadError::Disk(e.to_string()))?;
        self.hasher.update(chunk);
        self.done += chunk.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), DownloadError> {
        let hash: [u8; 32] = std::mem::take(&mut self.hasher).finalize().into();
        if self.done != self.size || hash != self.sha256 {
            return Err(DownloadError::Damaged);
        }
        let disk = |e: std::io::Error| DownloadError::Disk(e.to_string());
        // Closed before the rename (Windows can't rename an open file).
        self.file.take().ok_or(DownloadError::Damaged)?.sync_all().map_err(disk)?;
        std::fs::rename(&self.part, &self.dest).map_err(disk)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.part);
    }
}

/// Downloads `pkg` to `dest` (§3.5); `progress` gets the byte count at most every 100 ms.
/// Only a complete answer with the wrong size or hash is `Damaged`; a broken connection is
/// `Network`.
pub async fn download(src: &Source, pkg: &Package, dest: PathBuf, progress: impl Fn(u64)) -> Result<(), DownloadError> {
    let net = |e: reqwest::Error| DownloadError::Network(e.without_url().to_string());
    let client = client(src).map_err(net)?;
    let mut resp = client.get(&pkg.url).send().await.map_err(net)?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(DownloadError::Network(format!("HTTP {}", resp.status().as_u16())));
    }
    if resp.content_length().is_some_and(|n| n > pkg.size) {
        return Err(DownloadError::Damaged);
    }
    let mut sink = Sink::create(dest, pkg)?;
    let mut shown = Instant::now();
    while let Some(chunk) = resp.chunk().await.map_err(net)? {
        sink.write(&chunk)?;
        if shown.elapsed() >= Duration::from_millis(100) {
            shown = Instant::now();
            progress(sink.done());
        }
    }
    progress(sink.done());
    sink.finish()
}

/// The staged file still has the signed size and hash (§3.5, before applying).
pub fn file_matches(path: &std::path::Path, size: u64, sha256: &[u8; 32]) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    if f.metadata().map(|m| m.len()).ok() != Some(size) {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut f, &mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    <[u8; 32]>::from(hasher.finalize()) == *sha256
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/UPDATER_DESIGN.md §3.2, byte for byte as make-update-manifest.py writes it.
    pub(crate) const MANIFEST: &str = r#"{
  "platforms": {
    "linux-x86_64": {
      "sha256": "9f2c4e1a7b3d5c6e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6",
      "size": 16003285,
      "url": "https://github.com/r0sc/Rustcord/releases/download/v0.1.0/rustcord-0.1.0-linux-x86_64.tar.gz"
    },
    "linux-x86_64-deb": {
      "sha256": "0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5",
      "size": 11452804,
      "url": "https://github.com/r0sc/Rustcord/releases/download/v0.1.0/rustcord_0.1.0_amd64.deb"
    },
    "macos-universal": {
      "sha256": "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
      "size": 31457280,
      "url": "https://github.com/r0sc/Rustcord/releases/download/v0.1.0/Rustcord-0.1.0-macos-universal.app.tar.gz"
    },
    "windows-x86_64": {
      "sha256": "5d41402abc4b2a76b9719d911017c5925d41402abc4b2a76b9719d911017c592",
      "size": 24187210,
      "url": "https://github.com/r0sc/Rustcord/releases/download/v0.1.0/Rustcord-Setup-0.1.0.exe"
    }
  },
  "schema": 1,
  "version": "0.1.0"
}
"#;
    /// A throwaway key (openssl genpkey -algorithm ed25519; the private half was deleted) and
    /// its signature over MANIFEST (openssl pkeyutl -sign -rawin, as release.yml signs).
    const TEST_KEY: &str = "qfhI8y0rxvesm8m+knaaFmR1EsdXWzPB+o3F8g2/yeM=";
    const TEST_SIG: &str = "y7z7s3ZfeJ/RMo/skjYX3rkoalZiNLJaN7/3vsWEslSypIZaXoouXYHlZLqTP3TQnnVy9jwk4dy5HtOc3dZXAw==\n";

    fn github() -> Source {
        Source::github().unwrap()
    }

    #[test]
    fn committed_key_is_a_valid_point() {
        // Trimmed like `decode_key`: a CRLF checkout must not fail the Windows CI tests.
        assert_eq!(KEY.trim(), "xhWhAUDE17E0HTQVGomlSG7pgrr517sISogYm9/PvxY=");
        assert!(decode_key(KEY).is_some());
        assert_eq!(decode_key("AAAA"), None, "wrong length");
        assert_eq!(decode_key("not base64!"), None);
    }

    #[test]
    fn signature_checks() {
        let key = decode_key(TEST_KEY).unwrap();
        let json = MANIFEST.as_bytes();
        assert!(verify(&key, json, TEST_SIG.as_bytes()));
        let mut flipped = json.to_vec();
        flipped[10] ^= 1;
        assert!(!verify(&key, &flipped, TEST_SIG.as_bytes()), "a flipped manifest byte");
        let mut sig = base64::engine::general_purpose::STANDARD.decode(TEST_SIG.trim()).unwrap();
        sig[0] ^= 1;
        let bad = base64::engine::general_purpose::STANDARD.encode(&sig);
        assert!(!verify(&key, json, bad.as_bytes()), "a flipped signature byte");
        assert!(!verify(&decode_key(KEY).unwrap(), json, TEST_SIG.as_bytes()), "the wrong key");
        assert!(!verify(&key, json, b"%%%"), "bad base64");
        assert!(!verify(&key, json, b"AAAA"), "wrong length");
        assert!(!verify(&key, json, &[0xff, 0xfe]), "not UTF-8");
    }

    #[test]
    fn parses_the_design_fixture() {
        let offer = parse(MANIFEST.as_bytes(), Some(Key::Windows), &github()).unwrap();
        assert_eq!(offer.version, Version::new(0, 1, 0));
        let p = offer.package.unwrap();
        assert_eq!(p.url, "https://github.com/r0sc/Rustcord/releases/download/v0.1.0/Rustcord-Setup-0.1.0.exe");
        assert_eq!((hex(&p.sha256).as_str(), p.size), ("5d41402abc4b2a76b9719d911017c5925d41402abc4b2a76b9719d911017c592", 24187210));
        assert_eq!(offer.manifest, <[u8; 32]>::from(Sha256::digest(MANIFEST.as_bytes())));
        for (k, size) in [(Key::Linux, 16003285), (Key::LinuxDeb, 11452804), (Key::Macos, 31457280)] {
            assert_eq!(parse(MANIFEST.as_bytes(), Some(k), &github()).unwrap().package.unwrap().size, size, "{k:?}");
        }
        // No key for this copy: a notice only.
        assert_eq!(parse(MANIFEST.as_bytes(), None, &github()).unwrap().package, None);
        // Unknown fields are ignored; a missing entry is a notice.
        let lean = r#"{"schema":1,"version":"1.2.3","platforms":{},"extra":true}"#;
        assert_eq!(parse(lean.as_bytes(), Some(Key::Linux), &github()).unwrap().package, None);
    }

    #[test]
    fn manifest_rejections() {
        let entry = |url: &str, sha: &str, size: &str| {
            format!(r#"{{"schema":1,"version":"0.2.0","platforms":{{"linux-x86_64":{{"url":"{url}","sha256":"{sha}","size":{size}}}}}}}"#)
        };
        let good_url = "https://github.com/r0sc/Rustcord/releases/download/v0.2.0/rustcord-0.2.0-linux-x86_64.tar.gz";
        let sha = "ab".repeat(32);
        assert!(parse(entry(good_url, &sha, "5").as_bytes(), Some(Key::Linux), &github()).is_ok());
        let bad = [
            entry(good_url, &sha, "0"),
            entry(good_url, &sha, "209715201"),
            entry(good_url, &sha, "\"5\""),
            entry(good_url, &"AB".repeat(32), "5"),
            entry(good_url, &"ab".repeat(31), "5"),
            entry(good_url, &format!("{}zz", "ab".repeat(31)), "5"),
            entry("https://github.com/r0sc/Rustcord/releases/download/v0.1.0/rustcord-0.2.0-linux-x86_64.tar.gz", &sha, "5"),
            entry("https://evil.example/rustcord-0.2.0-linux-x86_64.tar.gz", &sha, "5"),
            entry(&good_url.replace("https:", "http:"), &sha, "5"),
            entry(&good_url.replace("linux-x86_64", "linux-aarch64"), &sha, "5"),
            entry(good_url, &sha, "5").replace("\"schema\":1", "\"schema\":2"),
            entry(good_url, &sha, "5").replace("\"schema\":1", "\"schema\":\"1\""),
            entry(good_url, &sha, "5").replace("0.2.0\"", "v0.2.0\""),
            entry(good_url, &sha, "5").replace("\"version\":\"0.2.0\"", "\"version\":\"0.2.0-beta.1\""),
            entry(good_url, &sha, "5").replace("\"version\":\"0.2.0\"", "\"version\":\"0.2\""),
            "[]".to_owned(),
            "{".to_owned(),
        ];
        for b in bad {
            assert!(parse(b.as_bytes(), Some(Key::Linux), &github()).is_err(), "{b}");
        }
    }

    #[test]
    fn versions_compare_with_prereleases() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("0.1.1") > v("0.1.0") && v("0.10.0") > v("0.9.9") && v("1.0.0") > v("0.99.99"));
        // A running prerelease updates to its release; the feed itself never has one.
        assert!(v("0.2.0") > v("0.2.0-beta.1") && v("0.2.0-beta.1") > v("0.1.9"));
        assert_eq!(plain_version("0.2.0-beta.1"), None);
        assert_eq!(plain_version("0.2.0+build"), None);
        assert_eq!(plain_version("v0.2.0"), None);
        assert_eq!(plain_version("01.2.0"), None);
        assert_eq!(plain_version("0.2.0"), Some(v("0.2.0")));
    }

    #[test]
    fn loopback_url_rules() {
        for ok in ["http://127.0.0.1:8765/latest.json", "https://localhost/x/latest.json", "http://[::1]:9/latest.json", "http://LOCALHOST/latest.json"] {
            assert!(loopback_url(ok).is_some(), "{ok}");
        }
        for bad in [
            "https://github.com/latest.json",
            "http://127.0.0.1.evil.com/latest.json",
            "http://127.0.0.1@evil.com/latest.json",
            "http://user:pw@127.0.0.1/latest.json",
            "http://[::ffff:127.0.0.1]/latest.json",
            "http://localhost.evil.com/latest.json",
            "http://127.0.0.2/latest.json",
            "ftp://127.0.0.1/latest.json",
            "file:///latest.json",
            "http://127.0.0.1/latest.json?x=1",
            "http://127.0.0.1/latest.json#x",
            "127.0.0.1/latest.json",
            "",
        ] {
            assert!(loopback_url(bad).is_none(), "{bad}");
        }
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(hop_ok(&u("https://objects.githubusercontent.com/x"), false));
        assert!(!hop_ok(&u("http://github.com/x"), false));
        assert!(!hop_ok(&u("http://127.0.0.1/x"), false), "plain http only in Test mode");
        assert!(hop_ok(&u("http://127.0.0.1/x"), true));
        assert!(!hop_ok(&u("http://example.com/x"), true));
    }

    #[test]
    fn expected_urls_per_key() {
        let v = Version::new(0, 1, 1);
        let gh = github();
        let base = "https://github.com/r0sc/Rustcord/releases/download/v0.1.1/";
        assert_eq!(gh.asset_base(&v), base);
        let names: Vec<String> = [Key::Windows, Key::Linux, Key::LinuxDeb, Key::Macos].iter().map(|k| k.file(&v)).collect();
        assert_eq!(names, ["Rustcord-Setup-0.1.1.exe", "rustcord-0.1.1-linux-x86_64.tar.gz", "rustcord_0.1.1_amd64.deb", "Rustcord-0.1.1-macos-universal.app.tar.gz"]);
        let test = Source::test(loopback_url("http://127.0.0.1:8765/feed/latest.json").unwrap(), None).unwrap();
        assert_eq!(test.asset_base(&v), "http://127.0.0.1:8765/feed/");
        assert_eq!(test.sig_url(), "http://127.0.0.1:8765/feed/latest.json.sig");
        let root = Source::test(loopback_url("http://127.0.0.1:8765").unwrap(), None).unwrap();
        assert_eq!(root.asset_base(&v), "http://127.0.0.1:8765/");
        assert_eq!(test.key, gh.key, "no override: the committed key");
        // In Test mode the packages must come from the loopback folder, not GitHub.
        let gh_json = MANIFEST.replace("0.1.0", "0.1.1");
        assert!(parse(gh_json.as_bytes(), Some(Key::Windows), &test).is_err());
        assert!(parse(gh_json.replace(base, "http://127.0.0.1:8765/feed/").as_bytes(), Some(Key::Windows), &test).is_ok());
    }

    #[test]
    fn hex_round_trip() {
        let b: [u8; 32] = core::array::from_fn(|i| (i * 9) as u8);
        assert_eq!(parse_hex32(&hex(&b)), Some(b));
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    fn sink_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-update-sink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn sink_checks_size_and_hash() {
        let data = b"hello update".repeat(1000);
        let pkg = Package { url: String::new(), sha256: Sha256::digest(&data).into(), size: data.len() as u64 };
        let dir = sink_dir("ok");
        let dest = dir.join("pkg.bin");
        let mut s = Sink::create(dest.clone(), &pkg).unwrap();
        for c in data.chunks(777) {
            s.write(c).unwrap();
        }
        assert!(dir.join("pkg.bin.part").exists() && !dest.exists());
        s.finish().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!dir.join("pkg.bin.part").exists());
        assert!(file_matches(&dest, pkg.size, &pkg.sha256));
        assert!(!file_matches(&dest, pkg.size - 1, &pkg.sha256));
        assert!(!file_matches(&dest, pkg.size, &[0; 32]));
        assert!(!file_matches(&dir.join("missing"), pkg.size, &pkg.sha256));

        // One byte too many: refused while streaming, the .part is gone.
        let mut s = Sink::create(dir.join("big.bin"), &pkg).unwrap();
        s.write(&data).unwrap();
        assert_eq!(s.write(b"x"), Err(DownloadError::Damaged));
        drop(s);
        assert!(!dir.join("big.bin.part").exists());
        // Short or wrong bytes: damaged, nothing staged.
        let mut s = Sink::create(dir.join("short.bin"), &pkg).unwrap();
        s.write(&data[1..]).unwrap();
        assert_eq!(s.finish(), Err(DownloadError::Damaged));
        let mut s = Sink::create(dir.join("wrong.bin"), &pkg).unwrap();
        let mut bad = data.clone();
        bad[5] ^= 1;
        s.write(&bad).unwrap();
        assert_eq!(s.finish(), Err(DownloadError::Damaged));
        assert!(!dir.join("short.bin").exists() && !dir.join("wrong.bin").exists());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["pkg.bin"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
