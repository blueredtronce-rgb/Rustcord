//! DM / group DM call REST (VOICE_NOTES §1.4): ringability, ring, stop ringing. Each one is a
//! click (start a call, decline); normal per-route buckets, nothing retried on 4xx.
//!
//! OWNERSHIP: dm-calls engineer.

use super::{ApiClient, ApiError};
use crate::model::Id;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

impl ApiClient {
    /// `GET /channels/{id}/call` -> `{ringable}`: may I ring this (non-friend) 1:1 DM.
    pub async fn call_ringable(&self, channel_id: Id) -> Result<bool, ApiError> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Ringable {
            ringable: bool,
        }
        let url = Self::url(&["channels", &channel_id.to_string(), "call"]);
        Ok(self.send_json::<Ringable>(Method::GET, url, None).await?.ringable)
    }

    /// `POST /channels/{id}/call/ring {"recipients": null | [ids]}` (null = everyone).
    pub async fn ring_call(&self, channel_id: Id, recipients: Option<&[Id]>) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "call", "ring"]);
        self.send(Method::POST, url, Some(json!({ "recipients": recipients }))).await.map(drop)
    }

    /// `POST /channels/{id}/call/stop-ringing`: `{"recipients": [ids]}`, or `{}` = stop ringing me
    /// (decline).
    pub async fn stop_ringing(&self, channel_id: Id, recipients: Option<&[Id]>) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &channel_id.to_string(), "call", "stop-ringing"]);
        self.send(Method::POST, url, Some(stop_body(recipients))).await.map(drop)
    }
}

fn stop_body(recipients: Option<&[Id]>) -> Value {
    match recipients {
        Some(r) => json!({ "recipients": r }),
        None => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies() {
        assert_eq!(json!({ "recipients": None::<&[Id]> }), json!({"recipients": null}));
        assert_eq!(json!({ "recipients": Some(&[Id(5)][..]) }), json!({"recipients": ["5"]}));
        assert_eq!(stop_body(None), json!({}));
        assert_eq!(stop_body(Some(&[Id(5)])), json!({"recipients": ["5"]}));
    }
}
