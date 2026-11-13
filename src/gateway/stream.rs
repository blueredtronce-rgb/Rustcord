//! Go Live on the main gateway (VOICE_NOTES §5.4): ops 18/19/20/22 and the STREAM_CREATE /
//! STREAM_SERVER_UPDATE / STREAM_DELETE dispatches. The dispatches go to the voice module only
//! (the server update carries a token); conn.rs routes them.
//!
//! OWNERSHIP: contract owner of docs/VIDEO_DESIGN.md.

use super::StreamOp;
use super::normalize::data;
use crate::model::{GatewayEvent, Id, null_default};
use serde::Deserialize;
use serde_json::json;

/// Names handled by `dispatch`.
pub(super) const EVENTS: [&str; 3] = ["STREAM_CREATE", "STREAM_SERVER_UPDATE", "STREAM_DELETE"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wire {
    #[serde(deserialize_with = "null_default")]
    stream_key: String,
    rtc_server_id: Id,
    rtc_channel_id: Id,
    #[serde(deserialize_with = "null_default")]
    viewer_ids: Vec<Id>,
    #[serde(deserialize_with = "null_default")]
    paused: bool,
    endpoint: Option<String>,
    #[serde(deserialize_with = "null_default")]
    token: String,
    reason: Option<String>,
    #[serde(deserialize_with = "null_default")]
    unavailable: bool,
}

/// One of `EVENTS` -> its event; nothing without a stream key (or a STREAM_CREATE without its
/// server).
pub(super) fn dispatch(t: &str, b: &[u8]) -> serde_json::Result<Option<GatewayEvent>> {
    use GatewayEvent as E;
    let w: Wire = data(b)?;
    if w.stream_key.is_empty() {
        return Ok(None);
    }
    Ok(match t {
        "STREAM_CREATE" if !w.rtc_server_id.is_zero() => Some(E::StreamCreate {
            stream_key: w.stream_key,
            rtc_server_id: w.rtc_server_id,
            rtc_channel_id: Some(w.rtc_channel_id).filter(|c| !c.is_zero()),
            viewer_ids: w.viewer_ids,
            paused: w.paused,
        }),
        "STREAM_SERVER_UPDATE" => Some(E::StreamServerUpdate {
            stream_key: w.stream_key,
            endpoint: w.endpoint.filter(|e| !e.is_empty()),
            token: crate::auth::Secret::new(w.token),
        }),
        "STREAM_DELETE" => Some(E::StreamDelete { stream_key: w.stream_key, reason: w.reason, unavailable: w.unavailable }),
        _ => None,
    })
}

/// The op for a `GatewayCommand::Stream`.
pub(super) fn payload(op: &StreamOp) -> String {
    match op {
        StreamOp::Create { guild_id, channel_id } => {
            let kind = if guild_id.is_some() { "guild" } else { "call" };
            json!({"op": 18, "d": {"type": kind, "guild_id": guild_id, "channel_id": channel_id}})
        }
        StreamOp::Delete { stream_key } => json!({"op": 19, "d": {"stream_key": stream_key}}),
        StreamOp::Watch { stream_key } => json!({"op": 20, "d": {"stream_key": stream_key}}),
        StreamOp::SetPaused { stream_key, paused } => json!({"op": 22, "d": {"stream_key": stream_key, "paused": paused}}),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn ev(t: &str, d: &str) -> Option<GatewayEvent> {
        let raw = format!(r#"{{"op":0,"t":"{t}","s":1,"d":{d}}}"#);
        dispatch(t, raw.as_bytes()).unwrap()
    }

    #[test]
    fn stream_dispatches() {
        let create = ev(
            "STREAM_CREATE",
            r#"{"stream_key":"guild:1:2:3","rtc_server_id":"77","rtc_channel_id":"78","region":"x","viewer_ids":["5"],"paused":false}"#,
        );
        let Some(GatewayEvent::StreamCreate { stream_key, rtc_server_id, rtc_channel_id, viewer_ids, paused }) = create else { panic!("{create:?}") };
        assert_eq!((stream_key.as_str(), rtc_server_id, rtc_channel_id, viewer_ids, paused), ("guild:1:2:3", Id(77), Some(Id(78)), vec![Id(5)], false));
        let bare = ev("STREAM_CREATE", r#"{"stream_key":"call:2:3","rtc_server_id":"77","viewer_ids":null,"paused":null}"#);
        assert!(matches!(bare, Some(GatewayEvent::StreamCreate { rtc_channel_id: None, paused: false, .. })));
        assert!(ev("STREAM_CREATE", r#"{"stream_key":"call:2:3"}"#).is_none(), "no server");

        let server = ev("STREAM_SERVER_UPDATE", r#"{"stream_key":"call:2:3","endpoint":"host:443","token":"s3cret","guild_id":null}"#);
        let Some(GatewayEvent::StreamServerUpdate { stream_key, endpoint, token }) = &server else { panic!() };
        assert_eq!((stream_key.as_str(), endpoint.as_deref(), token.expose()), ("call:2:3", Some("host:443"), "s3cret"));
        assert!(!format!("{server:?}").contains("s3cret"), "token redacted in Debug");
        assert!(matches!(ev("STREAM_SERVER_UPDATE", r#"{"stream_key":"k","endpoint":null,"token":"t"}"#), Some(GatewayEvent::StreamServerUpdate { endpoint: None, .. })));

        let del = ev("STREAM_DELETE", r#"{"stream_key":"call:2:3","reason":"stream_full","unavailable":true}"#);
        assert!(matches!(del, Some(GatewayEvent::StreamDelete { reason: Some(r), unavailable: true, .. }) if r == "stream_full"));
        assert!(ev("STREAM_DELETE", r#"{"reason":"stream_ended"}"#).is_none(), "no key");
    }

    #[test]
    fn stream_ops() {
        let v = |op: StreamOp| serde_json::from_str::<Value>(&payload(&op)).unwrap();
        assert_eq!(
            v(StreamOp::Create { guild_id: Some(Id(1)), channel_id: Id(2) }),
            json!({"op":18,"d":{"type":"guild","guild_id":"1","channel_id":"2"}})
        );
        assert_eq!(
            v(StreamOp::Create { guild_id: None, channel_id: Id(2) }),
            json!({"op":18,"d":{"type":"call","guild_id":null,"channel_id":"2"}})
        );
        assert_eq!(v(StreamOp::Delete { stream_key: "call:2:3".into() }), json!({"op":19,"d":{"stream_key":"call:2:3"}}));
        assert_eq!(v(StreamOp::Watch { stream_key: "call:2:3".into() }), json!({"op":20,"d":{"stream_key":"call:2:3"}}));
        assert_eq!(
            v(StreamOp::SetPaused { stream_key: "call:2:3".into(), paused: true }),
            json!({"op":22,"d":{"stream_key":"call:2:3","paused":true}})
        );
    }
}
