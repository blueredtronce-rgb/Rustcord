//! VoiceMessages (playback only; sending them is left out, the official desktop app records
//! none): a voice message's .ogg attachment gets a play button. A click downloads the file
//! from Discord's CDN (no token; what the official client fetches for the same click), and
//! a thread demuxes the Ogg pages and decodes the Opus packets into the chosen speaker as it
//! plays. One message plays at a time; memory: the compressed file (capped) and ~0.3 s of PCM.

use super::{Plugin, enabled, opt_int};
use crate::model::{Attachment, Id, Message};
use egui::{Color32, FontId, Rect, Sense, Stroke, Vec2, pos2, vec2};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Message flag IS_VOICE_MESSAGE.
const VOICE_MESSAGE: u64 = 1 << 13;
/// Larger files are refused (a 20 min message at 32 kbit/s is ~5 MB).
const MAX_BYTES: usize = 8 << 20;
const RATE: u32 = 48_000;

struct Playing {
    attachment: Id,
    stop: Arc<AtomicBool>,
    /// 48 kHz frames played so far, and the total (0 until known).
    pos: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
}

static NOW: Mutex<Option<Playing>> = Mutex::new(None);

pub fn is_voice_message(m: &Message, a: &Attachment) -> bool {
    (m.flags & VOICE_MESSAGE != 0 || a.content_type.as_deref().is_some_and(|c| c.starts_with("audio/ogg"))) && a.filename.to_ascii_lowercase().ends_with(".ogg")
}

fn cdn_url(url: &str) -> bool {
    url.starts_with("https://cdn.discordapp.com/") || url.starts_with("https://media.discordapp.net/")
}

/// The player in place of a voice message's file chip (`messages.rs` attachments); plays on
/// `output` (Voice & Video's output device, else the default). True when it was drawn.
pub fn player(ui: &mut egui::Ui, m: &Message, a: &Attachment, output: Option<&str>) -> bool {
    if !enabled(Plugin::VoiceMessages) || !is_voice_message(m, a) || !cdn_url(&a.url) {
        return false;
    }
    let v = ui.visuals().clone();
    let (accent, text, muted) = (v.selection.bg_fill, v.text_color(), v.weak_text_color());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().min(320.0), 48.0), Sense::hover());
    ui.painter().rect(rect, 8, v.extreme_bg_color, Stroke::new(1.0, muted.gamma_multiply(0.2)), egui::StrokeKind::Inside);
    let (playing, pos, total, error) = {
        let now = NOW.lock().unwrap_or_else(|e| e.into_inner());
        match now.as_ref().filter(|p| p.attachment == a.id) {
            Some(p) => (!p.stop.load(Ordering::Relaxed), p.pos.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed), p.error.lock().ok().and_then(|e| e.clone())),
            None => (false, 0, 0, None),
        }
    };
    let button = Rect::from_center_size(pos2(rect.left() + 24.0, rect.center().y), Vec2::splat(32.0));
    let resp = ui.interact(button, ui.make_persistent_id(("rc_voice_msg", a.id)), Sense::click());
    ui.painter().circle_filled(button.center(), 16.0, if resp.hovered() { accent.gamma_multiply(0.85) } else { accent });
    let c = button.center();
    if playing {
        for dx in [-4.0, 4.0] {
            ui.painter().rect_filled(Rect::from_center_size(c + vec2(dx, 0.0), vec2(4.0, 12.0)), 1, Color32::WHITE);
        }
    } else {
        let tri = vec![c + vec2(-4.0, -7.0), c + vec2(8.0, 0.0), c + vec2(-4.0, 7.0)];
        ui.painter().add(egui::Shape::convex_polygon(tri, Color32::WHITE, Stroke::NONE));
    }
    let tip = if playing { "Stop" } else { "Play" };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        if playing { stop() } else { play(a.id, a.url.clone(), output.map(str::to_owned)) }
    }
    let label = match &error {
        Some(e) => e.clone(),
        None if total > 0 => format!("{} / {}", clock(pos), clock(total)),
        None if playing => "Loading…".into(),
        None => "Voice message".into(),
    };
    let g = ui.painter().layout_no_wrap(label, FontId::proportional(11.5), if error.is_some() { v.error_fg_color } else { text });
    ui.painter().galley(pos2(rect.right() - 12.0 - g.size().x, rect.center().y - g.size().y / 2.0), g.clone(), text);
    // Progress line between the button and the time.
    let line = Rect::from_min_max(pos2(button.right() + 12.0, rect.center().y - 2.0), pos2(rect.right() - 24.0 - g.size().x, rect.center().y + 2.0));
    ui.painter().rect_filled(line, 2, muted.gamma_multiply(0.3));
    if total > 0 {
        let f = (pos as f32 / total as f32).clamp(0.0, 1.0);
        ui.painter().rect_filled(Rect::from_min_size(line.min, vec2(line.width() * f, line.height())), 2, accent);
    }
    if playing {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    true
}

fn clock(frames: u64) -> String {
    let s = frames / u64::from(RATE);
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn stop() {
    if let Some(p) = NOW.lock().unwrap_or_else(|e| e.into_inner()).take() {
        p.stop.store(true, Ordering::Relaxed);
    }
}

fn play(attachment: Id, url: String, output: Option<String>) {
    stop();
    let p = Playing { attachment, stop: Arc::default(), pos: Arc::default(), total: Arc::default(), error: Arc::default() };
    let (stop_flag, pos, total, error) = (p.stop.clone(), p.pos.clone(), p.total.clone(), p.error.clone());
    *NOW.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    let volume = opt_int(Plugin::VoiceMessages, "volume") as f32 / 100.0;
    let spawned = std::thread::Builder::new().name("rustcord-voice-msg".into()).spawn(move || {
        if let Err(e) = run(&url, &stop_flag, &pos, &total, volume, output.as_deref()) {
            *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
        }
        stop_flag.store(true, Ordering::Relaxed);
    });
    if spawned.is_err() {
        stop();
    }
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let mut resp = reqwest::Client::new().get(url).send().await.and_then(|r| r.error_for_status()).map_err(|_| "Could not load".to_owned())?;
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| "Could not load".to_owned())? {
            if out.len() + chunk.len() > MAX_BYTES {
                return Err("Too large".to_owned());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    })
}

/// Opus packets of an Ogg stream (packets spanning pages joined), and the last granule
/// position (48 kHz samples incl. the pre-skip).
pub fn ogg_packets(data: &[u8]) -> Result<(Vec<Vec<u8>>, u64), String> {
    let (mut packets, mut cur, mut granule, mut i) = (Vec::new(), Vec::new(), 0u64, 0usize);
    while i + 27 <= data.len() {
        if &data[i..i + 4] != b"OggS" {
            return Err("Not an Ogg file".into());
        }
        let g = u64::from_le_bytes(data[i + 6..i + 14].try_into().map_err(|_| "Bad page")?);
        if g != u64::MAX {
            granule = g;
        }
        let n = usize::from(data[i + 26]);
        let lacing = data.get(i + 27..i + 27 + n).ok_or("Truncated page")?;
        let mut at = i + 27 + n;
        for &len in lacing {
            let seg = data.get(at..at + usize::from(len)).ok_or("Truncated page")?;
            cur.extend_from_slice(seg);
            at += usize::from(len);
            if len < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        i = at;
    }
    Ok((packets, granule))
}

fn run(url: &str, stop: &AtomicBool, pos: &AtomicU64, total: &AtomicU64, volume: f32, output: Option<&str>) -> Result<(), String> {
    let data = download(url)?;
    let (packets, granule) = ogg_packets(&data)?;
    drop(data);
    let head = packets.first().filter(|p| p.starts_with(b"OpusHead") && p.len() >= 19).ok_or("Not an Opus file")?;
    let pre_skip = u64::from(u16::from_le_bytes([head[10], head[11]]));
    total.store(granule.saturating_sub(pre_skip), Ordering::Relaxed);
    let mut dec = opus_rs::OpusDecoder::new(RATE as i32, 2).map_err(|_| "Opus decoder")?;

    // The picked output (named as in voice::audio::device), else the system default; the
    // speaker code is shared with the video player.
    let mut out = crate::media::audio::AudioOut::open(output, RATE)?;
    out.set_volume(volume);
    let played = |out: &crate::media::audio::AudioOut| out.played_frames() * u64::from(RATE) / u64::from(out.rate().max(1));
    let target = out.rate() as usize * 3 / 10;
    let mut pcm = vec![0f32; 5760 * 2];
    let mut skip = pre_skip as usize;
    for p in packets.iter().skip(2) {
        let n = dec.decode(p, 5760, &mut pcm).map_err(|_| "Could not decode")?;
        let start = skip.min(n);
        skip -= start;
        // Wait while the speaker has enough queued.
        while out.queued_frames() > target {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            pos.store(played(&out), Ordering::Relaxed);
        }
        out.push(&pcm[2 * start..2 * n], 2);
    }
    // Let the queue drain.
    while out.queued_frames() > 0 && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(30));
        pos.store(played(&out), Ordering::Relaxed);
    }
    pos.store(total.load(Ordering::Relaxed), Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(granule: u64, segs: &[&[u8]], lacing_extra: &[u8]) -> Vec<u8> {
        let mut lacing: Vec<u8> = Vec::new();
        for s in segs {
            let mut n = s.len();
            while n >= 255 {
                lacing.push(255);
                n -= 255;
            }
            lacing.push(n as u8);
        }
        lacing.extend_from_slice(lacing_extra);
        let mut p = b"OggS".to_vec();
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&[0; 12]);
        p.push(lacing.len() as u8);
        p.extend_from_slice(&lacing);
        for s in segs {
            p.extend_from_slice(s);
        }
        p
    }

    #[test]
    fn ogg_pages_split_into_packets() {
        let big = vec![7u8; 300];
        let mut data = page(0, &[b"OpusHead0123456789xx", b"OpusTags"], &[]);
        data.extend(page(960, &[b"abc", &big], &[]));
        let (packets, granule) = ogg_packets(&data).unwrap();
        assert_eq!(packets.len(), 4);
        assert_eq!(packets[2], b"abc");
        assert_eq!(packets[3].len(), 300);
        assert_eq!(granule, 960);
        assert!(ogg_packets(b"nope and more bytes to fill twenty seven").is_err());
        assert_eq!(clock(48_000 * 75), "1:15");
        let m = Message { flags: VOICE_MESSAGE, ..Default::default() };
        let a = Attachment { filename: "voice-message.ogg".into(), ..Default::default() };
        assert!(is_voice_message(&m, &a));
        assert!(!cdn_url("https://evil.example/x.ogg"));
    }
}
