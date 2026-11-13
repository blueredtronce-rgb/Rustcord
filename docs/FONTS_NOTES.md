# Fonts: gg sans at runtime, and the □□ (tofu) fix

Research date 2026-11-13, on this Windows 11 machine (build 26300). Unverified claims are marked **UNCONFIRMED**.

**What was checked:**
- the live discord.com app shell and CSS;
- HEAD requests on every asset;
- a decode of gg sans with `wuff` in a scratch build (all decoded copies deleted afterwards);
- a cmap scan of the 367 faces in `C:\Windows\Fonts`;
- the epaint, eframe, egui 0.37 and skrifa 0.44 sources in `~/.cargo/registry`.

## 0. Editor's corrections

| # | Draft | Resolution |
|---|---|---|
| 1 | `rediscover()` used regexes | Not needed. Plain `str::find` / `split` is enough (code in §1.5). `regex` would be free anyway: it is already in Cargo.lock through keyring. |
| 2 | "wuff adds 2 crates" | wuff itself plus **2 transitive crates = 3 new crates**. `alloc-stdlib` is **not** pulled: wuff 0.2.10 depends on `brotli-decompressor ^6.2.0` with `default-features = false`, and its `brotli` feature does not enable `std`. `bytes` (1.12.1) is already in the lock. |
| 3 | Italic and gg mono 700 sizes missing | Filled in from HEAD requests. All 10 gg sans URLs, both gg mono URLs and the Stripe VF return 200. |
| 4 | "eframe's warning provider" | eframe adds `MissingSystemFontsWarning` **before** the app's providers. It returns `None`, so later providers are still asked. `system_font_fallback: false` only removes a one-time `log::info!`. |
| 5 | Provider code read bytes via `data.font.as_ref().as_ref()` | Use `data.bytes()` (`FontData::bytes`). `FaceStore::install` deduplicates by name, so repeat hits on `sys:{file}#{i}` cost nothing. |
| 6 | skrifa "unifies" | **Confirmed.** epaint 0.37 depends on `skrifa = 0.44.0` with features `["std","autohint_shaping"]`. A direct `skrifa = { version = "0.44", default-features = false, features = ["std"] }` compiles nothing new. `Charmap::map(impl Into<u32>) -> Option<GlyphId>` and `FontRef::from_index` exist (read-fonts 0.41). |

**New crates for the whole fonts work** (crates.io API, `.crate` = compressed size):

| crate | version | released | .crate | licence | why |
|---|---|---|---|---|---|
| **wuff** | 0.2.10 | 2026-11-05 | **27,122 B** | MIT | WOFF2 → sfnt, pure Rust, MSRV 1.85, ~1.15 M downloads |
| brotli-decompressor | 6.3.1 | 2026-11-09 | 299,310 B | BSD-3-Clause/MIT | wuff's brotli feature |
| alloc-no-stdlib | 3.0.0 | 2026-06-14 | 10,450 B | BSD-3-Clause | brotli-decompressor dependency |
| **total** | | | **336,882 B (≈329 KiB)** | | |

- Cargo line: `wuff = { version = "=0.2.10", default-features = false, features = ["brotli"] }` (needs the usual "(approved)" comment).
- Both wuff 0.2.10 and brotli-decompressor 6.3.x are under 2 weeks old. To use an older brotli: `cargo update -p brotli-decompressor --precise 6.2.0` (2026-10-25, 280,505 B; satisfies `^6.2.0`).
- skrifa needs no new crate (§0 #6). The tofu fix needs **no new crate at all**.

## 1. gg sans

### 1.1 How the web client loads it (verified)

- `https://discord.com/app` returns 200 (77 KB) with any UA. It links about 326 stylesheets as `<link href="/assets/<id>.<hash>.css" rel="stylesheet">`.
- gg sans `@font-face` rules are in the large main sheet. Today that is `/assets/952007.c7b995731f0792cf.css` (782 KB), the **second-to-last** stylesheet link. Its name changes on every deploy, so never hard-code it.
- The rules are minified with an unquoted family:
  ```css
  @font-face{font-display:swap;font-family:gg sans;font-style:normal;font-weight:400;src:url("/assets/66d715454104d24e.woff2") format("woff2")}
  ```
- woff2 only, no `unicode-range`: each file is the whole font.

**gg sans** (`https://discord.com/assets/<hash>.woff2`; all Last-Modified 2024-12-18, `max-age=2592000`, content-hashed so immutable):

| weight | style | hash | woff2 B | decoded sfnt B | need |
|---|---|---|---|---|---|
| 400 | normal | `66d715454104d24e` | 38,156 | 97,364 | **yes** |
| 400 | italic | `dd24010f3cf7def7` | 43,364 | 104,248 | **yes** (markdown `*em*`) |
| 500 | normal | `b272b33815319bae` | 39,424 | 97,376 | **yes** |
| 500 | italic | `6a1346ad3821ff3c` | 45,516 | – | no |
| 600 | normal | `2df2c3ff74408972` | 39,764 | 97,552 | **yes** |
| 600 | italic | `d5d789aeb6282532` | 45,684 | – | no |
| 700 | normal | `189422196a4f8b53` | 39,724 | 97,360 | **yes** (`BOLD_FAMILY`) |
| 700 | italic | `ce3b8055f5114434` | 45,868 | – | optional (bold italic) |
| 800 | normal | `b2fdbe507d6ce9ef` | 40,424 | 97,304 | optional |
| 800 | italic | `03dcf979852e8b8e` | 45,312 | – | no |

- The required set is 5 files, **≈200 KB download** and ≈490 KB decoded in RAM.
- **gg mono:** 400 `249d0a057895c668` (25,760 B), 700 `45efa6936fdfb918` (26,168 B). Latin only; optional for code blocks, with Consolas behind it.
- **Last resort:** `https://cdn.discordapp.com/assets/stripe/ggsans-vf.woff2` (51,864 B; Stripe Elements font).
  - Variable font, `wght` 400–800, v1.002.
  - Latin plus 4 Greek characters, **no Cyrillic**.
  - Register it once per weight with `FontTweak { coords: VariationCoords::new([("wght", 500.0)]), ..Default::default() }`, sharing one Blob.
- **Coverage of static v2.001:** 720 code points (Latin, Latin-1, Ext-A, Latin Extended Additional, Greek, Cyrillic, some arrows and math). Missing `・ ︱ ┊ ★ ♡ ✓`, CJK and Indic, so Segoe UI and the provider (§2) stay behind it.
- **Other faces in the same CSS** (not needed): Noto Sans (OFL), ABC Ginto, and the display-name-style fonts (COSMETICS_NOTES).

### 1.2 Weights and families

- Discord's CSS uses, by count: 500 about 82, 600 about 80, 700 about 69, 400 about 52, 900 10, 800 9, italic 40.
- Map 300 → 400 and 900 → 800.
- Rustcord families (`theme/apply.rs::build_fonts`):
  - **Proportional:** `[gg400, rc-ui (Segoe UI), os-emoji, egui fallbacks…]`
  - **`BOLD_FAMILY`:** `[gg700, rc-ui-bold, …prop]`
  - **New `Name("medium")`, `Name("semibold")`:** `[gg500 / gg600, rc-ui, …]`
  - **New `Name("italic")`:** `[gg400i, rc-ui, …]`
- A theme's own `ui_font` still wins over gg sans.

### 1.3 Discord's own fallback stack (verified from the CSS)

- `--font-primary: "gg sans","Noto Sans","Helvetica Neue",Helvetica,Arial,sans-serif`
- `:lang(ja|ko|zh-CN|zh-TW)` put Hiragino/Meiryo, Malgun/Gulim and YaHei/JhengHei/SimSun before Noto Sans.
- `--font-code: "gg mono","Source Code Pro",Consolas,…,monospace`
- Anything else goes to Chromium's per-character fallback. On Windows that is DirectWrite `IDWriteFontFallback::MapCharacters`. Which face it picks for kana in a `de` locale is UNCONFIRMED.

### 1.4 Licence (not legal advice)

- The `name` table says © 2023 Discord Inc. / Colophon Foundry.
- The embedded licence restricts use to Discord-related brand materials. It forbids modifying, **converting** and sharing the font, and there is no public download.

**Therefore:**
- never commit, bundle, install or export it;
- download only from discord.com on the user's machine, cached like a browser cache;
- keep the **original woff2** on disk and decode in memory at every start (≈1.3 ms per file);
- add a setting, "Use Discord font (gg sans, downloaded from discord.com)", that falls back to Segoe UI when off or offline. The user asked for it, so default it on.
- Use in a third-party client is a grey zone: owner's risk.

### 1.5 Runtime loading (implementation-ready)

```rust
// Seed manifest (weight, italic, hash); rediscover only on 404 or a failed decode.
const GG: &[(u16, bool, &str)] = &[(400, false, "66d715454104d24e"), (400, true, "dd24010f3cf7def7"),
    (500, false, "b272b33815319bae"), (600, false, "2df2c3ff74408972"), (700, false, "189422196a4f8b53")];

// cache: ProjectDirs::cache_dir()/fonts/gg-sans/<hash>.woff2   (`directories` is a dependency)
// fetch: reqwest GET https://discord.com/assets/<hash>.woff2   (in the background; the first frame uses Segoe UI)
fn decode(woff2: &[u8]) -> Option<Vec<u8>> {
    if !woff2.starts_with(b"wOF2") { return None; }
    let ttf = wuff::decompress_woff2(woff2).ok()?;
    looks_like_font(&ttf).then_some(ttf)          // existing check in theme/apply.rs (it rejects wOF2, so decode first)
}

// 404 → one rediscovery per run (no regex):
async fn rediscover(http: &reqwest::Client) -> Option<Vec<(u16, bool, String)>> {
    let html = http.get("https://discord.com/app").send().await.ok()?.text().await.ok()?;
    let sheets: Vec<&str> = html.split("<link href=\"").skip(1)
        .filter_map(|s| s.split('"').next()).filter(|h| h.starts_with("/assets/") && h.ends_with(".css")).collect();
    for css in sheets.iter().rev() {                 // the font sheet is second-to-last: ~2 requests
        let body = http.get(format!("https://discord.com{css}")).send().await.ok()?.text().await.ok()?;
        if !body.contains("font-family:gg sans;") { continue; }
        let faces = body.split("@font-face{").skip(1).filter_map(|r| {
            let r = r.split('}').next()?;
            let val = |k: &str| r.split(k).nth(1)?.split([';', '}']).next();
            (val("font-family:")? == "gg sans").then_some(())?;
            let hash = r.split("url(\"/assets/").nth(1)?.split(".woff2").next()?;
            Some((val("font-weight:")?.parse().ok()?, val("font-style:")? == "italic", hash.to_owned()))
        }).collect::<Vec<_>>();
        return (!faces.is_empty()).then_some(faces);
    }
    None
}
```

- **Rediscovery result:** write it to `manifest.json` next to the cache.
- **If rediscovery fails:** use the Stripe VF, otherwise stay on Segoe UI.
- **Validation:** also check that name ID 1 starts with "gg sans" using skrifa (`MetadataProvider::localized_strings(StringId::FAMILY_NAME)`). This is optional.
- **When the download finishes:** rebuild `FontDefinitions` and call `ctx.set_fonts`.
- **Re-check the emoji offset:** `emoji_font()` has `y_offset_factor: 0.17`, tuned to Segoe UI's line box. Check it again with gg sans as the primary face (UNCONFIRMED that it still fits).
- **Wire-up:** add a `use_discord_font: bool` to `config::Settings` and a toggle on the Appearance page.

## 2. The □□ fix: an own font provider

### 2.1 Mechanism (egui 0.37, verified)

- **`FontProvider::font_for(&FallbackRequest{cluster, family})`** is asked only after every installed font missed. It is asked **at most once per (family, character)**, even when it returns `None`.
- **Install:** `ctx.add_font_provider(Arc::new(f))`. Any `Fn(&FallbackRequest) -> Option<FontInsert> + Send + Sync` qualifies.
- **What `FontProviders::discover` does:**
  - skips control characters and combining marks;
  - installs the returned font and checks its glyph;
  - on a miss, logs `warn!` and asks the **next provider**, not the next font.
  - So the provider must check the cmap itself.
- **Order:** discovered fonts go after all configured ones. `✿ ❀ ❤ ✓ ↳` keep coming from Segoe UI Emoji (`os-emoji`), as today.
- **Why lazy rather than a long static chain:** `FontFace::new` eagerly builds shaping data, so 25 configured faces would all be parsed on every `set_fonts`.
- **eframe's `system_fonts` feature** (`egui_system_font_provider` 0.37 → fontique 0.11, DirectWrite):
  - It gives characters whose script is Common, Inherited or Unknown only the generic families: Arial, Segoe UI, Segoe UI Emoji, Cambria Math, Times New Roman, Consolas.
  - `・ ︱ ︲ ┊ ┆` are in none of them, so they stay tofu. Do not rely on it.
- **Invisible characters:** epaint hides `\r`, U+200B–200F, 202A–202E, 2060–206F and FEFF. U+00AD, U+034F, U+061C, U+180B–180F and tags U+E0000–E007F stay visible; strip U+00AD and U+180E from names if they show up (impact UNCONFIRMED).

### 2.2 Coverage map (cmap scan of stock Win11 fonts)

"Already OK" means Segoe UI or Segoe UI Emoji has it today.

| Block / typical Discord decoration | First stock font with it | Others |
|---|---|---|
| Modifiers, IPA, small caps, super/subscripts `˚ ˖ ʚ ɞ ᴀ ʙ ᵃ ₊` | already OK | – |
| Arabic, Hebrew, Greek, Cyrillic, Armenian, Georgian | already OK | tahoma, micross |
| **Box drawing** `─ ━ │ ┃ ┊ ┆ ╭ ╰ ═` | **seguisym.ttf** | YuGoth, msyh, msgothic |
| Block elements, misc technical `█ ⌗ ⎯ ⏤` | seguisym | Cambria Math (some) |
| Dingbats, misc symbols `✦ ✧ ➜ ❥ ★ ☆ ☾ ♫` | seguisym | Malgun, YuGoth, msgothic (some) |
| Math operators, arrows `⋆ ⊹ ⋮ ⟡ ⟶ ‧ ‣` | seguisym | Cambria Math |
| **Braille** (`⠀` blank filler) | **seguisym only** | – |
| Math alphanumerics `𝐀 𝓐 𝔄 𝕬 𝗔 𝟏` | seguisym | Cambria Math |
| Enclosed alphanumerics, supplemental punctuation `Ⓐ 🅐 ⸝ ⸙` | seguisym | YuGoth |
| **Katakana middle dot** `・ ･ ｡` | **YuGothM.ttc #1** | msyh, msjh, msgothic (not Malgun) |
| **CJK vertical forms** `︱ ︲` | **YuGoth** | msyh, msjh, simsun (not Malgun or seguisym) |
| Small form variants `﹒ ﹏` | msyh.ttc #1 | msjh, simsun |
| Fullwidth `Ａ ！ ｜`, CJK punctuation `「 」 【 】 《 》` | Malgun | YuGoth, msyh, msjh (`〘 〙` YuGoth only) |
| Han, kana | YuGoth / Malgun / msyh / msjh | glyph shapes differ per locale |
| **Hangul and fillers** `ㅤ` U+3164, U+FFA0, U+115F, U+1160 | **malgun.ttf only** | – |
| **Yi** `꒰ ꒱ ꒦ ꒷` | **msyi.ttf only** | – |
| **Canadian syllabics** `ᘏ ᗢ ᐟ`, Cherokee | **gadugi.ttf only** | – |
| Ethiopic, Vai `ꕤ`, Tifinagh, N'Ko | ebrima.ttf | – |
| Javanese `꧁ ꧂` | javatext.ttf only | – |
| Tibetan `༄` | himalaya.ttf only | – |
| Hieroglyphs `𓆩 𓆪`, Runic, Gothic | seguihis.ttf | – |
| Lepcha, Balinese, Batak, Cham, Medefaidrin, Linear A | SansSerifCollection.ttf only (newer Win11; not Win10) | – |
| Indic | Nirmala.ttc (newer Win11) / Nirmala.ttf (Win10) | – |
| Thai, Lao, Khmer | LeelawUI.ttf | tahoma, micross (Thai) |
| Myanmar / Mongolian / Tai Le / New Tai Lue / Phags-pa | mmrtext / monbaiti / taile / ntailu / phagspa | – |

- **Face indices:**
  - `YuGothM.ttc` 1 = Yu Gothic UI
  - `msyh.ttc` 1 = YaHei UI
  - `msjh.ttc` 1 = JhengHei UI
  - `cambria.ttc` 1 = Cambria Math
  - `Nirmala.ttc` 0
- **Sizes:** all under `open_font`'s 64 MB cap (largest: msjh 21.4 MB). Mapping costs address space only.
- **Not stock:** Meiryo, Euphemia, Khmer UI and Lao UI are Features-on-Demand.

**Screenshot reading:**
- The channels show `<emoji>□□name`; the role reads `□□□□□□» TEAM «□□□□□□`.
- This fits box drawing (`━ ┃ ┊`), `・` and `︱` separators, all covered above by seguisym and Yu Gothic.
- The exact code points cannot be read from a picture. The debug `eprintln!` below logs whatever is still missing.

### 2.3 Code (fits `src/theme/apply.rs`)

```rust
/// Stock Windows fonts, in cmap-scan order, for characters no configured font has
/// (decorated channel, role and user names).
#[cfg(windows)]
const FALLBACK: &[(&str, u32)] = &[
    ("seguisym.ttf", 0), ("YuGothM.ttc", 1), ("malgun.ttf", 0), ("msyh.ttc", 1), ("msjh.ttc", 1),
    ("Nirmala.ttc", 0), ("Nirmala.ttf", 0), ("LeelawUI.ttf", 0), ("ebrima.ttf", 0), ("gadugi.ttf", 0),
    ("seguihis.ttf", 0), ("msyi.ttf", 0), ("javatext.ttf", 0), ("himalaya.ttf", 0), ("mmrtext.ttf", 0),
    ("monbaiti.ttf", 0), ("taile.ttf", 0), ("ntailu.ttf", 0), ("phagspa.ttf", 0), ("sylfaen.ttf", 0),
    ("simsunb.ttf", 0), ("SimsunExtG.ttf", 0), ("SansSerifCollection.ttf", 0), ("cambria.ttc", 1),
    ("l_10646.ttf", 0), ("micross.ttf", 0), ("tahoma.ttf", 0),
];

#[cfg(windows)]
pub(crate) fn system_fallback(req: &egui::FallbackRequest<'_>) -> Option<egui::FontInsert> {
    use skrifa::MetadataProvider;
    let c = req.cluster.chars().next()?;
    let hit = FALLBACK.iter().find_map(|&(file, index)| {
        let mut data = system_font(file)?;            // existing mmap cache; FontData clones share the Blob
        skrifa::FontRef::from_index(data.bytes(), index).ok()?.charmap().map(c).filter(|g| g.to_u32() != 0)?;
        data.index = index;                            // system_font always returns face 0
        Some(egui::FontInsert::new(&format!("sys:{file}#{index}"), data, vec![]))
    });
    #[cfg(debug_assertions)]
    if hit.is_none() {
        eprintln!("rustcord: no system font for U+{:04X}", c as u32); // lists what is still tofu
    }
    hit
}
// setup (app creator): cc.egui_ctx.add_font_provider(Arc::new(theme::system_fallback));
// NativeOptions { system_font_fallback: false, .. }   // only silences eframe's one-time info log
```

- **Cargo:** `skrifa = { version = "0.44", default-features = false, features = ["std"] }`. Already compiled by epaint; needs the "(approved)" comment.
- **Weights:** discovered fonts are regular weight in every family, including bold. Bold faces exist (`YuGothB.ttc#1`, `msyhbd.ttc#1`, `malgunbd.ttf`); add them only if bold decorated names look wrong.
- **Han glyph shapes:** Yu Gothic before YaHei means Chinese Han text draws with Japanese shapes. Reordering by `sys-locale` is optional; Discord itself only switches by `:lang()`.
- **Logging:** Rustcord has no `log` dependency, so the miss is reported with `eprintln!` in debug builds only (the provider runs once per character).

## 3. Not checked / risk

- Not checked: Windows 10 coverage (the scan ran on Win11 only); vertical alignment of CJK fallback glyphs in epaint; which face DirectWrite picks for `・` in a `de` locale.
- Risk: the gg sans licence forbids converting and sharing, so download it at runtime, decode it in memory only and keep it behind a setting. Discord can change asset hashes on any deploy; that is what the rediscovery path is for.

## Sources

- **Live Discord (2026-11-13):**
  - `https://discord.com/app`
  - `https://discord.com/assets/952007.c7b995731f0792cf.css`
  - HEAD on all `/assets/<hash>.woff2` above
  - `https://cdn.discordapp.com/assets/stripe/ggsans-vf.woff2`
- **Discord's gg sans article:** https://support.discord.com/hc/en-us/articles/9507780972951
- **crates.io API:**
  - `https://crates.io/api/v1/crates/{wuff,brotli-decompressor,alloc-no-stdlib,alloc-stdlib}`
  - `/wuff/0.2.10/dependencies`
  - `/brotli-decompressor/6.3.1/dependencies`
- **wuff:** https://github.com/nicoburns/wuff
- **Local egui sources** (`~/.cargo/registry/src/index.crates.io-*/`):
  - `epaint-0.37.0/src/text/{font_provider.rs,face_store.rs,font_data.rs,unicode.rs}`
  - `egui-0.37.0/src/context.rs` (`add_font_provider`)
  - `eframe-0.37.0/src/native/epi_integration.rs`, `epi.rs` (`system_font_fallback`)
  - `skrifa-0.44.0/src/charmap.rs`
  - `epaint-0.37.0/Cargo.toml` (skrifa features)
- **egui_system_font_provider / fontique:**
  - https://docs.rs/crate/egui_system_font_provider/0.37.0/source/src/lib.rs
  - https://docs.rs/crate/fontique/0.11.1/source/src/backend/dwrite.rs
- **Windows 11 font list:** https://learn.microsoft.com/en-us/typography/fonts/windows_11_font_list
- **Chromium fallback:**
  - https://skia.googlesource.com/skia/+/chrome/m143/src/ports/SkFontMgr_win_dw.cpp
  - https://learn.microsoft.com/en-us/windows/win32/api/dwrite_2/nf-dwrite_2-idwritefontfallback-mapcharacters
- **Scratch tools** (researcher):
  - `<local scratch, not in repo>` (cmap scanner)
  - `<local scratch, not in repo>` (wuff + skrifa inspector)
  - `<local scratch, not in repo>` (per-character results)
