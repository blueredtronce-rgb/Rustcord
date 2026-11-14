//! Fonts beyond the theme's own files (docs/FONTS_NOTES.md):
//! - gg sans, Discord's UI face: fetched at runtime from discord.com into the cache dir
//!   (`fonts/gg-sans/<hash>.woff2`, content-hashed, so a deploy's new files sit beside the old)
//!   and decoded in memory at every start. Never bundled, converted on disk or shared (§1.4).
//!   The first frame uses what is cached; a background thread fetches the rest once per run
//!   and re-applies the fonts. Off (`Settings::use_discord_font`) or unavailable: Segoe UI.
//! - the system fallback chain: stock fonts (Windows, macOS; common ones on Linux),
//!   memory-mapped, asked for characters no configured face has (decorated channel / role
//!   names: box drawing, `・`, Yi, Canadian syllabics …).

use super::ThemeFont;
use egui::FontData;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// (weight, asset hash): regular, medium, semibold, bold. The seed of `manifest.json`, which a
/// rediscovery rewrites when a hash stops working.
const SEED: [(u16, &str); 4] = [(400, "66d715454104d24e"), (500, "b272b33815319bae"), (600, "2df2c3ff74408972"), (700, "189422196a4f8b53")];
const ORIGIN: &str = "https://discord.com";
/// A gg sans woff2 is ~40 KB.
const MAX_WOFF2: usize = 512 * 1024;

pub(super) struct Gg {
    /// The setting.
    on: bool,
    /// Decoded faces by weight.
    faces: Vec<(u16, FontData)>,
    fetching: bool,
    /// Bumped whenever `faces()` changes (part of `ThemeApplier`'s font key).
    pub generation: u32,
    /// The theme font last applied: re-applied with the faces when the download finishes.
    pub applied: Option<ThemeFont>,
}

impl Gg {
    /// The faces to use: none while the setting is off.
    pub fn faces(&self) -> &[(u16, FontData)] {
        if self.on { &self.faces } else { &[] }
    }
}

static GG: Mutex<Gg> = Mutex::new(Gg { on: false, faces: Vec::new(), fetching: false, generation: 0, applied: None });

pub(super) fn gg() -> MutexGuard<'static, Gg> {
    GG.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Startup (before the first `ThemeApplier::apply`): the system fallback chain, the box rules,
/// and gg sans when `discord_font`.
pub fn init_fonts(ctx: &egui::Context, discord_font: bool) {
    ctx.add_font_provider(std::sync::Arc::new(system_fallback));
    ctx.add_glyph_rasterizer(egui::GlyphRasterizer::new("rustcord::box_rules", box_rule).with_priority(egui::FontPriority::Highest));
    set_discord_font(ctx, discord_font);
}

/// Horizontal box-drawing characters as (left half, right half, dashes per cell); weights
/// 1 light, 2 heavy, 3 double, 0 none.
fn rule_parts(c: char) -> Option<(u8, u8, u8)> {
    Some(match c {
        '─' => (1, 1, 0),
        '━' => (2, 2, 0),
        '═' => (3, 3, 0),
        '╌' => (1, 1, 2),
        '╍' => (2, 2, 2),
        '┄' => (1, 1, 3),
        '┅' => (2, 2, 3),
        '┈' => (1, 1, 4),
        '┉' => (2, 2, 4),
        '╴' => (1, 0, 0),
        '╶' => (0, 1, 0),
        '╸' => (2, 0, 0),
        '╺' => (0, 2, 0),
        '╼' => (1, 2, 0),
        '╾' => (2, 1, 0),
        _ => return None,
    })
}

/// Draws horizontal box rules (`━━» TEAM «━━`) in every proportional family, before any font:
/// gg sans' rules are dashes 10 % short of their advance, and epaint snaps each glyph to the
/// pixel, so a run of them shows gaps (Chromium's are continuous). Each cell is as wide as the
/// largest rounded step between two of them, so a run never has a seam. Segoe UI Symbol's
/// geometry (0.5 em cell, axis 0.26 em up), so its corners (`╭ ╮`, `append_boxed`) still meet
/// them. Monospace keeps the font's own cells (code-block trees stay aligned).
fn box_rule(req: &egui::GlyphRasterizerRequest<'_>) -> Option<egui::RasterizedGlyph> {
    let mut chars = req.cluster.chars();
    let (left, right, dashes) = chars.next().and_then(rule_parts).filter(|_| chars.next().is_none())?;
    if *req.family == egui::FontFamily::Monospace {
        return None;
    }
    let size = req.font_size_px;
    let advance = size * 0.5;
    let w = advance.ceil().max(1.0) as usize;
    let light = (size * 0.065).round().max(1.0);
    let heavy = (size * 0.105).round().max(light + 1.0);
    let axis = -size * 0.26;
    // Rows (top, height) of one half's weight, relative to the baseline.
    let bands = |weight: u8| -> Vec<(f32, f32)> {
        match weight {
            0 => vec![],
            2 => vec![((axis - heavy / 2.0).round(), heavy)],
            3 => {
                let top = (axis - 1.5 * light).round();
                vec![(top, light), (top + 2.0 * light, light)]
            }
            _ => vec![((axis - light / 2.0).round(), light)],
        }
    };
    let (lb, rb) = (bands(left), bands(right));
    let top = lb.iter().chain(&rb).map(|b| b.0).fold(f32::INFINITY, f32::min);
    let bottom = lb.iter().chain(&rb).map(|b| b.0 + b.1).fold(f32::NEG_INFINITY, f32::max);
    let h = (bottom - top) as usize;
    let mid = w / 2;
    let seg = w as f32 / f32::from(dashes.max(1));
    let gap = if dashes > 0 { (seg * 0.35).round().max(1.0) } else { 0.0 };
    let mut image = egui::ColorImage::filled([w, h], egui::Color32::TRANSPARENT);
    for (halves, x_range) in [(&lb, 0..mid), (&rb, mid..w)] {
        for &(band_top, band_h) in halves.iter() {
            let y0 = (band_top - top) as usize;
            for x in x_range.clone() {
                // Dashed rules: a gap at the end of each segment.
                if gap > 0.0 && (x as f32 % seg) >= seg - gap {
                    continue;
                }
                for y in y0..y0 + band_h as usize {
                    image[(x, y)] = egui::Color32::WHITE;
                }
            }
        }
    }
    let bitmap = egui::GlyphBitmap { image, offset_px: egui::vec2(0.0, top), is_color: false };
    Some(egui::RasterizedGlyph { bitmap, advance_px: advance })
}

/// The "Discord font" switch: on uses the cached faces at once and fetches missing ones in the
/// background (not in offline mode); off goes back to Segoe UI.
pub fn set_discord_font(ctx: &egui::Context, on: bool) {
    let mut gg = gg();
    if gg.on == on {
        return;
    }
    gg.on = on;
    if on && gg.faces.is_empty() {
        let (list, dir) = (manifest(), dir());
        gg.faces = list.iter().filter_map(|(w, hash)| Some((*w, decode(&std::fs::read(dir.join(file(hash))).ok()?)?))).collect();
        if gg.faces.len() < list.len() && !gg.fetching && !crate::config::offline_mode() {
            gg.fetching = spawn_fetch(ctx.clone(), list, dir);
        }
    }
    gg.generation += 1;
    reapply(ctx, &gg);
}

/// Installs the fonts again with the current faces (no-op before the first apply).
fn reapply(ctx: &egui::Context, gg: &Gg) {
    if let Some(font) = &gg.applied {
        ctx.set_fonts(super::apply::build_fonts(font, gg.faces()));
    }
}

fn dir() -> PathBuf {
    crate::config::paths().cache_dir.join("fonts").join("gg-sans")
}

fn file(hash: &str) -> String {
    format!("{hash}.woff2")
}

/// An asset hash fit for a URL and a file name.
fn plain_hash(h: &str) -> bool {
    (8..=64).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The cached manifest (after a rediscovery), else the seed.
fn manifest() -> Vec<(u16, String)> {
    let saved: Option<Vec<(u16, String)>> = std::fs::read(dir().join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    saved
        .filter(|l| !l.is_empty() && l.iter().all(|(_, h)| plain_hash(h)))
        .unwrap_or_else(|| SEED.iter().map(|&(w, h)| (w, h.to_owned())).collect())
}

/// woff2 → sfnt in memory; only a sane font named "gg sans" passes (epaint must never get a
/// font it cannot parse).
fn decode(woff2: &[u8]) -> Option<FontData> {
    use skrifa::MetadataProvider;
    if !woff2.starts_with(b"wOF2") {
        return None;
    }
    let ttf = wuff::decompress_woff2(woff2).ok()?;
    let font = skrifa::FontRef::new(&ttf).ok()?;
    let family = font.localized_strings(skrifa::string::StringId::FAMILY_NAME).english_or_first()?.chars().collect::<String>();
    (family.starts_with("gg sans") && super::apply::looks_like_font(&ttf)).then(|| FontData::from_owned(ttf))
}

/// Fetches the missing faces on a thread of its own, then installs them. False if it could not start.
fn spawn_fetch(ctx: egui::Context, list: Vec<(u16, String)>, dir: PathBuf) -> bool {
    let run = move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let faces = rt.ok().and_then(|rt| rt.block_on(fetch(&list, &dir)));
        let mut gg = gg();
        gg.fetching = false;
        let Some(faces) = faces.filter(|f| f.len() > gg.faces.len()) else { return };
        gg.faces = faces;
        gg.generation += 1;
        if gg.on {
            reapply(&ctx, &gg);
            ctx.request_repaint();
        }
    };
    std::thread::Builder::new().name("rc-fonts".into()).spawn(run).is_ok()
}

/// All faces of `list` (cached or downloaded); after a miss (404, a file that does not decode)
/// one rediscovery from the web app's CSS, saved as the manifest. One try per run.
async fn fetch(list: &[(u16, String)], dir: &Path) -> Option<Vec<(u16, FontData)>> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().ok()?;
    let mut faces = get_all(&http, dir, list).await;
    if faces.len() < list.len()
        && let Some(found) = rediscover(&http).await
    {
        if let Ok(json) = serde_json::to_vec(&found)
            && let Err(e) = crate::config::write_atomic(&dir.join("manifest.json"), &json)
        {
            eprintln!("rustcord: font manifest not saved: {e}");
        }
        faces = get_all(&http, dir, &found).await;
    }
    (!faces.is_empty()).then_some(faces)
}

async fn get_all(http: &reqwest::Client, dir: &Path, list: &[(u16, String)]) -> Vec<(u16, FontData)> {
    let mut out = Vec::new();
    for (w, hash) in list {
        let cached = std::fs::read(dir.join(file(hash))).ok().and_then(|b| decode(&b));
        let face = match cached {
            Some(f) => Some(f),
            None => download(http, dir, hash).await,
        };
        out.extend(face.map(|f| (*w, f)));
    }
    out
}

/// `GET https://discord.com/assets/<hash>.woff2`, kept as is (the original file) once it decodes.
async fn download(http: &reqwest::Client, dir: &Path, hash: &str) -> Option<FontData> {
    let resp = http.get(format!("{ORIGIN}/assets/{}", file(hash))).send().await.ok()?;
    if !resp.status().is_success() || resp.content_length().is_some_and(|l| l > MAX_WOFF2 as u64) {
        return None;
    }
    let bytes = resp.bytes().await.ok().filter(|b| b.len() <= MAX_WOFF2)?;
    let face = decode(&bytes)?;
    if let Err(e) = crate::config::write_atomic(&dir.join(file(hash)), &bytes) {
        eprintln!("rustcord: font cache write failed: {e}");
    }
    Some(face)
}

/// The current gg sans hashes from the web app: its stylesheets, newest last (the font sheet
/// is second-to-last today, so ~2 requests).
async fn rediscover(http: &reqwest::Client) -> Option<Vec<(u16, String)>> {
    let html = http.get(format!("{ORIGIN}/app")).send().await.ok()?.text().await.ok()?;
    let sheets: Vec<&str> = html.split("<link href=\"").skip(1).filter_map(|s| s.split('"').next()).filter(|h| h.starts_with("/assets/") && h.ends_with(".css")).collect();
    for css in sheets.iter().rev().take(6) {
        let body = http.get(format!("{ORIGIN}{css}")).send().await.ok()?.text().await.ok()?;
        if body.contains("font-family:gg sans;") {
            let found = faces_in_css(&body);
            return (!found.is_empty()).then_some(found);
        }
    }
    None
}

/// (weight, hash) of the upright gg sans `@font-face` rules we use.
fn faces_in_css(css: &str) -> Vec<(u16, String)> {
    let mut out: Vec<(u16, String)> = css
        .split("@font-face{")
        .skip(1)
        .filter_map(|r| {
            let r = r.split('}').next()?;
            let val = |k: &str| r.split(k).nth(1)?.split([';', '}']).next();
            (val("font-family:")? == "gg sans" && val("font-style:")? == "normal").then_some(())?;
            let weight: u16 = val("font-weight:")?.parse().ok()?;
            let hash = r.split("url(\"/assets/").nth(1)?.split(".woff2").next()?;
            (SEED.iter().any(|&(w, _)| w == weight) && plain_hash(hash)).then(|| (weight, hash.to_owned()))
        })
        .collect();
    out.sort();
    out.dedup_by_key(|f| f.0);
    out
}

/// Stock Windows fonts, in cmap-scan order (FONTS_NOTES §2.2), for characters no configured
/// font has.
#[cfg(windows)]
const FALLBACK: &[(&str, u32)] = &[
    ("seguisym.ttf", 0), ("YuGothM.ttc", 1), ("malgun.ttf", 0), ("msyh.ttc", 1), ("msjh.ttc", 1),
    ("Nirmala.ttc", 0), ("Nirmala.ttf", 0), ("LeelawUI.ttf", 0), ("ebrima.ttf", 0), ("gadugi.ttf", 0),
    ("seguihis.ttf", 0), ("msyi.ttf", 0), ("javatext.ttf", 0), ("himalaya.ttf", 0), ("mmrtext.ttf", 0),
    ("monbaiti.ttf", 0), ("taile.ttf", 0), ("ntailu.ttf", 0), ("phagspa.ttf", 0), ("sylfaen.ttf", 0),
    ("simsunb.ttf", 0), ("SimsunExtG.ttf", 0), ("SansSerifCollection.ttf", 0), ("cambria.ttc", 1),
    ("l_10646.ttf", 0), ("micross.ttf", 0), ("tahoma.ttf", 0),
];

/// Stock macOS fonts (absolute paths; missing ones are skipped).
#[cfg(target_os = "macos")]
const FALLBACK: &[(&str, u32)] = &[
    ("/System/Library/Fonts/Apple Symbols.ttf", 0),
    ("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
    ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
    ("/System/Library/Fonts/AppleSDGothicNeo.ttc", 0),
];

/// Common Linux fonts (Debian / Ubuntu / Fedora / Arch paths; missing ones are skipped).
#[cfg(not(any(windows, target_os = "macos")))]
const FALLBACK: &[(&str, u32)] = &[
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/freefont/FreeSerif.ttf", 0),
];

/// egui font provider: the first stock font whose cmap has the character (asked once per family
/// and character; the files are mapped once per process and shared).
fn system_fallback(req: &egui::FallbackRequest<'_>) -> Option<egui::FontInsert> {
    let c = req.cluster.chars().next()?;
    let hit = FALLBACK.iter().find_map(|&(file, index)| {
        let mut data = super::apply::system_font(file)?;
        has_glyph(data.bytes(), index, c).then_some(())?;
        data.index = index;
        Some(egui::FontInsert::new(&format!("sys:{file}#{index}"), data, vec![]))
    });
    #[cfg(debug_assertions)]
    if hit.is_none() {
        eprintln!("rustcord: no system font for U+{:04X}", c as u32);
    }
    hit
}

fn has_glyph(font: &[u8], index: u32, c: char) -> bool {
    use skrifa::MetadataProvider;
    skrifa::FontRef::from_index(font, index).is_ok_and(|f| f.charmap().map(c).is_some_and(|g| g.to_u32() != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_upright_faces_in_minified_css() {
        let css = concat!(
            "a{}@font-face{font-display:swap;font-family:gg sans;font-style:normal;font-weight:400;src:url(\"/assets/66d715454104d24e.woff2\") format(\"woff2\")}",
            "@font-face{font-display:swap;font-family:gg sans;font-style:italic;font-weight:400;src:url(\"/assets/dd24010f3cf7def7.woff2\") format(\"woff2\")}",
            "@font-face{font-family:gg sans;font-style:normal;font-weight:700;src:url(\"/assets/189422196a4f8b53.woff2\") format(\"woff2\")}",
            "@font-face{font-family:gg sans;font-style:normal;font-weight:800;src:url(\"/assets/b2fdbe507d6ce9ef.woff2\") format(\"woff2\")}",
            "@font-face{font-family:gg mono;font-style:normal;font-weight:400;src:url(\"/assets/249d0a057895c668.woff2\") format(\"woff2\")}",
            "@font-face{font-family:gg sans;font-style:normal;font-weight:500;src:url(\"/assets/../../evil.woff2\") format(\"woff2\")}",
        );
        assert_eq!(faces_in_css(css), vec![(400, "66d715454104d24e".to_owned()), (700, "189422196a4f8b53".to_owned())]);
    }

    #[test]
    fn rejects_what_is_not_gg_sans() {
        assert!(decode(b"").is_none());
        assert!(decode(b"wOF2 not really").is_none());
        // A real font that is not gg sans (Segoe UI) is refused too.
        #[cfg(windows)]
        if let Some(f) = super::super::apply::system_font("segoeui.ttf") {
            assert!(decode(f.bytes()).is_none());
        }
    }

    #[cfg(windows)]
    #[test]
    fn fallback_covers_decorated_names() {
        // Box drawing, katakana middle dot, CJK vertical bar, Yi, Canadian syllabics, math bold.
        for c in ['┊', '・', '︱', '꒰', 'ᘏ', '𝐛', '〢'] {
            let req = egui::FallbackRequest { cluster: &c.to_string(), family: &egui::FontFamily::Proportional };
            let hit = system_fallback(&req).unwrap_or_else(|| panic!("no font for {c}"));
            assert!(has_glyph(hit.data.bytes(), hit.data.index, c), "{c}: {}", hit.name);
        }
    }

    #[test]
    fn box_rules_draw_continuous_runs() {
        use egui::epaint::text::{FontDefinitions, Fonts, LayoutJob, TextFormat, TextOptions};
        let rules = egui::GlyphRasterizer::new("test::box_rules", box_rule).with_priority(egui::FontPriority::Highest);
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default()).with_glyph_rasterizer(rules);
        for ppp in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for size in [11.0, 12.5, 14.0, 16.0] {
                let mut job = LayoutJob::default();
                job.append("\u{bb}\u{2501}\u{2501}\u{2501}\u{2500}\u{2500}\u{2550}\u{2550}", 0.0, TextFormat::simple(egui::FontId::proportional(size), egui::Color32::WHITE));
                let g = fonts.with_pixels_per_point(ppp).layout_job(job);
                let ink: Vec<(f32, f32)> = g.rows[0].glyphs[1..].iter().map(|gl| (gl.pos.x + gl.uv_rect.offset.x, gl.pos.x + gl.uv_rect.offset.x + gl.uv_rect.size.x)).collect();
                for pair in ink.windows(2) {
                    assert!(pair[0].1 >= pair[1].0 - 1e-3, "gap at ppp {ppp}, size {size}: {ink:?}");
                }
            }
        }
        // Not a rule, more than one character, or monospace (code-block trees): the fonts draw it.
        let req = |cluster: &'static str, family: &'static egui::FontFamily| egui::GlyphRasterizerRequest { cluster, family, font_size_px: 17.5, subpixel_offset_px: 0.0 };
        assert!(box_rule(&req("a", &egui::FontFamily::Proportional)).is_none());
        assert!(box_rule(&req("\u{2502}", &egui::FontFamily::Proportional)).is_none());
        assert!(box_rule(&req("\u{2500}\u{301}", &egui::FontFamily::Proportional)).is_none());
        assert!(box_rule(&req("\u{2500}", &egui::FontFamily::Monospace)).is_none());
        // Heavy is thicker than light; the double rule is two lines.
        let rows = |c: &'static str| {
            let g = box_rule(&req(c, &egui::FontFamily::Proportional)).unwrap().bitmap.image;
            (0..g.size[1]).filter(|&y| g[(0, y)].a() > 0).count()
        };
        assert!(rows("\u{2501}") > rows("\u{2500}"));
        assert_eq!(rows("\u{2550}"), 2 * rows("\u{2500}"));
    }
}

