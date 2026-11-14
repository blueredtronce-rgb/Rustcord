//! Image fetching + disk cache + decoding (runs on the backend runtime).
//!
//! - Only Discord CDN/media hosts, the GIF host (static.klipy.com) and Spotify's album art
//!   (i.scdn.co) are fetched; never sends the auth token.
//! - Disk cache in `config::paths().images_dir()`, file name = FNV-1a hash of the cache key.
//!   The cache key strips volatile signature params (`ex`, `is`, `hm`) of attachment URLs.
//! - Decoding happens in `spawn_blocking`, downscaled so the longest side <= max_side.
//!   GIF / APNG / animated WebP decode to multiple frames, capped by a memory budget
//!   (fallback: first frame only).
//!
//! OWNERSHIP: cache implementer.

use crate::backend::{BackendEvent, DecodedImage, EventSink, ImageFrame};
use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
use image::{AnimationDecoder, Frames, ImageDecoder, ImageFormat, RgbaImage};
use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;

const MAX_CONCURRENT: usize = 6;
/// Larger responses are not images we want to show inline (and could be huge originals).
const MAX_DOWNLOAD: usize = 32 * 1024 * 1024;
const DISK_CAP: u64 = 300 * 1024 * 1024;
/// Prune below the cap so it doesn't have to run again on the next start.
const DISK_PRUNE_TARGET: u64 = 240 * 1024 * 1024;
/// A third of the UI texture budget (12 MB) so one GIF can't crowd out everything else.
const MAX_ANIM_BYTES: usize = 4 * 1024 * 1024;
const MAX_ANIM_FRAMES: usize = 60;

#[derive(Clone)]
pub struct ImageLoader {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    permits: Semaphore,
    in_flight: Mutex<HashSet<String>>,
    dir: PathBuf,
}

impl ImageLoader {
    pub fn new() -> Self {
        // Same UA as the API client; no Authorization header ever.
        let ua = crate::api::props::ClientProps::new(&crate::config::Settings::default()).user_agent().to_owned();
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|a| {
                if a.previous().len() >= 5 || !host_allowed(a.url()) { a.stop() } else { a.follow() }
            }))
            .build()
            .expect("image HTTP client");
        let dir = crate::config::paths().images_dir();
        let prune_dir = dir.clone();
        let _ = std::thread::Builder::new()
            .name("rustcord-img-prune".into())
            .spawn(move || prune(&prune_dir, DISK_CAP, DISK_PRUNE_TARGET));
        Self {
            inner: Arc::new(Inner {
                http,
                permits: Semaphore::new(MAX_CONCURRENT),
                in_flight: Mutex::new(HashSet::new()),
                dir,
            }),
        }
    }

    /// Fetch+decode in the background and send `BackendEvent::ImageLoaded` to `sink`.
    /// Concurrency-limited; duplicate in-flight requests for the same url are coalesced.
    pub fn request(&self, url: String, max_side: u32, sink: EventSink) {
        if !self.inner.in_flight().insert(url.clone()) {
            return;
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let result = inner.load(&url, max_side).await;
            inner.in_flight().remove(&url);
            sink.send(BackendEvent::ImageLoaded { url, result });
        });
    }
}

impl Inner {
    fn in_flight(&self) -> MutexGuard<'_, HashSet<String>> {
        self.in_flight.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn load(&self, url: &str, max_side: u32) -> Result<DecodedImage, String> {
        // Local profile overrides: only files inside the overrides folder, never the network.
        if let Some(path) = crate::overrides::local_path(url) {
            return blocking(move || std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| decode(&b, max_side))).await?;
        }
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid image URL: {e}"))?;
        if !host_allowed(&parsed) {
            return Err(format!("image host not allowed: {}", parsed.host_str().unwrap_or("?")));
        }
        let _permit = self.permits.acquire().await.map_err(|e| e.to_string())?;
        let path = self.dir.join(format!("{:016x}", fnv1a64(cache_key(url).as_bytes())));

        let p = path.clone();
        // A cached file that no longer decodes is treated as a miss and overwritten.
        let cached = blocking(move || std::fs::read(&p).ok().and_then(|b| decode(&b, max_side).ok())).await?;
        if let Some(img) = cached {
            return Ok(img);
        }
        // Offline mode promises no network at all: disk cache only.
        if crate::config::offline_mode() {
            return Err("offline".into());
        }

        let bytes = self.download(parsed).await?;
        blocking(move || {
            let img = decode(&bytes, max_side)?;
            // Only cache what decodes, so error pages never end up on disk.
            if let Err(e) = crate::config::write_atomic(&path, &bytes) {
                eprintln!("rustcord: image cache write failed: {e}");
            }
            Ok(img)
        })
        .await?
    }

    async fn download(&self, url: reqwest::Url) -> Result<Vec<u8>, String> {
        let mut resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let len = resp.content_length().unwrap_or(0);
        if len > MAX_DOWNLOAD as u64 {
            return Err("image too large".into());
        }
        let mut body = Vec::with_capacity(len as usize);
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + chunk.len() > MAX_DOWNLOAD {
                return Err("image too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("image task failed: {e}"))
}

/// https on a Discord CDN/media host, the GIF picker host (FEATURES_NOTES §1.8) or Spotify's
/// album-art host only.
fn host_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.host_str().is_some_and(|h| {
            h == "cdn.discordapp.com"
                || h == "media.discordapp.net"
                || h == "static.klipy.com"
                // Album art of `spotify:` activities (`cdn::activity_asset_url`); the official
                // client loads it from here too.
                || h == "i.scdn.co"
                || h.strip_prefix("images-ext-")
                    .and_then(|r| r.strip_suffix(".discordapp.net"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// URL without the expiring signature params (`ex`, `is`, `hm`); other params identify the variant.
fn cache_key(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else { return url.to_owned() };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !matches!(p.split('=').next(), Some("ex" | "is" | "hm")))
        .collect();
    if kept.is_empty() { base.to_owned() } else { format!("{base}?{}", kept.join("&")) }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Delete the oldest files until the folder is below `target`, if it is above `cap`.
fn prune(dir: &Path, cap: u64, target: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= cap {
        return;
    }
    files.sort();
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Decode bytes (also used for local override images) into frames, longest side <= max_side.
pub fn decode(bytes: &[u8], max_side: u32) -> Result<DecodedImage, String> {
    let max_side = max_side.max(1);
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let animation = match reader.format() {
        Some(ImageFormat::Gif) => {
            let mut dec = GifDecoder::new(Cursor::new(bytes)).map_err(err)?;
            dec.set_limits(anim_limits()).map_err(err)?;
            Some(dec.into_frames())
        }
        Some(ImageFormat::Png) => {
            let dec = PngDecoder::with_limits(Cursor::new(bytes), anim_limits()).map_err(err)?;
            if dec.is_apng().map_err(err)? { Some(dec.apng().map_err(err)?.into_frames()) } else { None }
        }
        Some(ImageFormat::WebP) => {
            let mut dec = WebPDecoder::new(Cursor::new(bytes)).map_err(err)?;
            dec.set_limits(anim_limits()).map_err(err)?;
            dec.has_animation().then(|| dec.into_frames())
        }
        _ => None,
    };
    let frames = match animation {
        Some(frames) => animation_frames(frames, max_side)?,
        None => vec![ImageFrame { image: color_image(reader.decode().map_err(err)?.into_rgba8(), max_side), delay_ms: 0 }],
    };
    Ok(DecodedImage { frames })
}

/// The animation decoders default to no limits; a tiny file can declare a huge canvas.
fn anim_limits() -> image::Limits {
    let mut l = image::Limits::default(); // non_exhaustive: start from default
    l.max_image_width = Some(8192);
    l.max_image_height = Some(8192);
    l.max_alloc = Some(256 << 20);
    l
}

fn err(e: image::ImageError) -> String {
    e.to_string()
}

/// Collects frames within MAX_ANIM_FRAMES / MAX_ANIM_BYTES: when over budget, every other
/// kept frame is dropped (its delay merged into the previous one) and from then on only every
/// `stride`-th frame is resized, so memory stays bounded even for very long animations.
fn animation_frames(frames: Frames<'_>, max_side: u32) -> Result<Vec<ImageFrame>, String> {
    let mut kept: Vec<ImageFrame> = Vec::new();
    let mut bytes = 0usize;
    let mut stride = 1usize;
    for (i, frame) in frames.enumerate() {
        let frame = match frame {
            Ok(f) => f,
            Err(e) if kept.is_empty() => return Err(e.to_string()),
            // Truncated animation: show what decoded, like browsers do.
            Err(_) => break,
        };
        let delay_ms = frame_delay_ms(frame.delay());
        if i % stride != 0 {
            if let Some(last) = kept.last_mut() {
                last.delay_ms += delay_ms;
            }
            continue;
        }
        let image = color_image(frame.into_buffer(), max_side);
        bytes += image.pixels.len() * 4;
        kept.push(ImageFrame { image, delay_ms });
        while kept.len() > MAX_ANIM_FRAMES || bytes > MAX_ANIM_BYTES {
            if kept.len() == 1 {
                // A single frame is already over budget: show it as a still image.
                return Ok(vec![ImageFrame { delay_ms: 0, ..kept.remove(0) }]);
            }
            kept = halve(kept);
            bytes = kept.iter().map(|f| f.image.pixels.len() * 4).sum();
            stride *= 2;
        }
    }
    if kept.len() == 1 {
        kept[0].delay_ms = 0;
    }
    if kept.is_empty() { Err("image has no frames".into()) } else { Ok(kept) }
}

fn halve(frames: Vec<ImageFrame>) -> Vec<ImageFrame> {
    let mut out = Vec::with_capacity(frames.len().div_ceil(2));
    let mut it = frames.into_iter();
    while let Some(mut f) = it.next() {
        f.delay_ms += it.next().map_or(0, |g| g.delay_ms);
        out.push(f);
    }
    out
}

/// Browsers show very short GIF delays (often 0) as 100 ms.
fn frame_delay_ms(delay: image::Delay) -> u32 {
    let (n, d) = delay.numer_denom_ms();
    let ms = (n + d / 2).checked_div(d).unwrap_or(0);
    if ms < 20 { 100 } else { ms }
}

/// Premultiply (imageops::resize assumes it; avoids dark fringes), downscale, convert.
fn color_image(mut img: RgbaImage, max_side: u32) -> egui::ColorImage {
    for px in img.as_chunks_mut::<4>().0 {
        let a = px[3] as u16;
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
    }
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest > max_side {
        let scale = max_side as f64 / longest as f64;
        let nw = ((w as f64 * scale).round() as u32).clamp(1, max_side);
        let nh = ((h as f64 * scale).round() as u32).clamp(1, max_side);
        img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    }
    let (w, h) = img.dimensions();
    egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], img.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba};

    fn frames_of(list: Vec<(RgbaImage, u32)>) -> Frames<'static> {
        Frames::new(Box::new(
            list.into_iter().map(|(img, ms)| Ok(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1)))),
        ))
    }

    #[test]
    fn cache_key_drops_signature_params() {
        let u = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=65f&is=65e&hm=abc123&";
        assert_eq!(cache_key(u), "https://cdn.discordapp.com/attachments/1/2/a.png");
        let u = "https://media.discordapp.net/attachments/1/2/a.png?ex=1&is=2&hm=3&width=400&height=300";
        assert_eq!(cache_key(u), "https://media.discordapp.net/attachments/1/2/a.png?width=400&height=300");
        let u = "https://cdn.discordapp.com/avatars/1/abc.webp?size=64";
        assert_eq!(cache_key(u), u);
        assert_eq!(cache_key("https://cdn.discordapp.com/x.png"), "https://cdn.discordapp.com/x.png");
        // Same attachment with a refreshed signature hits the same file.
        let a = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1&is=2&hm=3&";
        let b = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=9&is=8&hm=7&";
        assert_eq!(fnv1a64(cache_key(a).as_bytes()), fnv1a64(cache_key(b).as_bytes()));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn host_allow_list() {
        let ok = |s: &str| host_allowed(&reqwest::Url::parse(s).unwrap());
        assert!(ok("https://cdn.discordapp.com/avatars/1/a.png"));
        assert!(ok("https://media.discordapp.net/stickers/1.gif"));
        assert!(ok("https://images-ext-1.discordapp.net/external/x/y.png"));
        assert!(ok("https://images-ext-12.discordapp.net/external/x/y.png"));
        assert!(ok("https://static.klipy.com/ii/x/y.gif"));
        assert!(ok("https://i.scdn.co/image/ab67616d0000b2734f8ef2c2c1b3a9a1c4d5e6f7"));
        assert!(!ok("http://i.scdn.co/image/x"));
        assert!(!ok("https://i.scdn.co.evil.example/image/x"));
        assert!(!ok("https://static.klipy.com.evil.example/a.gif"));
        assert!(!ok("http://static.klipy.com/a.gif"));
        assert!(!ok("http://cdn.discordapp.com/a.png"));
        assert!(!ok("https://cdn.discordapp.com:8443/a.png"));
        assert!(!ok("https://cdn.discordapp.com.evil.example/a.png"));
        assert!(!ok("https://images-ext-x.discordapp.net/a.png"));
        assert!(!ok("https://discord.com/api/v9/users/@me"));
        assert!(!ok("https://media.tenor.com/a.gif"));
    }

    #[test]
    fn decodes_and_downscales_png() {
        let img = RgbaImage::from_pixel(64, 32, Rgba([200, 100, 50, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).unwrap();
        let out = decode(&buf, 16).unwrap();
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.frames[0].image.size, [16, 8]);
        assert_eq!(out.frames[0].delay_ms, 0);
        assert_eq!(out.frames[0].image.pixels[0], egui::Color32::from_rgb(200, 100, 50));
        // Small images are not upscaled.
        assert_eq!(decode(&buf, 128).unwrap().frames[0].image.size, [64, 32]);
    }

    #[test]
    fn decodes_animated_gif() {
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for (i, ms) in [(0u8, 50u32), (1, 0), (2, 70)] {
                let img = RgbaImage::from_pixel(8, 8, Rgba([i * 100, 0, 0, 255]));
                enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
            }
        }
        let out = decode(&buf, 4).unwrap();
        let delays: Vec<u32> = out.frames.iter().map(|f| f.delay_ms).collect();
        assert_eq!(delays, [50, 100, 70]);
        assert!(out.frames.iter().all(|f| f.image.size == [4, 4]));
    }

    #[test]
    fn rejects_gif_with_huge_canvas() {
        // 35-byte GIF: logical screen `side`², 2-color palette, one 1x1 frame.
        let gif = |side: u16| {
            let mut g = b"GIF89a".to_vec();
            g.extend_from_slice(&side.to_le_bytes());
            g.extend_from_slice(&side.to_le_bytes());
            g.extend_from_slice(b"\x80\x00\x00\x00\x00\x00\xff\xff\xff");
            g.extend_from_slice(b"\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b");
            g
        };
        assert_eq!(decode(&gif(2), 64).unwrap().frames[0].image.size, [2, 2]);
        // 65535² would need ~17 GB per canvas buffer and abort the process.
        assert!(decode(&gif(65535), 64).is_err());
    }

    #[test]
    fn caps_frame_count_keeping_duration() {
        let list = (0..200).map(|_| (RgbaImage::new(2, 2), 30)).collect();
        let out = animation_frames(frames_of(list), 64).unwrap();
        assert!(out.len() <= MAX_ANIM_FRAMES);
        assert_eq!(out.iter().map(|f| f.delay_ms).sum::<u32>(), 200 * 30);
    }

    #[test]
    fn caps_frame_bytes() {
        // 6 x 1 MiB = 6 MiB > 4 MiB budget.
        let list = (0..6).map(|_| (RgbaImage::new(512, 512), 40)).collect();
        let out = animation_frames(frames_of(list), 4096).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().map(|f| f.delay_ms).sum::<u32>(), 240);
        // One frame alone over budget (1100² x 4 > 4 MiB): still first frame.
        let list = (0..3).map(|_| (RgbaImage::new(1100, 1100), 40)).collect();
        let out = animation_frames(frames_of(list), 4096).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].delay_ms, 0);
    }

    #[test]
    fn prune_removes_oldest_first() {
        let dir = std::env::temp_dir().join(format!("rustcord-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        for (name, age) in [("old", 300), ("mid", 200), ("new", 100)] {
            let path = dir.join(name);
            std::fs::write(&path, [0u8; 10]).unwrap();
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(now - Duration::from_secs(age)).unwrap();
        }
        prune(&dir, 40, 15); // under cap: nothing happens
        assert!(dir.join("old").exists());
        prune(&dir, 25, 15);
        assert!(!dir.join("old").exists() && !dir.join("mid").exists());
        assert!(dir.join("new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
