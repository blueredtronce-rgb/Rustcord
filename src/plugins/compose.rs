//! Composer plugins: TextReplace (PLUGINS_NOTES S30, plain-text rules only) and CharacterCounter
//! (S31). Both only change what this app shows or sends for the user's own Enter.

use super::{Plugin, enabled, opt_raw};

/// One TextReplace rule; plugins.toml: `[TextReplace] rules = [{ find, replace, case }]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rule {
    pub find: String,
    pub replace: String,
    /// Case-sensitive match.
    pub case: bool,
}

/// The saved rules (empty or broken entries are kept as they are; empty `find`s do nothing).
pub fn rules() -> Vec<Rule> {
    let Some(toml::Value::Array(list)) = opt_raw(Plugin::TextReplace, "rules") else { return Vec::new() };
    let s = |t: &toml::Table, k: &str| t.get(k).and_then(toml::Value::as_str).unwrap_or_default().to_owned();
    list.iter()
        .filter_map(toml::Value::as_table)
        .map(|t| Rule { find: s(t, "find"), replace: s(t, "replace"), case: t.get("case").and_then(toml::Value::as_bool).unwrap_or(false) })
        .collect()
}

/// `rules` as stored in plugins.toml.
pub fn rules_value(rules: &[Rule]) -> toml::Value {
    let row = |r: &Rule| {
        let mut t = toml::Table::new();
        t.insert("find".into(), r.find.clone().into());
        t.insert("replace".into(), r.replace.clone().into());
        t.insert("case".into(), r.case.into());
        toml::Value::Table(t)
    };
    toml::Value::Array(rules.iter().map(row).collect())
}

/// The text of a message the user sends with Enter, after the TextReplace rules (once, in order).
pub fn text_replace(text: String) -> String {
    if !enabled(Plugin::TextReplace) {
        return text;
    }
    apply(&rules(), text)
}

fn apply(rules: &[Rule], mut text: String) -> String {
    for r in rules.iter().filter(|r| !r.find.is_empty()) {
        text = replace(&text, &r.find, &r.replace.replace("\\n", "\n"), r.case);
    }
    text
}

/// Every non-overlapping `find` in `text` replaced by `with`; the result is not searched again.
fn replace(text: &str, find: &str, with: &str, case: bool) -> String {
    if case {
        return text.replace(find, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        match prefix_len_ci(rest, find) {
            Some(n) => {
                out.push_str(with);
                rest = &rest[n..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// Byte length of the start of `s` that equals `p` ignoring case (char by char).
fn prefix_len_ci(s: &str, p: &str) -> Option<usize> {
    let mut it = s.char_indices();
    for pc in p.chars() {
        let (_, sc) = it.next()?;
        if !sc.to_lowercase().eq(pc.to_lowercase()) {
            return None;
        }
    }
    Some(it.next().map_or(s.len(), |(i, _)| i))
}

/// Discord's message length limit: 4000 with full Nitro (premium_type 2), else 2000.
pub fn max_len(premium_type: u8) -> usize {
    if premium_type == 2 { 4000 } else { 2000 }
}

/// CharacterCounter is on: the composer counts every non-empty draft.
pub fn counting() -> bool {
    enabled(Plugin::CharacterCounter)
}

/// The composer's counter chip ("n/limit", over the limit) for a draft of `len` characters.
pub fn counter(len: usize, max: usize) -> Option<(String, bool)> {
    (len > 0).then(|| (format!("{len}/{max}"), len > max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(find: &str, replace: &str, case: bool) -> Rule {
        Rule { find: find.into(), replace: replace.into(), case }
    }

    #[test]
    fn text_replace_rules() {
        let rules = [rule("teh", "the", false), rule("RUST", "Rust", true), rule("", "x", false), rule("br", "line\\nbreak", false)];
        assert_eq!(apply(&rules, "Teh RUST and teh rust br".into()), "the Rust and the rust line\nbreak");
        // Applied once: a replacement containing its own `find` does not grow.
        assert_eq!(apply(&[rule("lol", "lol 😂", false)], "LOL".into()), "lol 😂");
        // Case-insensitive across non-ASCII, the text around kept byte for byte.
        assert_eq!(replace("Ärger ÄRGER ärger", "ärger", "Ä", false), "Ä Ä Ä");
        assert_eq!(replace("ab", "abc", "x", false), "ab");
        // Off by default: nothing changes.
        assert_eq!(text_replace("teh".into()), "teh");
        // Round trip through plugins.toml.
        let back: Vec<Rule> = match rules_value(&rules) {
            toml::Value::Array(a) => a
                .iter()
                .filter_map(toml::Value::as_table)
                .map(|t| rule(t["find"].as_str().unwrap(), t["replace"].as_str().unwrap(), t["case"].as_bool().unwrap()))
                .collect(),
            _ => panic!(),
        };
        assert_eq!(back, rules);
    }

    #[test]
    fn counter_limits() {
        assert_eq!((max_len(0), max_len(1), max_len(2), max_len(3)), (2000, 2000, 4000, 2000));
        assert_eq!(counter(0, 2000), None);
        assert_eq!(counter(12, 2000), Some(("12/2000".into(), false)));
        assert_eq!(counter(2000, 2000), Some(("2000/2000".into(), false)));
        assert_eq!(counter(2001, 2000), Some(("2001/2000".into(), true)));
        assert_eq!(counter(2001, 4000), Some(("2001/4000".into(), false)));
        assert!(!counting());
    }
}
