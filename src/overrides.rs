//! Local-only profile overrides: my banner and (animated) avatar, plus my cosmetics (avatar
//! decoration, nameplate, profile effect / frame from Discord's catalog, display-name style,
//! profile theme; COSMETICS_NOTES §0.4). Local cosmetics apply only without Nitro (with Nitro
//! the editor equips for real) and only name Discord SKUs: SKU-less picks of older builds are
//! ignored, the file stays as it is.
//!
//! The chosen images are copied into `<config>/overrides/` (content-hashed file names, so a
//! new pick never collides with a cached texture) and described by `overrides.toml`.
//! Nothing here is ever uploaded or sent as if I had Nitro: the images are loaded through the
//! normal image pipeline via `file://` URLs, which `cache::images` only accepts for files inside
//! the overrides folder; cosmetics only name Discord CDN assets / SKUs and apply only when the
//! UI draws me (`my_cosmetics`).

use crate::config::{AnimateMode, paths, read_toml, write_toml};
use crate::model::{DecorationData, Id, Member, NameStyle, Nameplate, User};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Upper bound for a picked file; larger GIFs would only be decoded down to a few frames anyway.
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalOverrides {
    /// File name inside the overrides folder.
    pub banner: Option<String>,
    /// File name inside the overrides folder (GIF / APNG / animated WebP / any still image).
    pub avatar: Option<String>,
    /// Animate my local avatar always, or only while hovered (saves CPU).
    pub avatar_animate: AnimateMode,
    pub cosmetics: LocalCosmetics,
}

/// How I see my own cosmetics in this client. `None` = my real (server) value; a set item that
/// `is_none()` hides the real one ("None" in the picker).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalCosmetics {
    /// Profile theme `[primary, accent]`, 0xRRGGBB.
    pub theme_colors: Option<[u32; 2]>,
    pub decoration: Option<LocalItem>,
    pub nameplate: Option<LocalItem>,
    /// Profile effect, by SKU (config from `GET /collectibles-products/{sku}`).
    pub effect: Option<LocalItem>,
    /// Profile frame ("Rahmen", collectible type 3), by SKU.
    pub frame: Option<LocalItem>,
    pub name_style: Option<LocalNameStyle>,
}

/// One collectible picked from Discord's catalog (`sku_id` set; all empty = "None"):
/// decorations and nameplates draw from `asset` (+ `palette`), effects and frames from `sku_id`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalItem {
    pub sku_id: Option<Id>,
    /// Decoration: `avatar-decoration-presets/{asset}` (`a_…` animates); nameplate:
    /// `assets/collectibles/{asset}` (ends with `/`). Empty for effects / frames.
    pub asset: String,
    /// Nameplate gradient key (`crimson`, `berry`, … COSMETICS_NOTES §4).
    pub palette: Option<String>,
    /// Shop name, for the picker.
    pub label: String,
}

impl LocalItem {
    /// "Show none": hides my real item.
    pub fn is_none(&self) -> bool {
        self.sku_id.is_none() && self.asset.is_empty() && self.palette.is_none()
    }
}

/// Display-name style (COSMETICS_NOTES §7): font id (11 = default), effect id (1 solid,
/// 2 gradient, 3 neon, 4 toon, 5 pop, 6 glow, 7 prism, 8 gummy), 0xRRGGBB colours (1–5;
/// none = plain name).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalNameStyle {
    pub font_id: u32,
    pub effect_id: u32,
    pub colors: Vec<u32>,
}

impl Default for LocalNameStyle {
    fn default() -> Self {
        Self { font_id: 11, effect_id: 1, colors: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Banner,
    Avatar,
}

impl LocalOverrides {
    fn file() -> PathBuf {
        paths().overrides_dir().join("overrides.toml")
    }

    pub fn load() -> Self {
        read_toml(&Self::file()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        write_toml(&Self::file(), self)
    }

    pub fn url(&self, kind: Kind) -> Option<String> {
        let name = match kind {
            Kind::Banner => self.banner.as_deref(),
            Kind::Avatar => self.avatar.as_deref(),
        }?;
        file_url(&paths().overrides_dir().join(name))
    }

    /// Set (`Some`) or clear (`None`) the file for `kind`. Returns the previous file name when it
    /// is no longer referenced, for the caller to drop its textures (re-picking the same image
    /// yields the same content-hashed name, which must survive).
    pub fn replace(&mut self, kind: Kind, name: Option<String>) -> Option<String> {
        let slot = match kind {
            Kind::Banner => &mut self.banner,
            Kind::Avatar => &mut self.avatar,
        };
        let old = std::mem::replace(slot, name);
        old.filter(|o| slot.as_ref() != Some(o))
    }
}

/// Cache key for showing a local image large (profile card, settings preview). The image cache
/// decodes a URL once at the first requested size (small avatars load first); the fragment gives
/// a separate texture while still naming the same file.
pub fn large(url: &str) -> String {
    format!("{url}#large")
}

/// Validate `src` as an image (size limit + it must decode), copy it into the overrides folder
/// and return the new file name. Blocking (disk + decode): call off the UI thread or accept a
/// short pause on an explicit user action.
pub fn import_image(src: &Path, kind: Kind) -> Result<String, String> {
    let len = std::fs::metadata(src).map_err(|e| format!("cannot read file: {e}"))?.len();
    if len > MAX_FILE_BYTES {
        return Err(format!("file is larger than {} MB", MAX_FILE_BYTES / (1024 * 1024)));
    }
    let bytes = std::fs::read(src).map_err(|e| format!("cannot read file: {e}"))?;
    crate::cache::images::decode(&bytes, 64, false).map_err(|e| format!("not a supported image: {e}"))?;
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("img").to_ascii_lowercase();
    let prefix = match kind {
        Kind::Banner => "banner",
        Kind::Avatar => "avatar",
    };
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    let name = format!("{prefix}-{hash:016x}.{ext}");
    crate::config::write_atomic(&paths().overrides_dir().join(&name), &bytes)?;
    Ok(name)
}

/// Delete imported images that `o` no longer references (call after a successful save), so
/// failed saves or an unreadable overrides.toml never leave files behind for good.
pub fn prune(o: &LocalOverrides) {
    prune_in(&paths().overrides_dir(), o);
}

fn prune_in(dir: &Path, o: &LocalOverrides) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if (n.starts_with("avatar-") || n.starts_with("banner-")) && o.avatar.as_ref() != Some(&n) && o.banner.as_ref() != Some(&n) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `file://` URL for a path (what the image pipeline uses as cache key).
pub fn file_url(path: &Path) -> Option<String> {
    reqwest::Url::from_file_path(path).ok().map(String::from)
}

/// Path of a `file://` URL if (and only if) it points into the overrides folder.
pub fn local_path(url: &str) -> Option<PathBuf> {
    let path = reqwest::Url::parse(url).ok().filter(|u| u.scheme() == "file")?.to_file_path().ok()?;
    let dir = paths().overrides_dir();
    let inside = path.parent() == Some(dir.as_path()) && !path.components().any(|c| c == std::path::Component::ParentDir);
    inside.then_some(path)
}

// ----- global view for widgets that only see a `User` (avatar URL helper) ---------------

struct Global {
    me: Option<Id>,
    avatar_url: Option<String>,
    animate: AnimateMode,
    /// None while nothing is set (the common case: no clone per drawn row).
    cosmetics: Option<LocalCosmetics>,
}

static GLOBAL: RwLock<Global> = RwLock::new(Global { me: None, avatar_url: None, animate: AnimateMode::Hover, cosmetics: None });

/// Publish the active overrides for me (call after load/changes and when `me` or its Nitro
/// changes).
pub fn publish(me: Option<&User>, o: &LocalOverrides) {
    let mut g = GLOBAL.write().unwrap_or_else(|e| e.into_inner());
    g.me = me.map(|u| u.id);
    g.avatar_url = o.url(Kind::Avatar);
    g.animate = o.avatar_animate;
    g.cosmetics = active_cosmetics(me.is_some_and(full_nitro), &o.cosmetics);
}

/// Full Nitro (premium_type 2): profile themes and name styles are its perks, not Basic's or
/// Classic's. Without it local cosmetics render.
pub fn full_nitro(u: &User) -> bool {
    u.premium_type == 2
}

/// The local cosmetics that render: none with Nitro, and only picks of a Discord SKU (or
/// "None"); None when nothing is left.
fn active_cosmetics(nitro: bool, c: &LocalCosmetics) -> Option<LocalCosmetics> {
    if nitro {
        return None;
    }
    let mut c = c.clone();
    for slot in [&mut c.decoration, &mut c.nameplate, &mut c.effect, &mut c.frame] {
        if slot.as_ref().is_some_and(|i| !i.is_none() && i.sku_id.is_none()) {
            *slot = None;
        }
    }
    (c != LocalCosmetics::default()).then_some(c)
}

/// My local cosmetics when `user_id` is me and any is set. Renderers use each set field instead
/// of the real one (precedence: local → guild member → user) and fall through on `None`.
pub fn my_cosmetics(user_id: Id) -> Option<LocalCosmetics> {
    let g = GLOBAL.read().unwrap_or_else(|e| e.into_inner());
    g.cosmetics.as_ref().filter(|_| g.me == Some(user_id)).cloned()
}

/// The per-user cosmetics a renderer draws (profile-only ones, theme colours / effect / frame,
/// come from `my_cosmetics` over the loaded profile).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effective {
    pub decoration: Option<DecorationData>,
    pub nameplate: Option<Nameplate>,
    pub name_style: Option<NameStyle>,
}

/// Cosmetics of `user` (shown in `member`'s guild when given): my local picks, else the guild
/// member's, else the user's own; expired items are dropped (COSMETICS_NOTES §0.2, §0.4).
pub fn effective_for(user: &User, member: Option<&Member>) -> Effective {
    effective_at(my_cosmetics(user.id), user, member, chrono::Utc::now().timestamp())
}

/// `user`'s cosmetics as Discord has them, what others see: never my local picks.
pub fn real_for(user: &User) -> Effective {
    effective_at(None, user, None, chrono::Utc::now().timestamp())
}

fn effective_at(local: Option<LocalCosmetics>, user: &User, member: Option<&Member>, now: i64) -> Effective {
    let live = |exp: Option<i64>| exp.is_none_or(|e| e > now);
    let local = local.unwrap_or_default();
    let decoration = match local.decoration {
        Some(i) => (!i.is_none()).then_some(DecorationData { asset: i.asset, sku_id: i.sku_id, expires_at: None }),
        None => member
            .and_then(|m| m.avatar_decoration_data.as_deref())
            .filter(|d| live(d.expires_at))
            .or(user.avatar_decoration_data.as_deref().filter(|d| live(d.expires_at)))
            .cloned(),
    };
    fn real_plate(c: Option<&crate::model::Collectibles>, now: i64) -> Option<&Nameplate> {
        c?.nameplate.as_ref().filter(|n| n.expires_at.is_none_or(|e| e > now))
    }
    let nameplate = match local.nameplate {
        Some(i) => (!i.is_none()).then_some(Nameplate { asset: i.asset, sku_id: i.sku_id, palette: i.palette, label: Some(i.label), expires_at: None }),
        None => real_plate(member.and_then(|m| m.collectibles.as_deref()), now).or(real_plate(user.collectibles.as_deref(), now)).cloned(),
    };
    let name_style = match local.name_style {
        // No colours = plain name (also hides a real style).
        Some(s) => (!s.colors.is_empty()).then_some(NameStyle { font_id: Some(s.font_id), effect_id: Some(s.effect_id), colors: s.colors }),
        None => member.and_then(|m| m.display_name_styles.as_deref()).or(user.display_name_styles.as_deref()).cloned(),
    };
    Effective { decoration, nameplate, name_style }
}

/// My local avatar URL when `user_id` is me and an override is set.
pub fn my_avatar_url(user_id: Id) -> Option<String> {
    let g = GLOBAL.read().unwrap_or_else(|e| e.into_inner());
    (g.me == Some(user_id)).then(|| g.avatar_url.clone()).flatten()
}

/// Whether my overridden avatar should animate right now (None = not me / no override).
pub fn my_avatar_animates(user_id: Id, hovered: bool) -> Option<bool> {
    let g = GLOBAL.read().unwrap_or_else(|e| e.into_inner());
    (g.me == Some(user_id) && g.avatar_url.is_some()).then_some(g.animate == AnimateMode::Always || hovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_only_inside_overrides_dir() {
        let dir = paths().overrides_dir();
        let ok = file_url(&dir.join("avatar-1.gif")).unwrap();
        assert_eq!(local_path(&ok), Some(dir.join("avatar-1.gif")));
        let outside = file_url(&paths().config_dir.join("settings.toml")).unwrap();
        assert_eq!(local_path(&outside), None);
        assert_eq!(local_path("https://cdn.discordapp.com/x.png"), None);
        assert_eq!(local_path(&large(&ok)), Some(dir.join("avatar-1.gif")));
    }

    #[test]
    fn replace_returns_only_unreferenced_files() {
        let mut o = LocalOverrides::default();
        assert_eq!(o.replace(Kind::Avatar, Some("avatar-1.gif".into())), None);
        // Same image picked again: same name, nothing to delete.
        assert_eq!(o.replace(Kind::Avatar, Some("avatar-1.gif".into())), None);
        assert_eq!(o.replace(Kind::Avatar, Some("avatar-2.gif".into())), Some("avatar-1.gif".into()));
        assert_eq!(o.replace(Kind::Banner, Some("banner-1.png".into())), None);
        assert_eq!(o.replace(Kind::Avatar, None), Some("avatar-2.gif".into()));
        assert_eq!((o.avatar, o.banner.as_deref()), (None, Some("banner-1.png")));
    }

    #[test]
    fn prune_keeps_referenced_and_foreign_files() {
        let dir = std::env::temp_dir().join(format!("rustcord-prune-overrides-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = ["avatar-1.gif", "avatar-2.gif", "banner-1.png", "banner-2.tmp", "overrides.toml", "notes.txt"];
        for n in names {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let o = LocalOverrides { avatar: Some("avatar-2.gif".into()), banner: Some("banner-1.png".into()), ..Default::default() };
        prune_in(&dir, &o);
        let left: Vec<bool> = names.iter().map(|n| dir.join(n).exists()).collect();
        assert_eq!(left, [false, true, true, false, true, true]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_non_images_and_oversize_files() {
        // Both fail before anything is written to the overrides folder.
        let dir = std::env::temp_dir().join(format!("rustcord-import-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("notes.png");
        std::fs::write(&text, b"definitely not a png").unwrap();
        assert!(import_image(&text, Kind::Avatar).unwrap_err().starts_with("not a supported image"));
        let big = dir.join("big.gif");
        std::fs::File::create(&big).unwrap().set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(import_image(&big, Kind::Banner).unwrap_err().contains("larger than"));
        assert!(import_image(&dir.join("missing.gif"), Kind::Avatar).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // One test for everything that touches GLOBAL (tests run in parallel).
    #[test]
    fn overrides_apply_only_to_me() {
        let me = User { id: Id(7), ..Default::default() };
        let o = LocalOverrides { avatar: Some("avatar-1.gif".into()), ..Default::default() };
        publish(Some(&me), &o);
        assert!(my_avatar_url(Id(7)).is_some_and(|u| u.starts_with("file://")));
        assert_eq!(my_avatar_url(Id(8)), None);
        assert_eq!(my_avatar_animates(Id(7), false), Some(false));
        assert_eq!(my_avatar_animates(Id(7), true), Some(true));
        assert_eq!(my_cosmetics(Id(7)), None);
        let mut o = LocalOverrides::default();
        o.cosmetics.theme_colors = Some([1, 2]);
        publish(Some(&me), &o);
        assert_eq!(my_avatar_url(Id(7)), None);
        assert_eq!(my_cosmetics(Id(7)).and_then(|c| c.theme_colors), Some([1, 2]));
        assert_eq!(my_cosmetics(Id(8)), None);
        // With Nitro my real cosmetics show (the editor equips for real); the file keeps them.
        let nitro = User { premium_type: 2, ..me.clone() };
        publish(Some(&nitro), &o);
        assert_eq!(my_cosmetics(Id(7)), None);
        assert_eq!(o.cosmetics.theme_colors, Some([1, 2]));
        // Nitro Basic has no profile themes: the local one shows.
        publish(Some(&User { premium_type: 3, ..me.clone() }), &o);
        assert!(my_cosmetics(Id(7)).is_some());
        publish(None, &o);
        assert_eq!(my_cosmetics(Id(7)), None);
    }

    #[test]
    fn only_discord_items_apply_locally() {
        let item = |sku: Option<u64>, palette: Option<&str>| LocalItem { sku_id: sku.map(Id), palette: palette.map(Into::into), ..Default::default() };
        let c = LocalCosmetics {
            decoration: Some(item(Some(5), None)),
            // A palette-only nameplate of an older build: client-made, dropped.
            nameplate: Some(item(None, Some("sky"))),
            // "None" still hides my real effect.
            effect: Some(LocalItem::default()),
            ..Default::default()
        };
        let active = active_cosmetics(false, &c).unwrap();
        assert_eq!((active.decoration, active.nameplate, active.effect), (c.decoration.clone(), None, Some(LocalItem::default())));
        assert_eq!(active_cosmetics(true, &c), None, "Nitro: no local cosmetics");
        assert_eq!(active_cosmetics(false, &LocalCosmetics { nameplate: c.nameplate, ..Default::default() }), None);
    }

    #[test]
    fn effective_prefers_local_then_member_then_user() {
        let deco = |a: &str, exp| Some(Box::new(DecorationData { asset: a.into(), sku_id: None, expires_at: exp }));
        let user = User { id: Id(7), avatar_decoration_data: deco("user", None), ..Default::default() };
        let member = Member { avatar_decoration_data: deco("member", Some(50)), ..Default::default() };
        let asset = |e: Effective| e.decoration.map(|d| d.asset);
        assert_eq!(asset(effective_at(None, &user, Some(&member), 10)).as_deref(), Some("member"));
        // The member's expired: the user's shows.
        assert_eq!(asset(effective_at(None, &user, Some(&member), 60)).as_deref(), Some("user"));
        let mut local = LocalCosmetics { decoration: Some(LocalItem { asset: "mine".into(), ..Default::default() }), ..Default::default() };
        assert_eq!(asset(effective_at(Some(local.clone()), &user, Some(&member), 10)).as_deref(), Some("mine"));
        // "None" picked: hides the real one.
        local.decoration = Some(LocalItem::default());
        assert_eq!(effective_at(Some(local.clone()), &user, None, 10), Effective::default());
        local.nameplate = Some(LocalItem { palette: Some("sky".into()), label: "Sky".into(), ..Default::default() });
        local.name_style = Some(LocalNameStyle { colors: vec![0xFF0000], ..Default::default() });
        let e = effective_at(Some(local), &user, None, 10);
        assert_eq!(e.nameplate.and_then(|n| n.palette).as_deref(), Some("sky"));
        assert_eq!(e.name_style, Some(NameStyle { font_id: Some(11), effect_id: Some(1), colors: vec![0xFF0000] }));
    }

    #[test]
    fn cosmetics_round_trip_through_toml() {
        let mut o = LocalOverrides { avatar: Some("avatar-1.gif".into()), ..Default::default() };
        let c = &mut o.cosmetics;
        c.decoration = Some(LocalItem { sku_id: Some(Id(1144058844004233369)), asset: "a_fed4".into(), label: "Cat".into(), ..Default::default() });
        c.nameplate = Some(LocalItem { palette: Some("berry".into()), ..Default::default() });
        c.effect = Some(LocalItem::default());
        c.name_style = Some(LocalNameStyle { font_id: 12, effect_id: 2, colors: vec![0x2AAEA6, 0xFFC490] });
        c.theme_colors = Some([0x101010, 0xFFFFFF]);
        let back: LocalOverrides = toml::from_str(&toml::to_string_pretty(&o).unwrap()).unwrap();
        assert_eq!(back, o);
        assert!(back.cosmetics.effect.is_some_and(|e| e.is_none()));
        assert!(!back.cosmetics.nameplate.unwrap().is_none());
        // Files written before cosmetics existed still load.
        let old: LocalOverrides = toml::from_str("avatar = \"a.gif\"\navatar_animate = \"always\"\n").unwrap();
        assert_eq!((old.avatar.as_deref(), old.cosmetics), (Some("a.gif"), LocalCosmetics::default()));
    }
}
