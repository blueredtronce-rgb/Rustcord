//! Dispatches of the account & server area (FEATURES_NOTES §3.1, §3.6): USER_SETTINGS_PROTO_UPDATE
//! and GUILD_BAN_ADD / _REMOVE, plus READY's `user_settings_proto` blob. A settings blob that
//! does not decode is logged and skipped, never fatal.
//!
//! OWNERSHIP: settings engineer (docs/ACCOUNT_SERVER_DESIGN.md).

use super::normalize::data;
use crate::api::settings_proto;
use crate::model::{GatewayEvent, Id, User, UserSettingsProto, null_default};
use serde::Deserialize;

/// Names handled by `dispatch`.
pub(super) const EVENTS: [&str; 3] = ["USER_SETTINGS_PROTO_UPDATE", "GUILD_BAN_ADD", "GUILD_BAN_REMOVE"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wire {
    settings: Option<ProtoWire>,
    #[serde(deserialize_with = "null_default")]
    partial: bool,
    guild_id: Id,
    user: Option<User>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProtoWire {
    /// 1 = `PreloadedUserSettings` (2 = frecency, 3 = test settings: not used).
    #[serde(rename = "type", deserialize_with = "null_default")]
    kind: u8,
    #[serde(deserialize_with = "null_default")]
    proto: String,
}

/// One of `EVENTS` -> its event; nothing for other settings types, bad blobs or missing ids.
pub(super) fn dispatch(t: &str, b: &[u8]) -> serde_json::Result<Option<GatewayEvent>> {
    use GatewayEvent as E;
    let w: Wire = data(b)?;
    let user = w.user.filter(|u| !u.id.is_zero() && !w.guild_id.is_zero());
    Ok(match t {
        "USER_SETTINGS_PROTO_UPDATE" => (w.settings.filter(|s| s.kind == 1))
            .and_then(|s| settings(&s.proto))
            .map(|settings| E::UserSettingsProto { settings, partial: w.partial }),
        "GUILD_BAN_ADD" => user.map(|user| E::GuildBanAdd { guild_id: w.guild_id, user }),
        "GUILD_BAN_REMOVE" => user.map(|u| E::GuildBanRemove { guild_id: w.guild_id, user_id: u.id }),
        _ => None,
    })
}

/// A base64 `PreloadedUserSettings`, decoded; a bad one is logged and skipped.
pub(super) fn settings(b64: &str) -> Option<UserSettingsProto> {
    settings_proto::decode(b64).inspect_err(|e| eprintln!("gateway: skipped the settings proto: {e}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserStatus;

    fn ev(t: &str, d: &str) -> Option<GatewayEvent> {
        let raw = format!(r#"{{"op":0,"t":"{t}","s":1,"d":{d}}}"#);
        dispatch(t, raw.as_bytes()).unwrap()
    }

    #[test]
    fn settings_proto_update() {
        let up = ev("USER_SETTINGS_PROTO_UPDATE", r#"{"settings":{"type":1,"proto":"WgcKBQoDZG5k"},"partial":true}"#);
        match up {
            Some(GatewayEvent::UserSettingsProto { settings, partial: true }) => {
                assert_eq!(settings.status.unwrap().status, Some(UserStatus::Dnd));
                assert_eq!(settings.guild_folders, None);
            }
            other => panic!("{other:?}"),
        }
        let full = ev("USER_SETTINGS_PROTO_UPDATE", r#"{"settings":{"type":1,"proto":""},"partial":false}"#);
        assert!(matches!(full, Some(GatewayEvent::UserSettingsProto { partial: false, .. })));
        assert!(ev("USER_SETTINGS_PROTO_UPDATE", r#"{"settings":{"type":2,"proto":"WgcKBQoDZG5k"},"partial":true}"#).is_none());
        assert!(ev("USER_SETTINGS_PROTO_UPDATE", r#"{"settings":{"type":1,"proto":"Wgc="},"partial":true}"#).is_none(), "bad blob skipped");
    }

    #[test]
    fn ready_settings_proto_is_optional_and_never_fatal() {
        let ready = |extra: &str| {
            let raw = format!(r#"{{"op":0,"t":"READY","d":{{"user":{{"id":"1"}},"session_id":"s"{extra}}}}}"#);
            super::super::normalize::ready(raw.as_bytes()).unwrap().settings
        };
        assert_eq!(ready(r#","user_settings_proto":"WgcKBQoDZG5k""#).unwrap().status.unwrap().status, Some(UserStatus::Dnd));
        assert_eq!(ready(r#","user_settings_proto":"!!""#), None);
        assert_eq!(ready(r#","user_settings_proto":null"#), None);
        assert_eq!(ready(""), None);
    }

    #[test]
    fn ban_dispatches() {
        let add = ev("GUILD_BAN_ADD", r#"{"guild_id":"5","user":{"id":"7","username":"spam"}}"#);
        assert!(matches!(add, Some(GatewayEvent::GuildBanAdd { guild_id: Id(5), user }) if user.username == "spam"));
        let rm = ev("GUILD_BAN_REMOVE", r#"{"guild_id":"5","user":{"id":"7"}}"#);
        assert!(matches!(rm, Some(GatewayEvent::GuildBanRemove { guild_id: Id(5), user_id: Id(7) })));
        assert!(ev("GUILD_BAN_ADD", r#"{"guild_id":"5","user":null}"#).is_none());
        assert!(ev("GUILD_BAN_REMOVE", r#"{"user":{"id":"7"}}"#).is_none());
    }
}
