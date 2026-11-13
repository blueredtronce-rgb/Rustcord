//! DM / group DM calls (VOICE_NOTES §1.4): CALL_CREATE / CALL_UPDATE / CALL_DELETE. With
//! capability AUTO_CALL_CONNECT (conn.rs) the server sends a CALL_CREATE for every call already
//! going on right after READY, so no op 13 per DM is needed.
//!
//! OWNERSHIP: dm-calls engineer.

use super::normalize::data;
use crate::model::{GatewayEvent, Id, VoiceState, null_default};
use serde::Deserialize;

/// Names handled by `dispatch`.
pub(super) const EVENTS: [&str; 3] = ["CALL_CREATE", "CALL_UPDATE", "CALL_DELETE"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wire {
    channel_id: Id,
    #[serde(deserialize_with = "null_default")]
    ringing: Vec<Id>,
    /// CALL_CREATE only: who is in the call (no `guild_id`).
    #[serde(deserialize_with = "null_default")]
    voice_states: Vec<VoiceState>,
}

/// One of `EVENTS` -> CALL_CREATE: its voice states, then `CallUpdate`; nothing without a channel.
pub(super) fn dispatch(t: &str, b: &[u8]) -> serde_json::Result<Vec<GatewayEvent>> {
    let w: Wire = data(b)?;
    if w.channel_id.is_zero() {
        return Ok(Vec::new());
    }
    let channel_id = w.channel_id;
    if t == "CALL_DELETE" {
        return Ok(vec![GatewayEvent::CallDelete { channel_id }]);
    }
    let mut out: Vec<GatewayEvent> = w
        .voice_states
        .into_iter()
        .map(|v| GatewayEvent::VoiceStateUpdate(Box::new(VoiceState { guild_id: None, channel_id: v.channel_id.or(Some(channel_id)), ..v })))
        .collect();
    out.push(GatewayEvent::CallUpdate { channel_id, ringing: w.ringing });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str, d: &str) -> Vec<GatewayEvent> {
        let raw = format!(r#"{{"op":0,"t":"{t}","s":1,"d":{d}}}"#);
        super::super::normalize::dispatch(t, raw.as_bytes(), Id(1)).unwrap()
    }

    #[test]
    fn call_dispatches() {
        let create = ev(
            "CALL_CREATE",
            r#"{"channel_id":"5","message_id":"9","region":"rotterdam","ringing":["1","3"],"embedded_activities":[],
               "voice_states":[{"user_id":"2","channel_id":"5","session_id":"s","self_mute":true,"guild_id":null}]}"#,
        );
        let [GatewayEvent::VoiceStateUpdate(v), GatewayEvent::CallUpdate { channel_id, ringing }] = &create[..] else { panic!("{create:?}") };
        assert_eq!((v.user_id, v.channel_id, v.guild_id, v.self_mute), (Id(2), Some(Id(5)), None, true));
        assert_eq!((*channel_id, ringing.as_slice()), (Id(5), [Id(1), Id(3)].as_slice()));

        let update = ev("CALL_UPDATE", r#"{"channel_id":"5","message_id":"9","region":"x","ringing":[],"guild_id":null}"#);
        assert!(matches!(&update[..], [GatewayEvent::CallUpdate { channel_id: Id(5), ringing }] if ringing.is_empty()));
        let nulls = ev("CALL_UPDATE", r#"{"channel_id":"5","ringing":null,"voice_states":null}"#);
        assert!(matches!(&nulls[..], [GatewayEvent::CallUpdate { .. }]));

        assert!(matches!(&ev("CALL_DELETE", r#"{"channel_id":"5","unavailable":true}"#)[..], [GatewayEvent::CallDelete { channel_id: Id(5) }]));
        assert!(ev("CALL_DELETE", r#"{"unavailable":true}"#).is_empty(), "no channel");
    }
}
