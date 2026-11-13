//! Unicode emoji data for the picker and `:shortcode:` autocomplete (FEATURES_NOTES §1.6):
//! the `emojis` crate (CLDR order, gemoji names, skin tones) plus Discord's spellings where
//! they differ. Pure functions; custom (guild) emoji live in `model::CustomEmoji`.

pub use emojis::{Emoji, Group};
use emojis::SkinTone;

/// Discord name -> gemoji name where they differ (exact parity UNCONFIRMED).
// shortcut: only the common differences; extend from user reports or a JoyPixels table.
const ALIASES: &[(&str, &str)] = &[
    ("slight_smile", "slightly_smiling_face"),
    ("slight_frown", "slightly_frowning_face"),
    ("upside_down", "upside_down_face"),
    ("hugging", "hugs"),
    ("nerd", "nerd_face"),
    ("zipper_mouth", "zipper_mouth_face"),
    ("face_palm", "facepalm"),
];

/// The picker's unicode categories in Discord's order (smileys and people share "People").
pub const CATEGORIES: [(&str, &[Group]); 8] = [
    ("People", &[Group::SmileysAndEmotion, Group::PeopleAndBody]),
    ("Nature", &[Group::AnimalsAndNature]),
    ("Food", &[Group::FoodAndDrink]),
    ("Activities", &[Group::Activities]),
    ("Travel", &[Group::TravelAndPlaces]),
    ("Objects", &[Group::Objects]),
    ("Symbols", &[Group::Symbols]),
    ("Flags", &[Group::Flags]),
];

/// Emoji of one category (default skin tone), CLDR order.
pub fn category_emojis(groups: &'static [Group]) -> impl Iterator<Item = &'static Emoji> {
    groups.iter().flat_map(|g| g.emojis())
}

/// Discord spelling of a gemoji shortcode.
fn discord_name(gemoji: &'static str) -> &'static str {
    ALIASES.iter().find(|(_, g)| *g == gemoji).map_or(gemoji, |(d, _)| d)
}

/// Shortcode shown for an emoji (picker footer, autocomplete), Discord spelling.
pub fn shortcode(e: &'static Emoji) -> Option<&'static str> {
    // gemoji lists `+1`/`-1` first; Discord shows `thumbsup`/`thumbsdown`.
    e.shortcodes().find(|c| c.starts_with(|ch: char| ch.is_ascii_alphabetic())).or(e.shortcode()).map(discord_name)
}

/// Emoji for a shortcode without colons: Discord or gemoji name, optionally with
/// `::skin-tone-N` (N 1..=5) as in `thumbsup::skin-tone-2`.
#[allow(dead_code, reason = "for `:shortcode:` conversion on send, which is not built yet")]
pub fn by_shortcode(code: &str) -> Option<&'static Emoji> {
    let (name, tone) = match code.split_once("::skin-tone-") {
        Some((name, t)) => (name, t.parse::<u8>().ok().filter(|t| (1..=5).contains(t))?),
        None => (code, 0),
    };
    let name = ALIASES.iter().find(|(d, _)| *d == name).map_or(name, |(_, g)| g);
    emojis::get_by_shortcode(name).map(|e| with_skin_tone(e, tone))
}

/// `:query` autocomplete: an exact shortcode first, then emoji with a shortcode starting with
/// `query`, then those containing it, each in CLDR order; at most `limit`. Returns (Discord
/// shortcode, emoji). A full scan (~1,900 emoji) per call: microseconds.
pub fn search(query: &str, limit: usize) -> Vec<(&'static str, &'static Emoji)> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let (mut prefix, mut inner) = (Vec::new(), Vec::new());
    for e in emojis::iter() {
        // Either spelling matches; the Discord one is shown.
        let hit = |f: &dyn Fn(&str) -> bool| e.shortcodes().find(|c| f(discord_name(c)) || f(c)).map(discord_name);
        if let Some(c) = hit(&|s| s.starts_with(&q)) {
            prefix.push((c, e));
        } else if inner.len() < limit
            && let Some(c) = hit(&|s| s.contains(&q))
        {
            inner.push((c, e));
        }
    }
    // Stable: CLDR order otherwise kept.
    prefix.sort_by_key(|(c, _)| *c != q);
    prefix.extend(inner);
    prefix.truncate(limit);
    prefix
}

/// `e` with skin tone 1..=5 (light..dark) where it has tones; 0 = default.
pub fn with_skin_tone(e: &'static Emoji, tone: u8) -> &'static Emoji {
    let tone = match tone {
        1 => SkinTone::Light,
        2 => SkinTone::MediumLight,
        3 => SkinTone::Medium,
        4 => SkinTone::MediumDark,
        5 => SkinTone::Dark,
        _ => SkinTone::Default,
    };
    e.with_skin_tone(tone).unwrap_or(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcodes_with_aliases_and_tones() {
        assert_eq!(by_shortcode("slight_smile").unwrap(), "🙂");
        assert_eq!(by_shortcode("slightly_smiling_face").unwrap(), "🙂");
        assert_eq!(by_shortcode("smile").unwrap(), "😄");
        assert_eq!(by_shortcode("thumbsup::skin-tone-2").unwrap(), "👍🏼");
        assert_eq!(by_shortcode("smile::skin-tone-2").unwrap(), "😄", "no tones: unchanged");
        assert!(by_shortcode("thumbsup::skin-tone-9").is_none());
        assert!(by_shortcode("no_such_emoji").is_none());
        assert_eq!(shortcode(emojis::get("🙂").unwrap()), Some("slight_smile"));
        assert_eq!(shortcode(emojis::get("😄").unwrap()), Some("smile"));
        assert_eq!(shortcode(emojis::get("👍").unwrap()), Some("thumbsup"));
        assert_eq!(shortcode(emojis::get("💯").unwrap()), Some("100"), "digit-only names stay");
        assert_eq!(by_shortcode("+1").unwrap(), "👍");
    }

    #[test]
    fn search_prefix_before_substring() {
        let names = |q, n| search(q, n).into_iter().map(|(c, _)| c).collect::<Vec<_>>();
        assert_eq!(names("thumbs", 2), ["thumbsup", "thumbsdown"]);
        let smile = names("SMILE", 200);
        let pos = |c: &str| smile.iter().position(|x| *x == c).unwrap_or_else(|| panic!("{c} missing in {smile:?}"));
        assert_eq!(smile[..2], ["smile", "smiley"], "exact first, then CLDR order (😃 before 😄)");
        assert!(pos("smiley") < pos("slight_smile"), "prefix matches first");
        assert_eq!(names("smile", 1), ["smile"]);
        assert!(smile.iter().all(|c| c.contains("smile")));
        assert_eq!(names("slight_sm", 5), ["slight_smile"]);
        assert!(search(" ", 5).is_empty());
        assert_eq!(search("a", 3).len(), 3);
    }

    #[test]
    fn categories_and_skin_tones() {
        assert_eq!(CATEGORIES.len(), 8);
        assert_eq!(category_emojis(CATEGORIES[0].1).next().unwrap(), "😀");
        assert!(category_emojis(CATEGORIES[0].1).any(|e| e == "👍"), "people include body");
        assert_eq!(category_emojis(CATEGORIES[7].1).next().unwrap(), "🏁");
        let thumbs = emojis::get("👍").unwrap();
        assert_eq!(with_skin_tone(thumbs, 5), "👍🏿");
        assert_eq!(with_skin_tone(with_skin_tone(thumbs, 5), 0), "👍");
        assert_eq!(with_skin_tone(emojis::get("😀").unwrap(), 3), "😀");
    }
}
