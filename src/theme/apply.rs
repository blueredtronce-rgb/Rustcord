//! Turning a `Theme` into egui fonts, visuals, text styles and spacing.

use super::{BOLD_FAMILY, Rgba, Theme, ThemeApplier, ThemeColors, ThemeFont};
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke, TextStyle, vec2};
use std::path::Path;
use std::sync::Arc;

impl ThemeApplier {
    pub fn apply(&mut self, ctx: &egui::Context, theme: &Theme) {
        // Locked across set_fonts: the gg sans download thread re-applies under the same lock.
        let mut gg = super::fonts::gg();
        gg.applied = Some(theme.font.clone());
        let key = (theme.font.ui_font.clone(), theme.font.mono_font.clone(), gg.generation);
        if self.fonts_key.as_ref() != Some(&key) {
            ctx.set_fonts(build_fonts(&theme.font, gg.faces()));
            self.fonts_key = Some(key);
        }
        drop(gg);
        if self.gradient != theme.gradient {
            self.gradient = theme.gradient.clone();
            self.gradient_mesh = None;
        }

        let which = if theme.dark { egui::Theme::Dark } else { egui::Theme::Light };
        ctx.set_theme(which);
        let visuals = visuals(theme);
        let size = if theme.font.size.is_finite() { theme.font.size.clamp(6.0, 48.0) } else { 14.5 };
        let compact = theme.layout.compact;
        ctx.style_mut_of(which, |s| {
            s.visuals = visuals;
            let prop = |k: f32| FontId::proportional(size * k);
            s.text_styles.insert(TextStyle::Body, prop(1.0));
            s.text_styles.insert(TextStyle::Button, prop(1.0));
            s.text_styles.insert(TextStyle::Small, prop(0.8));
            s.text_styles.insert(TextStyle::Monospace, FontId::monospace(size * 0.92));
            s.text_styles.insert(TextStyle::Heading, FontId::new(size * 1.3, super::bold_family()));

            let sp = &mut s.spacing;
            if compact {
                sp.item_spacing = vec2(6.0, 3.0);
                sp.button_padding = vec2(6.0, 2.0);
                sp.interact_size = vec2(32.0, 18.0);
                sp.window_margin = egui::Margin::same(8);
                sp.menu_margin = egui::Margin::same(4);
                sp.indent = 14.0;
            } else {
                sp.item_spacing = vec2(8.0, 6.0);
                sp.button_padding = vec2(10.0, 5.0);
                sp.interact_size = vec2(40.0, 22.0);
                sp.window_margin = egui::Margin::same(12);
                sp.menu_margin = egui::Margin::same(6);
                sp.indent = 18.0;
            }
        });
    }
}

fn visuals(theme: &Theme) -> egui::Visuals {
    let c = &theme.colors;
    let mut v = if theme.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let radius = CornerRadius::same(theme.layout.corner_radius.clamp(0.0, 255.0).round() as u8);
    let text = c.text.to_color32();
    let accent = c.accent.to_color32();
    let strong = is_high_contrast(c);

    let border = if strong { c.text_muted.to_color32() } else { mix(c.surface, c.text, 0.14) };
    let border_width = if strong { 1.5 } else { 1.0 };
    let (button, hover) = if theme.gradient.is_some() {
        // Translucent text tint: the same as the opaque mix on `surface`, and it lets the
        // gradient through on the translucent panels (selected rows, buttons).
        let tint = |a: f32| c.text.to_color32().gamma_multiply(a);
        (tint(0.06), tint(0.13))
    } else {
        (mix(c.surface, c.text, if strong { 0.12 } else { 0.06 }), mix(c.surface, c.text, 0.13))
    };

    v.override_text_color = None;
    v.weak_text_color = Some(c.text_muted.to_color32());
    v.hyperlink_color = c.link.to_color32();
    v.panel_fill = c.background.to_color32();
    v.window_fill = c.surface.to_color32();
    v.window_stroke = Stroke::new(border_width, border);
    v.extreme_bg_color = c.surface.to_color32();
    v.text_edit_bg_color = None;
    v.faint_bg_color = mix(c.background, c.text, 0.04);
    v.code_bg_color = c.code_background.to_color32();
    v.error_fg_color = c.danger.to_color32();
    v.window_corner_radius = radius;
    v.menu_corner_radius = radius;
    let shadow_alpha = if theme.dark { 72 } else { 28 };
    v.window_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
    v.popup_shadow = Shadow { offset: [0, 3], blur: 10, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
    v.text_cursor.stroke = Stroke::new(2.0, accent);

    // Selected text / selected buttons: translucent accent, text tinted towards the accent so
    // it stays readable; the same stroke outlines a focused text field.
    v.selection.bg_fill = accent.gamma_multiply(0.42);
    v.selection.stroke = Stroke::new(1.0, mix(c.text, c.accent, 0.35));

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c.background.to_color32();
    w.noninteractive.weak_bg_fill = c.background.to_color32();
    w.noninteractive.bg_stroke = Stroke::new(border_width, border); // separators
    w.noninteractive.fg_stroke = Stroke::new(1.0, text); // label text

    w.inactive.bg_fill = button;
    w.inactive.weak_bg_fill = button;
    w.inactive.bg_stroke = Stroke::new(border_width, border);
    w.inactive.fg_stroke = Stroke::new(1.0, text);

    w.hovered.bg_fill = hover;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(border_width, mix(c.text_muted, c.accent, 0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, text);

    w.active.bg_fill = accent;
    w.active.weak_bg_fill = accent;
    w.active.bg_stroke = Stroke::new(border_width, accent);
    w.active.fg_stroke = Stroke::new(2.0, c.accent_text.to_color32());

    w.open.bg_fill = hover;
    w.open.weak_bg_fill = hover;
    w.open.bg_stroke = Stroke::new(border_width, border);
    w.open.fg_stroke = Stroke::new(1.0, text);

    for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        wv.corner_radius = radius;
        wv.expansion = 0.0;
    }
    v
}

/// Themes whose secondary text is nearly as strong as the main text want strong borders too.
pub(crate) fn is_high_contrast(c: &ThemeColors) -> bool {
    contrast_ratio(c.text_muted, c.background) >= 9.0
}

pub(super) fn contrast_ratio(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// WCAG relative luminance.
pub(super) fn luminance(c: Rgba) -> f32 {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.0[0]) + 0.7152 * lin(c.0[1]) + 0.0722 * lin(c.0[2])
}

/// Opaque sRGB blend `a -> b` (alpha of `a`).
fn mix(a: Rgba, b: Rgba, t: f32) -> Color32 {
    let ch = |i: usize| (f32::from(a.0[i]) + (f32::from(b.0[i]) - f32::from(a.0[i])) * t).round() as u8;
    Color32::from_rgba_unmultiplied(ch(0), ch(1), ch(2), a.0[3])
}

/// egui's default fonts (keeping the emoji/icon fallbacks) with nicer primary faces:
/// the theme's own font files, else gg sans (`gg`: decoded faces by weight, fonts.rs) in front
/// of Segoe UI / Consolas on Windows, else egui's built-ins. Medium / semibold text has its own
/// families (`medium_family`, `semibold_family`), the regular / bold chains without gg sans.
pub(super) fn build_fonts(font: &ThemeFont, gg: &[(u16, FontData)]) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let custom = custom_font(&font.ui_font);
    // A theme's own UI font wins over gg sans.
    let gg = if custom.is_some() { &[][..] } else { gg };
    let (ui, bold) = match custom {
        // shortcut: a custom UI font has no bold face; bold text uses the regular face.
        Some(data) => (Some(data), None),
        None => (system_font("segoeui.ttf"), system_font("segoeuib.ttf")),
    };
    let mono = custom_font(&font.mono_font).or_else(|| system_font("consola.ttf"));
    let mut gg_face = |weight: u16| {
        let (_, data) = gg.iter().find(|(w, _)| *w == weight)?;
        let name = format!("gg-sans-{weight}");
        defs.font_data.insert(name.clone(), Arc::new(data.clone()));
        Some(name)
    };
    let gg_faces = [400, 500, 600, 700].map(&mut gg_face);

    let mut prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Some(data) = ui {
        defs.font_data.insert("rc-ui".into(), Arc::new(data));
        prop.insert(0, "rc-ui".into());
    }
    // Segoe UI stays right behind it: gg sans has ~720 characters.
    prop.splice(0..0, gg_faces[0].clone());
    // Emoji code points drawn as UI icons stay monochrome and take the icon color.
    defs.families.insert(icon_family(), prop.clone());
    if let Some(data) = emoji_font() {
        // After the text faces (digits, ©, ↔ stay text), before egui's monochrome fallbacks.
        defs.font_data.insert(OS_EMOJI.into(), Arc::new(data));
        for chain in [&mut prop, defs.families.entry(FontFamily::Monospace).or_default()] {
            let at = chain.iter().position(|n| n == "egui-icons").unwrap_or(chain.len());
            chain.insert(at, OS_EMOJI.into());
        }
    }
    let mut bold_chain = prop.clone();
    if let Some(data) = bold {
        defs.font_data.insert("rc-ui-bold".into(), Arc::new(data));
        bold_chain.insert(0, "rc-ui-bold".into());
    }
    if let Some(data) = mono {
        defs.font_data.insert("rc-mono".into(), Arc::new(data));
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "rc-mono".into());
    }
    let [_, medium, semibold, gg_bold] = gg_faces;
    let with = |face: Option<String>, chain: &[String]| face.into_iter().chain(chain.iter().cloned()).collect::<Vec<_>>();
    defs.families.insert(medium_family(), with(medium, &prop));
    defs.families.insert(semibold_family(), with(semibold, &bold_chain));
    defs.families.insert(FontFamily::Name(BOLD_FAMILY.into()), with(gg_bold, &bold_chain));
    // Box drawing without gg sans: its ━ ─ are short dashes; the provider finds Segoe UI Symbol's.
    defs.families.insert(box_family(), prop.iter().filter(|n| !n.starts_with("gg-sans-")).cloned().collect());
    defs.families.insert(FontFamily::Proportional, prop);
    defs
}

/// Medium (500) text: gg sans Medium, else the regular face.
pub fn medium_family() -> FontFamily {
    FontFamily::Name("rc-medium".into())
}

/// Box drawing (U+2500-257F) in decorated names (`append_boxed`): the text faces without gg sans,
/// whose box glyphs are shorter than their advance, so Segoe UI Symbol's corners and bars draw
/// them. (Horizontal rules like `━━━` are drawn in every family by `fonts::box_rule`.)
pub fn box_family() -> FontFamily {
    FontFamily::Name("rc-box".into())
}

fn is_box(c: char) -> bool {
    ('\u{2500}'..='\u{257f}').contains(&c)
}

/// Appends `text` to `job` in `format`, its box-drawing runs in `box_family` (same size), so
/// rules like `━━» TEAM «━━` stay continuous.
pub fn append_boxed(job: &mut egui::text::LayoutJob, text: &str, format: &egui::TextFormat) {
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let boxed = is_box(c);
        let end = rest.find(|ch: char| is_box(ch) != boxed).unwrap_or(rest.len());
        let mut f = format.clone();
        if boxed {
            f.font_id = egui::FontId::new(format.font_id.size, box_family());
        }
        job.append(&rest[..end], 0.0, f);
        rest = &rest[end..];
    }
}

/// Semibold (600) text, Discord's usual weight for headings and labels: gg sans Semibold, else
/// the bold face.
pub fn semibold_family() -> FontFamily {
    FontFamily::Name("rc-semibold".into())
}

fn custom_font(path: &str) -> Option<FontData> {
    if path.trim().is_empty() {
        return None;
    }
    let font = read_font(Path::new(path.trim()));
    if font.is_none() {
        eprintln!("rustcord: theme font {path:?} is missing or not a TrueType/OpenType font");
    }
    font.map(FontData::from_owned)
}

/// Mapped rather than read: the pages stay shared with the OS file cache instead of costing
/// ~2.4 MB of private heap (Segoe UI, its bold face, Consolas). Once per process: every font
/// rebuild shares the blob, so `set_fonts` compares pointers, not whole files, and one view
/// per file stays mapped.
#[cfg(windows)]
pub(super) fn system_font(file: &str) -> Option<FontData> {
    static MAPPED: std::sync::Mutex<Vec<(String, Option<FontData>)>> = std::sync::Mutex::new(Vec::new());
    let mut cache = MAPPED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, font)) = cache.iter().find(|(f, _)| f == file) {
        return font.clone();
    }
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let path = Path::new(&root).join("Fonts").join(file);
    let mapped = open_font(&path).and_then(|f| map_file(&f)).filter(|m| looks_like_font(m.as_ref()));
    let font = match mapped {
        Some(m) => Some(FontData::from_blob(Arc::new(m), 0)),
        None => read_font(&path).map(FontData::from_owned),
    };
    cache.push((file.to_owned(), font.clone()));
    font
}

#[cfg(not(windows))]
pub(super) fn system_font(_file: &str) -> Option<FontData> {
    None
}

/// Font data name of the OS color emoji font.
const OS_EMOJI: &str = "os-emoji";

/// Family for emoji glyphs used as UI icons (💬, 🔒, ⚙): the text faces plus egui's monochrome
/// emoji, tinted like text. Everything else draws unicode emoji with the OS color font.
pub(crate) fn icon_family() -> FontFamily {
    FontFamily::Name("rc-icons".into())
}

/// The OS color emoji font (epaint `color_fonts`): Segoe UI Emoji (COLR), mapped like the UI fonts.
/// epaint centres its shorter line box on Segoe UI's, which leaves the emoji ~0.17 em above the
/// text (and above the centre of their own galley, e.g. in reaction chips): lowered by that.
#[cfg(windows)]
fn emoji_font() -> Option<FontData> {
    system_font("seguiemj.ttf").map(|f| f.tweak(egui::FontTweak { y_offset_factor: 0.17, ..Default::default() }))
}

/// Noto Color Emoji (CBDT), read into memory (~10 MB) once per process (see `system_font`).
// shortcut: macOS' Apple Color Emoji.ttc (~190 MB) is over `open_font`'s cap, so macOS keeps
// the monochrome emoji; map it (mmap) when a macOS build matters.
#[cfg(not(windows))]
fn emoji_font() -> Option<FontData> {
    static EMOJI: std::sync::OnceLock<Option<FontData>> = std::sync::OnceLock::new();
    let paths = [
        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/google-noto-color-emoji-fonts/NotoColorEmoji.ttf",
        "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
    ];
    EMOJI.get_or_init(|| paths.iter().find_map(|p| read_font(Path::new(p))).map(FontData::from_owned)).clone()
}

/// Opens a font file: only plain local files of a sane size.
fn open_font(path: &Path) -> Option<std::fs::File> {
    // Paths come from (imported) theme files: only plain local files, before any fs call.
    // UNC paths would open network connections (leaking NTLM credentials), device paths hang.
    #[cfg(windows)]
    if !matches!(path.components().next(), Some(std::path::Component::Prefix(p))
        if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
    {
        return None;
    }
    #[cfg(not(windows))]
    if !path.is_absolute() {
        return None;
    }
    // Fonts are a few MB at most; anything bigger is not worth keeping in RAM.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 64 * 1024 * 1024 {
        return None;
    }
    std::fs::File::open(path).ok()
}

pub(super) fn read_font(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    open_font(path)?.read_to_end(&mut bytes).ok()?;
    looks_like_font(&bytes).then_some(bytes)
}

/// Read-only view of a whole file (unmapped on drop). Writers can't truncate a mapped file.
#[cfg(windows)]
struct MappedFile {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the view is read-only and lives until drop.
#[cfg(windows)]
unsafe impl Send for MappedFile {}
#[cfg(windows)]
unsafe impl Sync for MappedFile {}

#[cfg(windows)]
impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `ptr` is a live view of `len` readable bytes (see `map_file`).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(file: isize, attrs: *const u8, protect: u32, size_high: u32, size_low: u32, name: *const u16) -> isize;
    fn MapViewOfFile(mapping: isize, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *const u8;
    fn UnmapViewOfFile(base: *const u8) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[cfg(windows)]
impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from MapViewOfFile and is unmapped once.
        unsafe { UnmapViewOfFile(self.ptr) };
    }
}

#[cfg(windows)]
fn map_file(file: &std::fs::File) -> Option<MappedFile> {
    use std::os::windows::io::AsRawHandle;
    const PAGE_READONLY: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x04;
    let len = usize::try_from(file.metadata().ok()?.len()).ok().filter(|&l| l > 0)?;
    // SAFETY: plain Win32 calls on a valid file handle; the view keeps the section alive after
    // its handle is closed, and covers the whole file (`len` bytes).
    unsafe {
        let mapping = CreateFileMappingW(file.as_raw_handle() as isize, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
        if mapping == 0 {
            return None;
        }
        let ptr = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        CloseHandle(mapping);
        (!ptr.is_null()).then_some(MappedFile { ptr, len })
    }
}

/// Sanity check of the sfnt table directory. epaint panics on a font it cannot parse, so a
/// bad path in a theme file must never reach it.
pub(super) fn looks_like_font(b: &[u8]) -> bool {
    let u32_at = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_be_bytes([s[0], s[1]]));
    let mut start = 0usize;
    if b.starts_with(b"ttcf") {
        // Collection: epaint uses face 0.
        let Some(off) = u32_at(12) else { return false };
        start = off as usize;
    }
    let Some(tag) = u32_at(start) else { return false };
    if !matches!(tag, 0x0001_0000 | 0x4F54_544F /* OTTO */ | 0x7472_7565 /* true */) {
        return false;
    }
    let Some(num_tables) = u16_at(start + 4) else { return false };
    let mut has_cmap = false;
    for i in 0..usize::from(num_tables) {
        let rec = start + 12 + i * 16;
        let (Some(tag), Some(off), Some(len)) = (b.get(rec..rec + 4), u32_at(rec + 8), u32_at(rec + 12)) else {
            return false;
        };
        if (off as usize).checked_add(len as usize).is_none_or(|end| end > b.len()) {
            return false;
        }
        has_cmap |= tag == b"cmap";
    }
    has_cmap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_drawing_runs_get_their_own_family() {
        let mut job = egui::text::LayoutJob::default();
        append_boxed(&mut job, "\u{2501}\u{2501}\u{bb} TEAM \u{ab}\u{2501}", &egui::TextFormat::simple(egui::FontId::proportional(14.0), egui::Color32::WHITE));
        let runs: Vec<(&str, bool)> = job.sections.iter().map(|s| (&job.text[s.byte_range.start.0..s.byte_range.end.0], s.format.font_id.family == box_family())).collect();
        assert_eq!(runs, [("\u{2501}\u{2501}", true), ("\u{bb} TEAM \u{ab}", false), ("\u{2501}", true)]);
        assert!(job.sections.iter().all(|s| s.format.font_id.size == 14.0));
    }

    #[test]
    fn only_high_contrast_builtin_gets_strong_borders() {
        let strong: Vec<bool> = Theme::builtins().iter().map(|t| is_high_contrast(&t.colors)).collect();
        assert_eq!(strong[..3], [false, false, true]);
        assert!(!strong[3..].contains(&true));
    }

    #[test]
    fn font_sanity_check() {
        // Minimal sfnt: header + one "cmap" record pointing at 4 bytes inside the file.
        let mut f = vec![0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        f.extend_from_slice(b"cmap");
        f.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 28, 0, 0, 0, 4]);
        f.extend_from_slice(&[0, 0, 0, 0]);
        assert!(looks_like_font(&f));

        let mut out_of_bounds = f.clone();
        out_of_bounds[27] = 200; // table length past the end of the file
        assert!(!looks_like_font(&out_of_bounds));
        assert!(!looks_like_font(b"PK\x03\x04 definitely a zip"));
        assert!(!looks_like_font(&f[..20]));
        assert!(!looks_like_font(b""));
    }

    #[test]
    fn font_paths_must_be_plain_local_files() {
        for p in [r"\\127.0.0.1\x\f.ttf", r"\\?\UNC\127.0.0.1\x\f.ttf", r"\\.\pipe\x", "relative.ttf", "/dev/zero"] {
            assert!(read_font(Path::new(p)).is_none(), "{p}");
        }
        assert!(read_font(&std::env::temp_dir()).is_none()); // a folder, not a file
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_fonts_pass_the_check() {
        // Segoe UI ships with every supported Windows version.
        let mapped = system_font("segoeui.ttf").expect("Segoe UI");
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        assert_eq!(mapped.bytes(), read_font(&Path::new(&root).join("Fonts").join("segoeui.ttf")).unwrap());
        // Mapped once: a rebuild shares the blob (`set_fonts` then compares pointers).
        assert!(Arc::ptr_eq(&mapped.font, &system_font("segoeui.ttf").unwrap().font));
    }

    #[test]
    fn bold_family_always_registered() {
        let defs = build_fonts(&ThemeFont { ui_font: "Z:\\nope\\missing.ttf".into(), ..ThemeFont::default() }, &[]);
        for family in [FontFamily::Name(BOLD_FAMILY.into()), medium_family(), semibold_family()] {
            let chain = &defs.families[&family];
            assert!(!chain.is_empty());
            assert!(chain.iter().all(|name| defs.font_data.contains_key(name)));
            assert!(chain.contains(&"NotoEmoji-Regular".to_string()));
        }
    }

    #[test]
    fn gg_sans_leads_each_weight() {
        // Any face stands in for the downloaded ones (fonts.rs decodes and checks them).
        let face = FontData::from_static(b"stand-in");
        let gg = [(400, face.clone()), (600, face.clone()), (700, face)];
        let defs = build_fonts(&ThemeFont::default(), &gg);
        let first = |f: FontFamily| defs.families[&f][0].clone();
        assert_eq!(first(FontFamily::Proportional), "gg-sans-400");
        assert_eq!(first(semibold_family()), "gg-sans-600");
        assert_eq!(first(FontFamily::Name(BOLD_FAMILY.into())), "gg-sans-700");
        // No medium face: the regular chain, gg sans regular first.
        assert_eq!(defs.families[&medium_family()], defs.families[&FontFamily::Proportional]);
    }

    #[cfg(windows)]
    #[test]
    fn os_emoji_font_sits_between_text_and_monochrome_faces() {
        let defs = build_fonts(&ThemeFont::default(), &[]);
        for family in [FontFamily::Proportional, FontFamily::Monospace, FontFamily::Name(BOLD_FAMILY.into())] {
            let chain = &defs.families[&family];
            let at = |name: &str| chain.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name} in {family:?}"));
            assert!(at(OS_EMOJI) > 0 && at(OS_EMOJI) < at("NotoEmoji-Regular"), "{family:?}: {chain:?}");
        }
        let icons = &defs.families[&icon_family()];
        assert!(icons.contains(&"NotoEmoji-Regular".to_string()) && !icons.contains(&OS_EMOJI.to_string()));
    }

    #[test]
    fn mix_endpoints() {
        let (a, b) = (Rgba::rgb(0, 100, 200), Rgba::rgb(200, 0, 100));
        assert_eq!(mix(a, b, 0.0), a.to_color32());
        assert_eq!(mix(a, b, 1.0), b.to_color32());
    }
}
