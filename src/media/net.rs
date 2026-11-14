//! Media bytes for the player and the viewer: HTTP Range reads in 1 MiB blocks (≤ 8 in memory,
//! two read ahead), the whole body when a server ignores Range (≤ 32 MiB), small files written
//! to the image disk cache once fully read, Save and Copy downloads. Same host allow-list and
//! client as images (`cache::images`): never an Authorization header, never a third-party host.

use super::Fail;
use crate::cache::images;
use std::collections::HashSet;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

pub const BLOCK: u64 = 1 << 20;
const MAX_BLOCKS: usize = 8;
/// A server without Range support: bodies up to this size are read whole.
pub const MAX_WHOLE: u64 = 32 << 20;
/// Files up to this size are written to the disk cache once every block was read.
const CACHE_MAX: u64 = MAX_BLOCKS as u64 * BLOCK;
const MAX_SAVE: u64 = 1 << 30;

static NET: OnceLock<(tokio::runtime::Handle, reqwest::Client)> = OnceLock::new();

/// Once, from `backend::spawn`: the app's runtime (no new one) and the image HTTP client.
pub fn init(rt: tokio::runtime::Handle) {
    NET.get_or_init(|| (rt, images::http_client()));
}

/// Runs `f` on the app's runtime; false before `init` (tests).
pub fn spawn(f: impl Future<Output = ()> + Send + 'static) -> bool {
    NET.get().map(|(rt, _)| rt.spawn(f)).is_some()
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// One fetched range: a `206` part with the file's total size, or a `200` whole body.
pub enum Got {
    Part(Vec<u8>, u64),
    Whole(Vec<u8>),
}

/// Fetches bytes `a..=b` (blocking).
pub type Fetch = Arc<dyn Fn(u64, u64) -> Result<Got, Fail> + Send + Sync>;
/// Runs a read-ahead job in the background.
pub type Spawn = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// The disk-cache file of `url` (shared with images and their 300 MB prune).
pub fn cache_path(url: &str) -> PathBuf {
    images::cache_path(url)
}

fn cached(url: &str) -> Option<PathBuf> {
    Some(cache_path(url)).filter(|p| p.is_file())
}

/// Whether `url` plays without any request (its bytes are in the disk cache).
pub fn is_cached(url: &str) -> bool {
    cached(url).is_some()
}

fn allowed(url: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(url).ok().filter(images::host_allowed)
}

/// `Content-Range: bytes a-b/total` -> (a, b, total).
pub fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let (a, b, total) = (a.parse().ok()?, b.parse().ok()?, total.parse().ok()?);
    (a <= b && b < total).then_some((a, b, total))
}

/// The bytes of `url` for the player: the disk cache, else Range reads; with the size.
pub fn open(url: &str) -> Result<(Box<dyn ReadSeek>, u64), Fail> {
    if let Some(p) = cached(url) {
        let f = std::fs::File::open(p).map_err(|_| Fail::Network)?;
        let len = f.metadata().map_err(|_| Fail::Network)?.len();
        return Ok((Box::new(io::BufReader::with_capacity(64 << 10, f)), len));
    }
    if crate::config::offline_mode() {
        return Err(Fail::Network);
    }
    let parsed = allowed(url).ok_or(Fail::Unsupported)?;
    let (rt, http) = NET.get().cloned().ok_or(Fail::Network)?;
    let rt2 = rt.clone();
    let fetch: Fetch = Arc::new(move |a, b| rt.block_on(fetch_range(&http, parsed.clone(), a, b)));
    let spawn: Spawn = Arc::new(move |job| drop(rt2.spawn_blocking(job)));
    RangeSource::open(fetch, spawn, Some(cache_path(url)))
}

async fn fetch_range(http: &reqwest::Client, url: reqwest::Url, a: u64, b: u64) -> Result<Got, Fail> {
    let resp = http
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={a}-{b}"))
        // Byte ranges of the file itself, not of a compressed body.
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| Fail::Network)?;
    match resp.status().as_u16() {
        206 => {
            let total = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range).ok_or(Fail::Network)?.2;
            Ok(Got::Part(body(resp, b - a + 1).await?, total))
        }
        200 if resp.content_length().is_some_and(|l| l > MAX_WHOLE) => Err(Fail::CantStream),
        // One byte over the cap tells `RangeSource::open` it is too large.
        200 => Ok(Got::Whole(body(resp, MAX_WHOLE + 1).await?)),
        403 | 404 | 410 => Err(Fail::Gone),
        _ => Err(Fail::Network),
    }
}

/// At most `cap` bytes of the body.
async fn body(mut resp: reqwest::Response, cap: u64) -> Result<Vec<u8>, Fail> {
    let mut out = Vec::with_capacity(resp.content_length().unwrap_or(0).min(cap) as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|_| Fail::Network)? {
        let room = (cap as usize).saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() as u64 >= cap {
            break;
        }
    }
    Ok(out)
}

/// The whole of `url` (Copy image), at most `cap` bytes; the disk cache first.
pub async fn fetch_all(url: String, cap: u64) -> Result<Vec<u8>, String> {
    if let Some(p) = cached(&url) {
        return std::fs::read(p).map_err(|e| e.to_string());
    }
    let parsed = allowed(&url).ok_or("This file can't be fetched here.")?;
    let (_, http) = NET.get().ok_or("Not ready")?;
    let resp = http.get(parsed).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let out = body(resp, cap + 1).await.map_err(|e| e.to_string())?;
    if out.len() as u64 > cap { Err("Too large".into()) } else { Ok(out) }
}

/// Download `url` to `path` (through `path.part`, renamed when complete; at most 1 GiB).
pub async fn save(url: String, path: PathBuf) -> Result<(), String> {
    if let Some(p) = cached(&url) {
        return std::fs::copy(p, &path).map(drop).map_err(|e| e.to_string());
    }
    let parsed = allowed(&url).ok_or("This file can't be fetched here.")?;
    let (_, http) = NET.get().ok_or("Not ready")?;
    // Big files take longer than the image client's 30 s.
    let mut resp = http.get(parsed).timeout(Duration::from_secs(3600)).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let part = path.with_extension(path.extension().map_or("part".into(), |e| format!("{}.part", e.to_string_lossy())));
    let result = async {
        use std::io::Write;
        let mut f = io::BufWriter::new(std::fs::File::create(&part).map_err(|e| e.to_string())?);
        let mut n = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            n += chunk.len() as u64;
            if n > MAX_SAVE {
                return Err("Too large".to_owned());
            }
            f.write_all(&chunk).map_err(|e| e.to_string())?;
        }
        f.flush().map_err(|e| e.to_string())
    }
    .await
    .and_then(|()| std::fs::rename(&part, &path).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Read + Seek over a file fetched in `BLOCK`-sized ranges.
pub struct RangeSource {
    inner: Arc<Inner>,
    pos: u64,
}

struct Inner {
    fetch: Fetch,
    spawn: Spawn,
    total: u64,
    cache_to: Option<PathBuf>,
    blocks: Mutex<Blocks>,
    ready: Condvar,
}

#[derive(Default)]
struct Blocks {
    /// Least recently used first.
    have: Vec<(u64, Arc<Vec<u8>>)>,
    busy: HashSet<u64>,
    seen: HashSet<u64>,
    written: bool,
}

impl RangeSource {
    /// Fetches the first block. A server that answers with the whole file gives an in-memory
    /// source (or `CantStream` above `MAX_WHOLE`).
    pub fn open(fetch: Fetch, spawn: Spawn, cache_to: Option<PathBuf>) -> Result<(Box<dyn ReadSeek>, u64), Fail> {
        match fetch(0, BLOCK - 1)? {
            Got::Whole(body) if body.len() as u64 > MAX_WHOLE => Err(Fail::CantStream),
            Got::Whole(body) => {
                if let Some(p) = cache_to.filter(|_| body.len() as u64 <= CACHE_MAX) {
                    let _ = crate::config::write_atomic(&p, &body);
                }
                let len = body.len() as u64;
                Ok((Box::new(io::Cursor::new(body)), len))
            }
            Got::Part(first, total) => Ok((Box::new(Self::with_first(fetch, spawn, cache_to, first, total)?), total)),
        }
    }

    fn with_first(fetch: Fetch, spawn: Spawn, cache_to: Option<PathBuf>, first: Vec<u8>, total: u64) -> Result<RangeSource, Fail> {
        if first.len() as u64 != BLOCK.min(total) {
            return Err(Fail::Network);
        }
        let inner = Arc::new(Inner { fetch, spawn, total, cache_to, blocks: Mutex::default(), ready: Condvar::new() });
        inner.insert(inner.lock(), 0, Arc::new(first));
        Ok(RangeSource { inner, pos: 0 })
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Blocks> {
        self.blocks.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fetch_block(&self, k: u64) -> Result<Vec<u8>, Fail> {
        let a = k * BLOCK;
        let b = (a + BLOCK).min(self.total) - 1;
        match (self.fetch)(a, b)? {
            Got::Part(d, _) if d.len() as u64 == b - a + 1 => Ok(d),
            _ => Err(Fail::Network),
        }
    }

    /// Adds block `k` (dropping the least recently used beyond `MAX_BLOCKS`); a small file whose
    /// every block is here gets written to the disk cache once.
    fn insert(&self, mut g: MutexGuard<'_, Blocks>, k: u64, data: Arc<Vec<u8>>) {
        g.have.push((k, data));
        if g.have.len() > MAX_BLOCKS {
            g.have.remove(0);
        }
        g.seen.insert(k);
        let n = self.total.div_ceil(BLOCK);
        let whole = self.cache_to.is_some() && !g.written && self.total <= CACHE_MAX && g.have.len() as u64 == n;
        let parts = whole.then(|| {
            g.written = true;
            let mut v = g.have.clone();
            v.sort_by_key(|b| b.0);
            v
        });
        drop(g);
        self.ready.notify_all();
        if let (Some(parts), Some(p)) = (parts, &self.cache_to) {
            let _ = crate::config::write_atomic(p, &parts.iter().flat_map(|b| b.1.iter().copied()).collect::<Vec<u8>>());
        }
    }

    /// Block `k`, waiting for its read-ahead or fetching it here.
    fn block(&self, k: u64) -> Result<Arc<Vec<u8>>, Fail> {
        let mut g = self.lock();
        loop {
            if let Some(i) = g.have.iter().position(|b| b.0 == k) {
                let e = g.have.remove(i);
                g.have.push(e.clone());
                return Ok(e.1);
            }
            if !g.busy.contains(&k) {
                break;
            }
            g = self.ready.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.busy.insert(k);
        drop(g);
        let got = self.fetch_block(k).map(Arc::new);
        let mut g = self.lock();
        g.busy.remove(&k);
        match got {
            Ok(d) => {
                self.insert(g, k, d.clone());
                Ok(d)
            }
            Err(e) => {
                drop(g);
                self.ready.notify_all();
                Err(e)
            }
        }
    }

    fn read_ahead(self: &Arc<Self>, k: u64) {
        if k * BLOCK >= self.total {
            return;
        }
        let mut g = self.lock();
        if g.busy.contains(&k) || g.have.iter().any(|b| b.0 == k) {
            return;
        }
        g.busy.insert(k);
        drop(g);
        let me = self.clone();
        (self.spawn)(Box::new(move || {
            let got = me.fetch_block(k);
            let mut g = me.lock();
            g.busy.remove(&k);
            match got {
                Ok(d) => me.insert(g, k, Arc::new(d)),
                Err(_) => {
                    drop(g);
                    me.ready.notify_all();
                }
            }
        }));
    }
}

impl Read for RangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.inner.total || buf.is_empty() {
            return Ok(0);
        }
        let k = self.pos / BLOCK;
        let block = self.inner.block(k).map_err(io::Error::other)?;
        let off = (self.pos - k * BLOCK) as usize;
        let n = buf.len().min(block.len().saturating_sub(off));
        buf[..n].copy_from_slice(&block[off..off + n]);
        self.pos += n as u64;
        if off + n > BLOCK as usize / 2 {
            self.inner.read_ahead(k + 1);
            self.inner.read_ahead(k + 2);
        }
        Ok(n)
    }
}

impl Seek for RangeSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.inner.total.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

/// The `Fail` inside an I/O error from a `RangeSource` (Network for anything else).
pub fn fail_of(e: &io::Error) -> Fail {
    e.get_ref().and_then(|x| x.downcast_ref::<Fail>()).copied().unwrap_or(Fail::Network)
}

/// A file name for Save dialogs: the last path segment without the query.
pub fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() { "download".into() } else { percent_encoding::percent_decode_str(name).decode_utf8_lossy().into_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake server over `len` bytes: 206 parts (200 whole bodies when `whole`), logging the
    /// start of every request. Read-ahead jobs run inline.
    struct Fake {
        data: Arc<Vec<u8>>,
        hits: Arc<Mutex<Vec<u64>>>,
    }

    impl Fake {
        fn new(len: usize) -> Fake {
            Fake { data: Arc::new((0..len).map(|i| (i % 251) as u8).collect()), hits: Arc::default() }
        }

        fn fetch(&self, whole: bool) -> Fetch {
            let (data, hits) = (self.data.clone(), self.hits.clone());
            Arc::new(move |a, b| {
                hits.lock().unwrap().push(a);
                if whole {
                    return Ok(Got::Whole(data.to_vec()));
                }
                Ok(Got::Part(data[a as usize..=b as usize].to_vec(), data.len() as u64))
            })
        }

        fn source(&self) -> RangeSource {
            let fetch = self.fetch(false);
            let Ok(Got::Part(first, total)) = fetch(0, BLOCK - 1) else { unreachable!() };
            RangeSource::with_first(fetch, Arc::new(|job| job()), None, first, total).unwrap()
        }

        fn hits(&self) -> Vec<u64> {
            self.hits.lock().unwrap().clone()
        }
    }

    fn blocks(src: &RangeSource) -> Vec<u64> {
        src.inner.lock().have.iter().map(|b| b.0).collect()
    }

    #[test]
    fn content_range_header() {
        assert_eq!(parse_content_range("bytes 0-1048575/5000000"), Some((0, 1_048_575, 5_000_000)));
        assert_eq!(parse_content_range(" bytes 10-10/11 "), Some((10, 10, 11)));
        assert_eq!(parse_content_range("bytes */5000"), None);
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn reads_across_block_edges_and_reads_ahead_once() {
        let fake = Fake::new((BLOCK * 4 + 100) as usize);
        let mut src = fake.source();
        assert_eq!(fake.hits(), [0]);
        src.seek(SeekFrom::Start(BLOCK - 8)).unwrap();
        let mut buf = [0u8; 16];
        src.read_exact(&mut buf).unwrap();
        let want: Vec<u8> = (BLOCK - 8..BLOCK + 8).map(|i| (i % 251) as u8).collect();
        assert_eq!(buf.to_vec(), want, "16 bytes over the edge of blocks 0 and 1");
        assert_eq!(fake.hits(), [0, BLOCK, 2 * BLOCK], "passing half of block 0 fetched blocks 1 and 2 ahead, block 1 once");
        // Past half of block 1: block 2 is already here, only block 3 is new.
        src.seek(SeekFrom::Start(BLOCK + BLOCK / 2 + 1)).unwrap();
        src.read_exact(&mut buf).unwrap();
        assert_eq!(fake.hits(), [0, BLOCK, 2 * BLOCK, 3 * BLOCK]);
        // The tail block is short; nothing is fetched beyond the end.
        src.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = Vec::new();
        src.read_to_end(&mut tail).unwrap();
        assert_eq!(tail.len(), 4);
        assert_eq!(fake.hits().len(), 5);
        assert!(src.seek(SeekFrom::Current(-(BLOCK as i64) * 9)).is_err());
    }

    #[test]
    fn keeps_at_most_eight_blocks() {
        let fake = Fake::new((BLOCK * 12) as usize);
        let mut src = fake.source();
        let mut b = [0u8; 1];
        for k in 0..12 {
            src.seek(SeekFrom::Start(k * BLOCK)).unwrap();
            src.read_exact(&mut b).unwrap();
            assert_eq!(b[0], ((k * BLOCK) % 251) as u8);
        }
        assert_eq!(fake.hits().len(), 12, "reads at block starts never trigger read-ahead");
        assert_eq!(blocks(&src), (4..12).collect::<Vec<_>>(), "least recently used dropped first");
        src.seek(SeekFrom::Start(5 * BLOCK)).unwrap();
        src.read_exact(&mut b).unwrap();
        assert_eq!(blocks(&src).last(), Some(&5), "a hit moves to the back");
        assert_eq!(fake.hits().len(), 12);
    }

    #[test]
    fn whole_bodies_without_range_support() {
        let fake = Fake::new(5000);
        let (mut src, len) = RangeSource::open(fake.fetch(true), Arc::new(|job| job()), None).unwrap();
        assert_eq!(len, 5000);
        let mut all = Vec::new();
        src.read_to_end(&mut all).unwrap();
        assert_eq!(all, *fake.data);
        let big = Fake::new((MAX_WHOLE + 1) as usize);
        assert_eq!(RangeSource::open(big.fetch(true), Arc::new(|job| job()), None).err(), Some(Fail::CantStream));
    }

    #[test]
    fn small_files_land_in_the_cache_once_read() {
        let dir = std::env::temp_dir().join(format!("rustcord-media-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("f");
        let data: Arc<Vec<u8>> = Arc::new((0..(BLOCK * 2 + 10) as usize).map(|i| (i % 7) as u8).collect());
        let d = data.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let fetch: Fetch = Arc::new(move |a, b| {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(Got::Part(d[a as usize..=b as usize].to_vec(), d.len() as u64))
        });
        let (mut src, _) = RangeSource::open(fetch, Arc::new(|job| job()), Some(path.clone())).unwrap();
        assert!(!path.exists(), "not before every block was read");
        let mut all = Vec::new();
        src.read_to_end(&mut all).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), *data);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_carry_their_kind() {
        let fetch: Fetch = Arc::new(|a, _| if a == 0 { Ok(Got::Part(vec![0; BLOCK as usize], BLOCK * 2)) } else { Err(Fail::Gone) });
        let (mut src, _) = RangeSource::open(fetch, Arc::new(|_| {}), None).unwrap();
        src.seek(SeekFrom::Start(BLOCK)).unwrap();
        let e = src.read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(fail_of(&e), Fail::Gone);
        assert_eq!(file_name("https://cdn.discordapp.com/attachments/1/2/my%20cat.mp4?ex=1&hm=2"), "my cat.mp4");
        assert_eq!(file_name("https://x/"), "download");
    }
}