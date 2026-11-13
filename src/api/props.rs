//! Client identity sent in gateway IDENTIFY `properties` and the REST `X-Super-Properties`
//! header (same values in both places, like the official web client).
//!
//! OWNERSHIP: api implementer.

use base64::Engine as _;
use serde::Serialize;

/// Chrome stable major version for the user agent (Chrome 157 shipped 2026-11-03).
/// Chrome's reduced UA always reports `<major>.0.0.0`.
/// shortcut: hard-coded, bump it every few months so the UA does not look stale.
const CHROME_MAJOR: u32 = 157;

#[derive(Debug, Clone, Serialize)]
pub struct ClientProps {
    pub os: String,
    pub browser: String,
    pub device: String,
    pub system_locale: String,
    pub browser_user_agent: String,
    pub browser_version: String,
    pub os_version: String,
    pub referrer: String,
    pub referring_domain: String,
    pub referrer_current: String,
    pub referring_domain_current: String,
    pub release_channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_build_number: Option<u64>,
    pub client_event_source: Option<String>,
}

/// (os, os_version, UA platform token) as desktop Chrome reports them.
fn os_identity() -> (&'static str, &'static str, &'static str) {
    if cfg!(target_os = "windows") {
        // Chrome reports "Windows NT 10.0" on Windows 10 and 11 alike.
        ("Windows", "10", "Windows NT 10.0; Win64; x64")
    } else if cfg!(target_os = "macos") {
        // Chrome freezes the macOS version in the UA.
        ("Mac OS X", "10.15.7", "Macintosh; Intel Mac OS X 10_15_7")
    } else {
        ("Linux", "", "X11; Linux x86_64")
    }
}

impl ClientProps {
    /// Build from OS info + settings overrides.
    pub fn new(settings: &crate::config::Settings) -> Self {
        let (os, os_version, platform) = os_identity();
        let browser_version = format!("{CHROME_MAJOR}.0.0.0");
        let browser_user_agent = format!(
            "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{browser_version} Safari/537.36"
        );
        let system_locale = settings
            .system_locale
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("en-US")
            .to_owned();
        Self {
            os: os.into(),
            browser: "Chrome".into(),
            device: String::new(),
            system_locale,
            browser_user_agent,
            browser_version,
            os_version: os_version.into(),
            referrer: String::new(),
            referring_domain: String::new(),
            referrer_current: String::new(),
            referring_domain_current: String::new(),
            release_channel: "stable".into(),
            client_build_number: settings.client_build_number,
            client_event_source: None,
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.browser_user_agent
    }

    /// Accept-Language a browser with this locale sends ("de-DE" -> "de-DE,de;q=0.9").
    pub fn accept_language(&self) -> String {
        let locale = self.system_locale.as_str();
        let lang = locale.split('-').next().unwrap_or(locale);
        if lang != locale { format!("{locale},{lang};q=0.9") } else { locale.to_owned() }
    }

    /// base64(JSON) for the X-Super-Properties header.
    pub fn super_properties_header(&self) -> String {
        // serde_json::to_string is compact (no spaces), matching the web client.
        let json = serde_json::to_string(self).unwrap_or_default();
        base64::engine::general_purpose::STANDARD.encode(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    #[test]
    fn header_decodes_to_same_json() {
        let props = ClientProps::new(&Settings::default());
        let decoded = base64::engine::general_purpose::STANDARD.decode(props.super_properties_header()).unwrap();
        let json = String::from_utf8(decoded).unwrap();
        assert_eq!(json, serde_json::to_string(&props).unwrap());
        assert!(!json.contains(": "), "must be compact JSON");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["browser"], "Chrome");
        assert_eq!(v["release_channel"], "stable");
        assert_eq!(v["system_locale"], "en-US");
        assert!(v["client_event_source"].is_null());
        assert!(v.get("client_build_number").is_none());
    }

    #[test]
    fn ua_matches_version_and_overrides_apply() {
        let settings = Settings {
            client_build_number: Some(123_456),
            system_locale: Some("de".into()),
            ..Settings::default()
        };
        let props = ClientProps::new(&settings);
        assert_eq!(props.user_agent(), props.browser_user_agent);
        assert!(props.browser_user_agent.contains(&format!("Chrome/{} ", props.browser_version)));
        assert_eq!(props.system_locale, "de");
        let v: serde_json::Value = serde_json::to_value(&props).unwrap();
        assert_eq!(v["client_build_number"], 123_456);
    }
}
