//! Quest dispatches (docs/QUESTS_NOTES.md §1.3): a quest's user status, the enrolment block and
//! the Orbs balance. Whether a web-identified session receives them for progress made in the
//! desktop app is UNCONFIRMED; the page's fetch on open shows the state either way.
//!
//! OWNERSHIP: quests engineer.

use super::normalize::data;
use crate::model::GatewayEvent;
use crate::quests::{QuestEvent, QuestUserStatus};
use serde::Deserialize;

/// Names handled by `dispatch`.
pub(super) const EVENTS: [&str; 3] = ["QUESTS_USER_STATUS_UPDATE", "QUESTS_USER_COMPLETION_UPDATE", "VIRTUAL_CURRENCY_BALANCE_UPDATE"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wire {
    user_status: Option<QuestUserStatus>,
    quest_enrollment_blocked_until: Option<String>,
    balance: Option<f64>,
}

/// One of `EVENTS` -> its event; nothing when a status has no quest id.
pub(super) fn dispatch(t: &str, b: &[u8]) -> serde_json::Result<Option<GatewayEvent>> {
    let w: Wire = data(b)?;
    let ev = match t {
        "QUESTS_USER_STATUS_UPDATE" => w.user_status.filter(|s| !s.quest_id.is_zero()).map(|s| QuestEvent::Status(Box::new(s))),
        "QUESTS_USER_COMPLETION_UPDATE" => Some(QuestEvent::Blocked(w.quest_enrollment_blocked_until)),
        "VIRTUAL_CURRENCY_BALANCE_UPDATE" => w.balance.map(|n| QuestEvent::Orbs(n.max(0.0) as u64)),
        _ => None,
    };
    Ok(ev.map(GatewayEvent::Quests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Id;

    fn ev(t: &str, d: &str) -> Option<QuestEvent> {
        let raw = format!(r#"{{"op":0,"t":"{t}","s":1,"d":{d}}}"#);
        match dispatch(t, raw.as_bytes()).unwrap() {
            Some(GatewayEvent::Quests(e)) => Some(e),
            None => None,
            Some(other) => panic!("{other:?}"),
        }
    }

    #[test]
    fn quest_dispatches() {
        let status = ev(
            "QUESTS_USER_STATUS_UPDATE",
            r#"{"user_status":{"user_id":"1","quest_id":"101","enrolled_at":"2026-11-10T10:00:00+00:00","completed_at":null,
               "progress":{"WATCH_VIDEO":{"event_name":"WATCH_VIDEO","value":90,"updated_at":"2026-11-10T10:03:00+00:00","completed_at":null}}}}"#,
        );
        match status {
            Some(QuestEvent::Status(s)) => assert_eq!((s.quest_id, s.progress["WATCH_VIDEO"].value), (Id(101), 90.0)),
            other => panic!("{other:?}"),
        }
        assert!(ev("QUESTS_USER_STATUS_UPDATE", r#"{"user_status":null}"#).is_none());
        let blocked = ev("QUESTS_USER_COMPLETION_UPDATE", r#"{"quest_enrollment_blocked_until":"2026-11-20T00:00:00+00:00"}"#);
        assert!(matches!(blocked, Some(QuestEvent::Blocked(Some(_)))));
        assert!(matches!(ev("QUESTS_USER_COMPLETION_UPDATE", r#"{"quest_enrollment_blocked_until":null}"#), Some(QuestEvent::Blocked(None))));
        assert!(matches!(ev("VIRTUAL_CURRENCY_BALANCE_UPDATE", r#"{"balance":1940}"#), Some(QuestEvent::Orbs(1940))));
    }
}
