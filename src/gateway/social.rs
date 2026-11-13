//! Dispatches of the friends/DMs/inbox area (FEATURES_NOTES §2.4-§2.6): CHANNEL_RECIPIENT_ADD /
//! _REMOVE, RECENT_MENTION_DELETE, USER_NOTE_UPDATE. RELATIONSHIP_* stay in normalize.rs.
//!
//! OWNERSHIP: friends/DMs/inbox engineer (docs/SOCIAL_DESIGN.md).

use super::normalize::data;
use crate::model::{GatewayEvent, Id, User, null_default};
use serde::Deserialize;

/// Names handled by `people`.
pub(super) const PEOPLE_EVENTS: [&str; 4] =
    ["CHANNEL_RECIPIENT_ADD", "CHANNEL_RECIPIENT_REMOVE", "RECENT_MENTION_DELETE", "USER_NOTE_UPDATE"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct PeopleWire {
    /// USER_NOTE_UPDATE: the user the note is about.
    id: Id,
    channel_id: Id,
    message_id: Id,
    user: Option<User>,
    #[serde(deserialize_with = "null_default")]
    note: String,
}

/// One of `PEOPLE_EVENTS` -> its event; nothing when the payload lacks the ids it is about.
pub(super) fn people(t: &str, b: &[u8]) -> serde_json::Result<Option<GatewayEvent>> {
    use GatewayEvent as E;
    let w: PeopleWire = data(b)?;
    let user = w.user.filter(|u| !u.id.is_zero() && !w.channel_id.is_zero());
    Ok(match t {
        "CHANNEL_RECIPIENT_ADD" => user.map(|user| E::ChannelRecipientAdd { channel_id: w.channel_id, user }),
        "CHANNEL_RECIPIENT_REMOVE" => user.map(|u| E::ChannelRecipientRemove { channel_id: w.channel_id, user_id: u.id }),
        "RECENT_MENTION_DELETE" => (!w.message_id.is_zero()).then_some(E::RecentMentionDelete { message_id: w.message_id }),
        "USER_NOTE_UPDATE" => (!w.id.is_zero()).then_some(E::UserNoteUpdate { user_id: w.id, note: w.note }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: &str, d: &str) -> Option<GatewayEvent> {
        let raw = format!(r#"{{"op":0,"t":"{t}","s":1,"d":{d}}}"#);
        people(t, raw.as_bytes()).unwrap()
    }

    #[test]
    fn people_dispatches() {
        let add = ev("CHANNEL_RECIPIENT_ADD", r#"{"channel_id":"20","user":{"id":"2","username":"zed"},"nick":null}"#);
        assert!(matches!(add, Some(GatewayEvent::ChannelRecipientAdd { channel_id: Id(20), user }) if user.username == "zed"));
        let rm = ev("CHANNEL_RECIPIENT_REMOVE", r#"{"channel_id":"20","user":{"id":"1","username":"me"}}"#);
        assert!(matches!(rm, Some(GatewayEvent::ChannelRecipientRemove { channel_id: Id(20), user_id: Id(1) })));
        assert!(ev("CHANNEL_RECIPIENT_REMOVE", r#"{"channel_id":"20","user":null}"#).is_none());
        let del = ev("RECENT_MENTION_DELETE", r#"{"message_id":"99"}"#);
        assert!(matches!(del, Some(GatewayEvent::RecentMentionDelete { message_id: Id(99) })));
        let note = ev("USER_NOTE_UPDATE", r#"{"id":"2","note":"likes cats"}"#);
        assert!(matches!(note, Some(GatewayEvent::UserNoteUpdate { user_id: Id(2), note }) if note == "likes cats"));
        let cleared = ev("USER_NOTE_UPDATE", r#"{"id":"2","note":null}"#);
        assert!(matches!(cleared, Some(GatewayEvent::UserNoteUpdate { note, .. }) if note.is_empty()));
        assert!(ev("SOMETHING_ELSE", "{}").is_none());
    }
}
