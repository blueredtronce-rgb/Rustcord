//! Discord collectibles for the cosmetics editor (COSMETICS_NOTES §12): the catalog (one
//! `GET /collectibles-categories/v2` when the editor opens without a fresh disk copy; 24 h,
//! `<cache>/collectibles.json`, at most `MAX_ITEMS`), my owned items (`GET
//! /users/@me/collectibles-purchases`, once when the editor opens) and equipping
//! (`CosmeticsPatch`: `PATCH /users/@me` or `/users/@me/profile`, sent only from the editor's
//! "Apply to account" / "Remove" click, only for owned items or Nitro perks). Only what the
//! editor needs is kept: kind, SKU, name, category, the decoration / nameplate asset and the
//! effect thumbnail. Local picks (`crate::overrides`) never come here.
//!
//! OWNERSHIP: cosmetics-source-gating engineer.

use super::{ApiClient, ApiError, cdn};
use crate::model::{Id, NameStyle, ProfileCollectible};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

const CACHE_SECS: u64 = 24 * 3600;
/// Catalog items kept (the shop lists a few hundred): bounds the disk copy and memory.
const MAX_ITEMS: usize = 2000;
/// Category of owned items the catalog does not list (retired or event items).
pub const OWNED: &str = "Your collection";

/// Collectible item `type` (UD collectibles); products also use 1000 bundle, 2000 variants group,
/// 3000 external SKU.
pub mod kind {
    pub const DECORATION: u8 = 0;
    pub const EFFECT: u8 = 1;
    pub const NAMEPLATE: u8 = 2;
    pub const FRAME: u8 = 3;
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogItem {
    /// `kind::*`.
    pub kind: u8,
    pub sku_id: Id,
    pub name: String,
    pub category: String,
    /// Decoration (`a_…` animates) / nameplate (`nameplates/…/`) asset; empty for effects, frames.
    pub asset: String,
    /// Nameplate gradient palette key.
    pub palette: Option<String>,
    /// Effect preview (`thumbnailPreviewSrc`), absolute.
    pub thumb: Option<String>,
}

impl CatalogItem {
    /// Picker thumbnail for a `px` square: the effect's own preview, the decoration / nameplate
    /// asset (the URLs the renderers use), else the shop image by SKU (frames). `animated` gives
    /// the APNG where there is one. None: nothing to show (palette-only nameplate).
    pub fn thumb_url(&self, px: u32, animated: bool) -> Option<String> {
        if let Some(t) = &self.thumb {
            return Some(t.clone());
        }
        let by_asset = match self.kind {
            kind::DECORATION if !self.asset.is_empty() => cdn::decoration_url(&self.asset, px, animated),
            kind::NAMEPLATE if !self.asset.is_empty() => cdn::nameplate_url(&self.asset, animated),
            _ => None,
        };
        by_asset.or_else(|| (!self.sku_id.is_zero()).then(|| shop_url(self.sku_id, animated)))
    }
}

/// `media/v1/collectibles-shop/{sku}/static|animated` [ED].
fn shop_url(sku_id: Id, animated: bool) -> String {
    format!("{}/media/v1/collectibles-shop/{sku_id}/{}", cdn::CDN, if animated { "animated" } else { "static" })
}

/// One "Apply to account" / "Remove" click (COSMETICS_NOTES §3-§8); None / an empty list removes.
#[derive(Debug, Clone, PartialEq)]
pub enum CosmeticsPatch {
    /// `PATCH /users/@me {"avatar_decoration_sku_id"}`.
    Decoration(Option<Id>),
    /// `PATCH /users/@me {"nameplate_sku_id"}`.
    Nameplate(Option<Id>),
    /// `PATCH /users/@me/profile {"collectibles_sku_ids"}`: the full list (`profile_sku_ids`);
    /// an item left out is unequipped.
    ProfileItems(Vec<Id>),
    /// `PATCH /users/@me {"display_name_font_id","display_name_effect_id","display_name_colors"}` (Nitro).
    NameStyle(Option<NameStyle>),
    /// `PATCH /users/@me/profile {"theme_colors"}` (Nitro).
    ThemeColors(Option<[u32; 2]>),
}

impl CosmeticsPatch {
    /// (on `/users/@me/profile`, JSON body).
    pub fn body(&self) -> (bool, Value) {
        match self {
            Self::Decoration(sku) => (false, json!({ "avatar_decoration_sku_id": sku })),
            Self::Nameplate(sku) => (false, json!({ "nameplate_sku_id": sku })),
            Self::ProfileItems(skus) => (true, json!({ "collectibles_sku_ids": skus })),
            Self::NameStyle(s) => (
                false,
                json!({
                    "display_name_font_id": s.as_ref().and_then(|s| s.font_id),
                    "display_name_effect_id": s.as_ref().and_then(|s| s.effect_id),
                    "display_name_colors": s.as_ref().map(|s| &s.colors),
                }),
            ),
            Self::ThemeColors(c) => (true, json!({ "theme_colors": c })),
        }
    }
}

/// `collectibles_sku_ids` after setting my profile item of `kind` (1 effect, 3 frame) to `sku`
/// (None = unequip): every other equipped item is kept, as Discord's client does.
pub fn profile_sku_ids(current: &[ProfileCollectible], kind: u16, sku: Option<Id>) -> Vec<Id> {
    current.iter().filter(|c| c.kind != kind && !c.sku_id.is_zero()).map(|c| c.sku_id).chain(sku).collect()
}

impl ApiClient {
    /// `GET /collectibles-categories/v2?include_bundles=true&variants_return_style=1`, parsed;
    /// a non-empty result is saved for 24 h.
    pub async fn collectibles_catalog(&self) -> Result<Vec<CatalogItem>, ApiError> {
        let mut url = Self::url(&["collectibles-categories", "v2"]);
        url.query_pairs_mut().append_pair("include_bundles", "true").append_pair("variants_return_style", "1");
        let v: Value = self.send_json(Method::GET, url, None).await?;
        let items = parse_catalog(&v);
        if !items.is_empty() {
            let text = serde_json::to_vec(&Cached { fetched_at: now_secs(), items: items.clone() }).unwrap_or_default();
            if let Err(e) = crate::config::write_atomic(&cache_file(), &text) {
                eprintln!("rustcord: collectibles cache write failed: {e}");
            }
        }
        Ok(items)
    }

    /// `GET /users/@me/collectibles-purchases`: my unexpired items (category `OWNED`).
    pub async fn owned_collectibles(&self) -> Result<Vec<CatalogItem>, ApiError> {
        let v: Value = self.send_json(Method::GET, Self::url(&["users", "@me", "collectibles-purchases"]), None).await?;
        Ok(parse_owned(&v, now_secs() as i64))
    }

    /// Equip / unequip for real (USER_UPDATE carries the user fields; the profile ones show on
    /// the next profile fetch).
    pub async fn update_cosmetics(&self, patch: &CosmeticsPatch) -> Result<(), ApiError> {
        let (profile, body) = patch.body();
        let path: &[&str] = if profile { &["users", "@me", "profile"] } else { &["users", "@me"] };
        self.send(Method::PATCH, Self::url(path), Some(body)).await.map(drop)
    }
}

/// Categories (`{categories:[…]}` or a bare array) → products → items. Bundles contribute their
/// items, variant groups their variants; unknown kinds and duplicate SKUs are dropped.
pub fn parse_catalog(v: &Value) -> Vec<CatalogItem> {
    let cats = v.get("categories").unwrap_or(v).as_array();
    let mut out: Vec<CatalogItem> = Vec::new();
    for c in cats.into_iter().flatten() {
        let category = c["name"].as_str().unwrap_or_default();
        for p in c["products"].as_array().into_iter().flatten() {
            product(p, category, &mut out);
        }
    }
    dedupe(out)
}

/// The purchases list (a bare array of products), without expired ones.
pub fn parse_owned(v: &Value, now: i64) -> Vec<CatalogItem> {
    let live = |p: &&Value| p["expires_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).is_none_or(|t| t.timestamp() > now);
    let mut out = Vec::new();
    for p in v.as_array().into_iter().flatten().filter(live) {
        product(p, OWNED, &mut out);
    }
    dedupe(out)
}

fn dedupe(mut items: Vec<CatalogItem>) -> Vec<CatalogItem> {
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert((i.kind, i.sku_id)));
    items.truncate(MAX_ITEMS);
    items
}

fn product(p: &Value, category: &str, out: &mut Vec<CatalogItem>) {
    // External SKUs (3000) are not Discord collectibles.
    if p["type"].as_u64() == Some(3000) {
        return;
    }
    for v in p["variants"].as_array().into_iter().flatten() {
        product(v, category, out);
    }
    let bundle = p["type"].as_u64() == Some(1000);
    let pname = p["name"].as_str().unwrap_or_default();
    for it in p["items"].as_array().into_iter().flatten() {
        let Some(kind) = it["type"].as_u64().filter(|k| *k <= kind::FRAME as u64) else { continue };
        let Some(sku_id) = id(&it["sku_id"]).or_else(|| id(&p["sku_id"])) else { continue };
        let label = it["label"].as_str().or_else(|| it["title"].as_str()).filter(|l| !l.is_empty());
        // A single-item product's name is the shop name; a bundle's items name themselves.
        let name = if bundle { label.unwrap_or(pname) } else if pname.is_empty() { label.unwrap_or_default() } else { pname };
        out.push(CatalogItem {
            kind: kind as u8,
            sku_id,
            name: name.to_owned(),
            category: category.to_owned(),
            asset: it["asset"].as_str().unwrap_or_default().to_owned(),
            palette: it["palette"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            thumb: it["thumbnailPreviewSrc"].as_str().and_then(cdn::collectible_asset_url),
        });
    }
}

fn id(v: &Value) -> Option<Id> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(Id),
        _ => None,
    }
    .filter(|i: &Id| !i.is_zero())
}

#[derive(Serialize, Deserialize)]
struct Cached {
    fetched_at: u64,
    items: Vec<CatalogItem>,
}

fn cache_file() -> PathBuf {
    crate::config::paths().cache_dir.join("collectibles.json")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The saved catalog if it is younger than 24 h (small file; read once when the picker opens).
pub fn read_cache() -> Option<Vec<CatalogItem>> {
    let c: Cached = serde_json::from_slice(&std::fs::read(cache_file()).ok()?).ok()?;
    fresh(c.fetched_at, now_secs()).then_some(c.items).filter(|i| !i.is_empty())
}

fn fresh(fetched_at: u64, now: u64) -> bool {
    fetched_at <= now && now - fetched_at < CACHE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_products_bundles_and_variants() {
        let v = json!({"categories": [{"name": "Cats", "products": [
            {"sku_id": "11", "name": "Cat Ears", "type": 0, "items": [{"type": 0, "sku_id": "11", "asset": "a_fed4", "label": "ears"}]},
            {"sku_id": "12", "name": "Angel", "type": 2, "items": [{"type": 2, "sku_id": "12", "asset": "nameplates/x/angel/", "palette": "bubble_gum", "label": "It's angel time"}]},
            {"sku_id": "13", "name": "Sparkles", "type": 1, "items": [{"type": 1, "sku_id": "13", "title": "Sparkles", "thumbnailPreviewSrc": "assets/profile_effects/s/thumbnail.png"}]},
            {"sku_id": "20", "name": "Cat Bundle", "type": 1000, "items": [
                {"type": 0, "sku_id": "11", "asset": "a_fed4", "label": "ears"},
                {"type": 3, "sku_id": "21", "label": "Cat Frame"},
                {"type": 7, "sku_id": "22"}]},
            {"sku_id": "30", "name": "Colors", "type": 2000, "items": [], "variants": [
                {"sku_id": "31", "name": "Red", "type": 0, "items": [{"type": 0, "sku_id": 31, "asset": "red"}]}]},
            {"sku_id": "40", "type": 3000, "items": [{"type": 0}]}
        ]}]});
        let items = parse_catalog(&v);
        let summary: Vec<(u8, u64, &str, &str)> = items.iter().map(|i| (i.kind, i.sku_id.0, i.name.as_str(), i.asset.as_str())).collect();
        assert_eq!(
            summary,
            [(0, 11, "Cat Ears", "a_fed4"), (2, 12, "Angel", "nameplates/x/angel/"), (1, 13, "Sparkles", ""), (3, 21, "Cat Frame", ""), (0, 31, "Red", "red")]
        );
        assert_eq!(items[1].palette.as_deref(), Some("bubble_gum"));
        assert_eq!(items[2].thumb.as_deref(), Some("https://cdn.discordapp.com/assets/profile_effects/s/thumbnail.png"));
        assert!(items.iter().all(|i| i.category == "Cats"));
        // A bare array (v1 shape) works too.
        assert_eq!(parse_catalog(&json!([{"name": "A", "products": v["categories"][0]["products"]}])).len(), items.len());
        assert!(parse_catalog(&json!({"error": 1})).is_empty());
        // Bounded.
        let many: Vec<Value> = (1..=MAX_ITEMS as u64 + 5).map(|i| json!({"sku_id": i, "type": 0, "items": [{"type": 0, "sku_id": i}]})).collect();
        assert_eq!(parse_catalog(&json!([{"name": "Big", "products": many}])).len(), MAX_ITEMS);
    }

    #[test]
    fn owned_items_skip_expired() {
        let v = json!([
            {"sku_id": "11", "name": "Cat Ears", "type": 0, "expires_at": null, "items": [{"type": 0, "sku_id": "11", "asset": "a_fed4"}]},
            {"sku_id": "12", "name": "Old", "type": 0, "expires_at": "2001-01-01T00:00:00+00:00", "items": [{"type": 0, "sku_id": "12", "asset": "b"}]},
            {"sku_id": "13", "name": "Angel", "type": 2, "expires_at": "2100-01-01T00:00:00+00:00", "items": [{"type": 2, "sku_id": "13", "asset": "nameplates/x/angel/"}]}
        ]);
        let owned = parse_owned(&v, 1_700_000_000);
        assert_eq!(owned.iter().map(|i| i.sku_id.0).collect::<Vec<_>>(), [11, 13]);
        assert!(owned.iter().all(|i| i.category == OWNED));
        assert!(parse_owned(&json!({"message": "401"}), 0).is_empty());
    }

    #[test]
    fn equip_payloads() {
        assert_eq!(CosmeticsPatch::Decoration(Some(Id(5))).body(), (false, json!({"avatar_decoration_sku_id": "5"})));
        assert_eq!(CosmeticsPatch::Decoration(None).body(), (false, json!({"avatar_decoration_sku_id": null})));
        assert_eq!(CosmeticsPatch::Nameplate(Some(Id(6))).body(), (false, json!({"nameplate_sku_id": "6"})));
        let style = NameStyle { font_id: Some(12), effect_id: Some(2), colors: vec![0x2AAEA6, 0xFFC490] };
        assert_eq!(
            CosmeticsPatch::NameStyle(Some(style)).body(),
            (false, json!({"display_name_font_id": 12, "display_name_effect_id": 2, "display_name_colors": [0x2AAEA6, 0xFFC490]}))
        );
        assert_eq!(
            CosmeticsPatch::NameStyle(None).body(),
            (false, json!({"display_name_font_id": null, "display_name_effect_id": null, "display_name_colors": null}))
        );
        assert_eq!(CosmeticsPatch::ThemeColors(Some([1, 2])).body(), (true, json!({"theme_colors": [1, 2]})));
        assert_eq!(CosmeticsPatch::ThemeColors(None).body(), (true, json!({"theme_colors": null})));
        assert_eq!(CosmeticsPatch::ProfileItems(vec![Id(7), Id(8)]).body(), (true, json!({"collectibles_sku_ids": ["7", "8"]})));
    }

    #[test]
    fn profile_list_keeps_the_other_items() {
        let item = |sku, kind| ProfileCollectible { sku_id: Id(sku), kind, expires_at: None };
        let current = [item(1, 1), item(3, 3), item(0, 3)];
        // New effect: the frame stays.
        assert_eq!(profile_sku_ids(&current, 1, Some(Id(9))), [Id(3), Id(9)]);
        // Frame removed: the effect stays.
        assert_eq!(profile_sku_ids(&current, 3, None), [Id(1)]);
        assert_eq!(profile_sku_ids(&[], 3, Some(Id(4))), [Id(4)]);
    }

    #[test]
    fn thumbnails_and_cache_age() {
        let frame = CatalogItem { kind: kind::FRAME, sku_id: Id(5), ..Default::default() };
        assert_eq!(frame.thumb_url(96, false).as_deref(), Some("https://cdn.discordapp.com/media/v1/collectibles-shop/5/static"));
        assert_eq!(frame.thumb_url(96, true).as_deref(), Some("https://cdn.discordapp.com/media/v1/collectibles-shop/5/animated"));
        let deco = CatalogItem { kind: kind::DECORATION, asset: "a_fed4".into(), ..frame.clone() };
        assert_eq!(deco.thumb_url(80, true).as_deref(), Some("https://cdn.discordapp.com/avatar-decoration-presets/a_fed4.png?size=128&passthrough=true"));
        let plate = CatalogItem { kind: kind::NAMEPLATE, asset: "nameplates/x/angel/".into(), ..frame.clone() };
        assert_eq!(plate.thumb_url(240, false).as_deref(), Some("https://cdn.discordapp.com/assets/collectibles/nameplates/x/angel/static.png"));
        let fx = CatalogItem { thumb: Some("https://cdn.discordapp.com/t.png".into()), ..frame };
        assert_eq!(fx.thumb_url(96, true).as_deref(), Some("https://cdn.discordapp.com/t.png"));
        // Palette-only nameplate: nothing to load.
        assert_eq!(CatalogItem { kind: kind::NAMEPLATE, palette: Some("sky".into()), ..Default::default() }.thumb_url(240, false), None);
        assert!(fresh(100, 100 + CACHE_SECS - 1));
        assert!(!fresh(100, 100 + CACHE_SECS));
        // A clock that went backwards never trusts the file.
        assert!(!fresh(200, 100));
    }
}
