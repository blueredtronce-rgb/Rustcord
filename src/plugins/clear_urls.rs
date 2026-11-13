//! ClearURLs (PLUGINS_NOTES S29): strips tracking parameters from links in messages I send or
//! edit and from every link I open. A short hand-picked rule list instead of the ClearURLs
//! data file (licence unconfirmed, and it needs regex): global tracking params plus a few
//! per-site ones. shortcut: no path rules (amazon `/ref=`), add rules here when one is missed.

use std::borrow::Cow;

/// Removed on every host. A trailing `*` matches a prefix.
const GLOBAL: &[&str] = &[
    "utm_*", "fbclid", "gclid", "gclsrc", "dclid", "gbraid", "wbraid", "msclkid", "mc_cid", "mc_eid", "yclid", "_hsenc", "_hsmi", "__hssc", "__hstc",
    "__hsfp", "hsCtaTracking", "mkt_tok", "igshid", "igsh", "ttclid", "twclid", "rb_clickid", "s_cid", "oly_anon_id", "oly_enc_id", "vero_id", "vero_conv",
    "wickedid", "_openstat", "ga_*", "pk_*", "piwik_*", "matomo_*", "spm", "scm",
];

/// (host suffix, params) removed only there.
const SITES: &[(&str, &[&str])] = &[
    ("youtube.com", &["si", "feature", "pp", "ab_channel"]),
    ("youtu.be", &["si", "feature"]),
    ("spotify.com", &["si", "nd", "context"]),
    ("twitter.com", &["s", "t", "ref_src", "ref_url"]),
    ("x.com", &["s", "t", "ref_src", "ref_url"]),
    ("tiktok.com", &["_r", "_t", "is_from_webapp", "sender_device", "is_copy_url", "share_app_id", "share_link_id", "u_code", "timestamp", "user_id", "tt_from"]),
    ("instagram.com", &["img_index"]),
    ("reddit.com", &["share_id", "ref", "ref_source", "rdt"]),
    ("amazon.", &["ref", "ref_", "pf_rd_*", "pd_rd_*", "_encoding", "psc", "tag", "linkCode", "linkId", "creative", "creativeASIN", "camp", "qid", "sr", "content-id", "th"]),
    ("aliexpress.com", &["spm", "scm", "aff_*", "algo_*", "btsid", "ws_ab_test", "gatewayAdapt", "pdp_*"]),
    ("google.com", &["ved", "ei", "sa", "usg", "sxsrf", "gs_lcp", "oq", "aqs", "sourceid", "client", "bih", "biw", "uact", "sca_esv"]),
    ("bing.com", &["form", "sp", "sc", "qs", "pq", "sk", "cvid"]),
    ("facebook.com", &["__tn__", "__cft__*", "mibextid", "rdid", "share_url"]),
    ("linkedin.com", &["trk", "trkInfo", "lipi", "trackingId", "refId"]),
];

fn matches(rule: &str, key: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => key == rule,
    }
}

/// `host` of an http(s) URL, lowercased, without a `www.` or port.
fn host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").map(str::to_owned).unwrap_or(host))
}

fn site_rules(host: &str) -> impl Iterator<Item = &'static str> + '_ {
    SITES
        .iter()
        .filter(move |(suffix, _)| if suffix.ends_with('.') { host.starts_with(suffix) || host.contains(&format!(".{suffix}")) } else { host == *suffix || host.ends_with(&format!(".{suffix}")) })
        .flat_map(|(_, r)| r.iter().copied())
}

/// One URL without its tracking params (unchanged when it has none or isn't http(s)).
pub fn clean(url: &str) -> Cow<'_, str> {
    let Some(host) = host(url) else { return Cow::Borrowed(url) };
    let Some(q) = url.find('?') else { return Cow::Borrowed(url) };
    let (base, rest) = url.split_at(q);
    let (query, fragment) = match rest[1..].find('#') {
        Some(i) => (&rest[1..=i], &rest[i + 1..]),
        None => (&rest[1..], ""),
    };
    let tracked = |pair: &&str| {
        let key = pair.split('=').next().unwrap_or("");
        GLOBAL.iter().any(|r| matches(r, key)) || site_rules(&host).any(|r| matches(r, key))
    };
    let pairs: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
    if !pairs.iter().any(tracked) {
        return Cow::Borrowed(url);
    }
    let kept: Vec<&str> = pairs.into_iter().filter(|p| !tracked(p)).collect();
    let mut out = base.to_owned();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(fragment);
    Cow::Owned(out)
}

/// Every http(s) link in a message text cleaned (`clean`); everything else unchanged.
pub fn clean_text(text: &str) -> Cow<'_, str> {
    if !text.contains("http") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(i) = rest.find("http") {
        let tail = &rest[i..];
        if !(tail.starts_with("https://") || tail.starts_with("http://")) {
            out.push_str(&rest[..i + 4]);
            rest = &rest[i + 4..];
            continue;
        }
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | ')' | ']' | '|')).unwrap_or(tail.len());
        // Markdown markers and sentence punctuation stuck to the link stay outside it.
        let url = tail[..end].trim_end_matches(['.', ',', ':', ';', '!', '?', '*', '_', '~', '\'']);
        let end = url.len();
        let clean = clean(url);
        changed |= matches!(clean, Cow::Owned(_));
        out.push_str(&rest[..i]);
        out.push_str(&clean);
        rest = &tail[end..];
    }
    out.push_str(rest);
    if changed { Cow::Owned(out) } else { Cow::Borrowed(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking_params_only() {
        assert_eq!(clean("https://example.com/a?utm_source=x&id=5&utm_medium=y#top"), "https://example.com/a?id=5#top");
        assert_eq!(clean("https://example.com/a?fbclid=abc"), "https://example.com/a");
        assert_eq!(clean("https://youtu.be/dQw4w9WgXcQ?si=abc&t=42"), "https://youtu.be/dQw4w9WgXcQ?t=42");
        assert_eq!(clean("https://www.youtube.com/watch?v=x&pp=y&t=10s"), "https://www.youtube.com/watch?v=x&t=10s");
        assert_eq!(clean("https://open.spotify.com/track/1?si=2"), "https://open.spotify.com/track/1");
        assert_eq!(clean("https://www.amazon.de/dp/B0?tag=aff-21&th=1"), "https://www.amazon.de/dp/B0");
        // `t` is tracking on X but a timestamp on YouTube; `si` only on its sites.
        assert_eq!(clean("https://x.com/u/status/1?s=20&t=abc"), "https://x.com/u/status/1");
        assert_eq!(clean("https://example.com/?si=1"), "https://example.com/?si=1");
        // Nothing to do: the same borrowed string.
        assert!(matches!(clean("https://example.com/a?id=1"), Cow::Borrowed(_)));
        assert!(matches!(clean("mailto:a@b.c?utm_source=x"), Cow::Borrowed(_)));
    }

    #[test]
    fn cleans_links_inside_text() {
        let t = "look https://a.com/x?utm_source=1 and <https://b.com/?gclid=2&q=3> (https://c.com/?fbclid=4)";
        assert_eq!(clean_text(t), "look https://a.com/x and <https://b.com/?q=3> (https://c.com/)");
        assert!(matches!(clean_text("no links, just http talk"), Cow::Borrowed(_)));
        assert_eq!(clean_text("httpx https://a.com/?igsh=1"), "httpx https://a.com/");
        // Spoiler / bold markers and a full stop after the link survive.
        assert_eq!(clean_text("||https://youtu.be/x?si=a||"), "||https://youtu.be/x||");
        assert_eq!(clean_text("**https://a.com/?fbclid=1**"), "**https://a.com/**");
        assert_eq!(clean_text("see https://youtu.be/x?si=abc."), "see https://youtu.be/x.");
    }
}
