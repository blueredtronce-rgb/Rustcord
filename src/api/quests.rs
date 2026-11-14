//! Quests (docs/QUESTS_NOTES.md §1): list, Orbs balance, accept, claim, reward code, and video
//! progress while the user plays a quest video in Rustcord. Each call is one user action (or the
//! page open's fetch, or one progress send per 10 s of real play), through the limiter.
//!
//! Never called: `POST /quests/{id}/heartbeat` (play / stream / activity quests). It only accepts
//! the official desktop app's identity, and Rustcord does not pose as it.
//!
//! A 401 on these routes is about the quest, not the login: it never logs the user out.
//!
//! OWNERSHIP: quests engineer.

use super::{ApiClient, ApiError};
use crate::model::{Id, null_default};
use crate::quests::{LOCATION_QUEST_HOME, QuestUserStatus, QuestsPage};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

/// What a quest action was, for its captcha copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Accept,
    Claim,
    Other,
}

/// `POST /quests/{id}/claim-reward` answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClaimAnswer {
    pub claimed_at: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub errors: Vec<ClaimError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClaimError {
    #[serde(deserialize_with = "null_default")]
    pub message: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Balance {
    #[serde(deserialize_with = "null_default")]
    balance: f64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RewardCode {
    #[serde(deserialize_with = "null_default")]
    code: String,
}

impl ApiClient {
    /// `GET /quests/@me`.
    pub async fn quests_me(&self) -> Result<QuestsPage, ApiError> {
        self.quest_call(Method::GET, quest_url(&["@me"]), None, Action::Other).await
    }

    /// `GET /users/@me/virtual-currency/balance`: the Orbs balance.
    pub async fn orbs_balance(&self) -> Result<u64, ApiError> {
        let url = Self::url(&["users", "@me", "virtual-currency", "balance"]);
        let b: Balance = self.quest_call(Method::GET, url, None, Action::Other).await?;
        Ok(b.balance.max(0.0) as u64)
    }

    /// `POST /quests/{id}/enroll` ("Accept quest").
    pub async fn quest_enroll(&self, id: Id, sealed: Option<&str>) -> Result<QuestUserStatus, ApiError> {
        let mut s: QuestUserStatus = self.quest_call(Method::POST, quest_url(&[&id.to_string(), "enroll"]), Some(enroll_body(sealed)), Action::Accept).await?;
        s.quest_id = id;
        Ok(s)
    }

    /// `POST /quests/{id}/video-progress`: seconds of the quest video really played.
    pub async fn quest_video_progress(&self, id: Id, timestamp: u32) -> Result<QuestUserStatus, ApiError> {
        let url = quest_url(&[&id.to_string(), "video-progress"]);
        let mut s: QuestUserStatus = self.quest_call(Method::POST, url, Some(video_body(timestamp)), Action::Other).await?;
        s.quest_id = id;
        Ok(s)
    }

    /// `POST /quests/{id}/claim-reward` ("Claim reward").
    pub async fn quest_claim(&self, id: Id, platform: u8, sealed: Option<&str>) -> Result<ClaimAnswer, ApiError> {
        let url = quest_url(&[&id.to_string(), "claim-reward"]);
        self.quest_call(Method::POST, url, Some(claim_body(platform, sealed)), Action::Claim).await
    }

    /// `GET /quests/{id}/reward-code` ("Show code").
    pub async fn quest_reward_code(&self, id: Id) -> Result<crate::auth::Secret, ApiError> {
        let r: RewardCode = self.quest_call(Method::GET, quest_url(&[&id.to_string(), "reward-code"]), None, Action::Other).await?;
        Ok(crate::auth::Secret::new(r.code))
    }

    /// `send_json` whose 401 neither logs out nor locks the client, and whose captcha answer
    /// says to finish in the official app.
    async fn quest_call<T: serde::de::DeserializeOwned>(&self, method: Method, url: Url, body: Option<Value>, action: Action) -> Result<T, ApiError> {
        // shortcut: the token-rotation guard is what keeps a 401 from arming the logout lock; give
        // ApiClient a per-request "no logout" flag if more routes need this.
        let _no_logout = self.rotation();
        self.send_json(method, url, body).await.map_err(|e| quest_error(e, action))
    }
}

fn quest_url(segments: &[&str]) -> Url {
    let mut url = ApiClient::url(&["quests"]);
    url.path_segments_mut().expect("API_BASE can be a base").extend(segments);
    url
}

fn enroll_body(sealed: Option<&str>) -> Value {
    let mut b = json!({ "location": LOCATION_QUEST_HOME });
    if let Some(s) = sealed {
        b["traffic_metadata_sealed"] = json!(s);
    }
    b
}

fn claim_body(platform: u8, sealed: Option<&str>) -> Value {
    let mut b = json!({ "platform": platform, "location": LOCATION_QUEST_HOME });
    if let Some(s) = sealed {
        b["traffic_metadata_sealed"] = json!(s);
    }
    b
}

fn video_body(timestamp: u32) -> Value {
    json!({ "timestamp": timestamp })
}

/// Quest copy for a captcha answer and for any 401 (QUESTS_NOTES §4.5).
fn quest_error(e: ApiError, action: Action) -> ApiError {
    match e {
        ApiError::Http { status: 400, message } if message == super::CAPTCHA_MSG => {
            let message = match action {
                Action::Accept => "Discord wants a captcha to accept this. Accept it in the official app.",
                Action::Claim => "Discord wants a captcha to claim this. Claim it in the official app.",
                Action::Other => "Discord wants a captcha for this. Finish it in the official app.",
            };
            ApiError::Http { status: 400, message: message.into() }
        }
        ApiError::Http { status: 401, .. } => ApiError::Http { status: 401, message: "Discord didn't accept this from Rustcord. Finish this quest in the official app.".into() },
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_bodies() {
        assert_eq!(quest_url(&["@me"]).as_str(), "https://discord.com/api/v9/quests/@me");
        assert_eq!(quest_url(&["101", "claim-reward"]).as_str(), "https://discord.com/api/v9/quests/101/claim-reward");
        assert_eq!(enroll_body(None), json!({"location": 11}));
        assert_eq!(enroll_body(Some("abc")), json!({"location": 11, "traffic_metadata_sealed": "abc"}));
        assert_eq!(claim_body(2, None), json!({"platform": 2, "location": 11}));
        assert_eq!(video_body(30), json!({"timestamp": 30}));
        let a: ClaimAnswer = serde_json::from_str(r#"{"claimed_at":null,"entitlements":[],"errors":[{"code":1,"message":"Out of codes"}]}"#).unwrap();
        assert_eq!((a.claimed_at, a.errors[0].message.as_str()), (None, "Out of codes"));
        let b: Balance = serde_json::from_str(r#"{"balance":1240}"#).unwrap();
        assert_eq!(b.balance as u64, 1240);
    }

    #[test]
    fn captcha_and_401_become_quest_copy() {
        let captcha = super::super::error_from(400, br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x"}"#);
        assert_eq!(quest_error(captcha.clone(), Action::Claim).to_string(), "Discord wants a captcha to claim this. Claim it in the official app.");
        assert_eq!(quest_error(captcha, Action::Accept).to_string(), "Discord wants a captcha to accept this. Accept it in the official app.");
        // Under the rotation guard a quest 401 arrives as Http 401: never a logout.
        let e = quest_error(ApiError::Http { status: 401, message: "Your login changed".into() }, Action::Other);
        assert!(matches!(e, ApiError::Http { status: 401, .. }) && e.to_string().contains("official app"));
        assert!(matches!(quest_error(ApiError::NotFound, Action::Claim), ApiError::NotFound));
    }
}
