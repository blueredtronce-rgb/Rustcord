//! Game detection and activity icons (crate::activity): the detectable-games list and public
//! application metadata (icons), both GETs the official client makes too, through the limiter.
//!
//! OWNERSHIP: activities engineer.

use super::{ApiClient, ApiError, net_err};
use crate::model::Id;
use reqwest::{Method, Url};
use serde::Deserialize;

/// Application ids per `/applications/public` request.
// shortcut: 25 like `/games`; the server's own limit is unconfirmed.
pub const MAX_APP_IDS: usize = 25;

/// `/applications/public` entry; only the icon is used.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PublicApp {
    pub id: Id,
    pub icon: Option<String>,
}

impl ApiClient {
    /// `GET /games/detectable` (the official client's detection list, ~13 MB of JSON): the raw
    /// body, parsed off the async workers by the caller.
    pub async fn detectable_games(&self) -> Result<Vec<u8>, ApiError> {
        let resp = self.send(Method::GET, Self::url(&["games", "detectable"]), None).await?;
        Ok(resp.bytes().await.map_err(net_err)?.to_vec())
    }

    /// `GET /applications/public?application_ids=<id>&…` (1-25 ids); unknown ids are left out.
    pub async fn public_apps(&self, ids: &[Id]) -> Result<Vec<PublicApp>, ApiError> {
        self.send_json(Method::GET, public_apps_url(ids), None).await
    }
}

fn public_apps_url(ids: &[Id]) -> Url {
    let mut url = ApiClient::url(&["applications", "public"]);
    url.query_pairs_mut().extend_pairs(ids.iter().take(MAX_APP_IDS).map(|id| ("application_ids", id.to_string())));
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let u = public_apps_url(&(1..=30).map(Id).collect::<Vec<_>>());
        assert!(u.as_str().contains("/applications/public?application_ids=1&application_ids=2&"), "{u}");
        assert!(u.as_str().ends_with("&application_ids=25"), "{u}");
        let apps: Vec<PublicApp> = serde_json::from_str(r#"[{"id":"7","name":"x","icon":"abc"},{"id":"8","icon":null}]"#).unwrap();
        assert_eq!((apps[0].id, apps[0].icon.as_deref(), apps[1].icon.as_deref()), (Id(7), Some("abc"), None));
    }
}
